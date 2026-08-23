// SPDX-License-Identifier: BSD-2-Clause

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;
use toml_edit::{DocumentMut, Item};

use crate::model::ServiceId;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum EditError {
    #[error("manager TOML is invalid: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("manager TOML is missing string default_group")]
    MissingDefaultGroup,
    #[error("manager TOML is missing [groups.{0}].wants array")]
    MissingWants(String),
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// Adds or removes one service from the default group's wants array while
/// retaining comments and unrelated formatting.
///
/// # Errors
///
/// Returns an error when the document lacks the already-validated group shape
/// or cannot be parsed by `toml_edit`.
pub fn edit_enabled(source: &str, service: &ServiceId, enabled: bool) -> Result<String, EditError> {
    let mut document = source.parse::<DocumentMut>()?;
    let group = document
        .get("default_group")
        .and_then(Item::as_str)
        .ok_or(EditError::MissingDefaultGroup)?
        .to_owned();
    let wants = document
        .get_mut("groups")
        .and_then(Item::as_table_like_mut)
        .and_then(|groups| groups.get_mut(&group))
        .and_then(Item::as_table_like_mut)
        .and_then(|group| group.get_mut("wants"))
        .and_then(Item::as_array_mut)
        .ok_or_else(|| EditError::MissingWants(group.clone()))?;

    let existing = wants
        .iter()
        .position(|value| value.as_str() == Some(service.as_str()));
    match (enabled, existing) {
        (true, None) => wants.push(service.as_str()),
        (false, Some(index)) => {
            wants.remove(index);
        }
        _ => {}
    }
    Ok(document.to_string())
}

/// Durably replaces a manager-owned TOML file using a same-directory rename.
///
/// # Errors
///
/// Returns an error from directory creation, temporary-file IO, fsync, chmod,
/// rename, or parent-directory fsync.
pub fn atomic_write(path: &Path, contents: &str, mode: u32) -> Result<(), EditError> {
    let parent = path.parent().ok_or_else(|| EditError::Write {
        path: path.display().to_string(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| write_error(path, source))?;
    let temporary = parent.join(format!(
        ".loom.toml.{}.{}.tmp",
        process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|source| write_error(path, source))
}

fn write_error(path: &Path, source: io::Error) -> EditError {
    EditError::Write {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"# keep this comment
schema_version = 1
default_group = "boot"

[groups.boot]
# and this one
wants = ["db"]
"#;

    #[test]
    fn edits_membership_without_losing_comments() {
        let service = ServiceId::new("web").unwrap();
        let enabled = edit_enabled(SOURCE, &service, true).unwrap();
        assert!(enabled.contains("# keep this comment"));
        assert!(enabled.contains("# and this one"));
        assert!(enabled.contains("\"web\""));

        let disabled = edit_enabled(&enabled, &service, false).unwrap();
        assert!(!disabled.contains("\"web\""));
        assert!(disabled.contains("\"db\""));
    }

    #[test]
    fn repeated_edit_is_idempotent() {
        let service = ServiceId::new("db").unwrap();
        assert_eq!(edit_enabled(SOURCE, &service, true).unwrap(), SOURCE);
    }
}
