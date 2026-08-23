// SPDX-License-Identifier: BSD-2-Clause

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{fs::MetadataExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    config::ConfigSnapshot,
    model::{ConfigError, ManagerScope, ServiceId},
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const DEFAULT_USER_MANAGER: &str = "schema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\n";

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("failed to inspect {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("missing manager configuration {0}")]
    MissingManager(PathBuf),
    #[error("unsafe configuration path {path}: {reason}")]
    Unsafe { path: PathBuf, reason: String },
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("configuration {0} is not UTF-8")]
    NotUtf8(PathBuf),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

pub struct ConfigLoader;

impl ConfigLoader {
    /// Loads system configuration beneath `root` with whole-file layering.
    ///
    /// # Errors
    ///
    /// Returns an error for missing/unsafe files, IO failure, invalid UTF-8, or
    /// any schema/graph validation failure.
    pub fn load_system(root: &Path) -> Result<ConfigSnapshot, LoadError> {
        let manager = under(root, "/etc/loom/loom.toml");
        let layers = [
            (under(root, "/usr/lib/loom/services"), 0),
            (under(root, "/etc/loom/services"), 0),
            (under(root, "/run/loom/services"), 0),
        ];
        Self::load(&manager, None, &layers, ManagerScope::System, 0)
    }

    /// Loads one user's configuration from validated XDG roots.
    ///
    /// Missing user manager configuration produces an empty in-memory boot group
    /// and never writes to the user's home directory.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe roots/files, IO failure, invalid UTF-8, or an
    /// invalid snapshot.
    pub fn load_user(
        system_root: &Path,
        config_home: &Path,
        runtime_dir: &Path,
        uid: u32,
    ) -> Result<ConfigSnapshot, LoadError> {
        validate_directory(runtime_dir, uid)?;
        let manager = config_home.join("loom/loom.toml");
        let layers = [
            (under(system_root, "/usr/lib/loom/user/services"), 0),
            (config_home.join("loom/services"), uid),
            (runtime_dir.join("loom/services"), uid),
        ];
        Self::load(
            &manager,
            Some(DEFAULT_USER_MANAGER),
            &layers,
            ManagerScope::User,
            uid,
        )
    }

    fn load(
        manager_path: &Path,
        missing_manager_default: Option<&str>,
        layers: &[(PathBuf, u32)],
        scope: ManagerScope,
        manager_uid: u32,
    ) -> Result<ConfigSnapshot, LoadError> {
        let manager = match read_secure_file(manager_path, manager_uid) {
            Ok(source) => source,
            Err(LoadError::Inspect { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                missing_manager_default
                    .map(str::to_owned)
                    .ok_or_else(|| LoadError::MissingManager(manager_path.to_path_buf()))?
            }
            Err(error) => return Err(error),
        };

        let mut sources = BTreeMap::<ServiceId, String>::new();
        for (directory, uid) in layers {
            if !directory.exists() {
                continue;
            }
            validate_directory(directory, *uid)?;
            let entries = fs::read_dir(directory).map_err(|source| LoadError::Read {
                path: directory.clone(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| LoadError::Read {
                    path: directory.clone(),
                    source,
                })?;
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                    continue;
                }
                let id = ServiceId::from_path(&path)?;
                let source = read_secure_file(&path, *uid)?;
                sources.insert(id, source);
            }
        }
        ConfigSnapshot::build(
            &manager,
            sources
                .iter()
                .map(|(id, source)| (id.clone(), source.as_str())),
            scope,
        )
        .map_err(LoadError::from)
    }
}

fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

fn validate_directory(path: &Path, uid: u32) -> Result<(), LoadError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LoadError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_dir() {
        return Err(LoadError::Unsafe {
            path: path.to_path_buf(),
            reason: "must be a real directory, not a symlink".into(),
        });
    }
    validate_metadata(path, &metadata, uid)
}

fn read_secure_file(path: &Path, uid: u32) -> Result<String, LoadError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path).map_err(|source| LoadError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| LoadError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(LoadError::Unsafe {
            path: path.to_path_buf(),
            reason: "must be a regular file".into(),
        });
    }
    validate_metadata(path, &metadata, uid)?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(LoadError::Unsafe {
            path: path.to_path_buf(),
            reason: format!("exceeds {MAX_CONFIG_BYTES} byte limit"),
        });
    }
    let capacity = usize::try_from(metadata.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(LoadError::Unsafe {
            path: path.to_path_buf(),
            reason: format!("exceeds {MAX_CONFIG_BYTES} byte limit"),
        });
    }
    String::from_utf8(bytes).map_err(|_| LoadError::NotUtf8(path.to_path_buf()))
}

fn validate_metadata(path: &Path, metadata: &fs::Metadata, uid: u32) -> Result<(), LoadError> {
    if metadata.uid() != uid {
        return Err(LoadError::Unsafe {
            path: path.to_path_buf(),
            reason: format!("owner UID is {}, expected {uid}", metadata.uid()),
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(LoadError::Unsafe {
            path: path.to_path_buf(),
            reason: "must not be writable by group or other".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "loom-loader-{}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manager() -> &'static str {
        "schema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\nwants = [\"svc\"]\n"
    }

    fn service(command: &str) -> String {
        format!("schema_version = 1\n[process]\ncommand = [\"{command}\"]\n")
    }

    #[test]
    fn higher_layer_replaces_whole_service_file() {
        let root = TestRoot::new();
        let uid = fs::metadata(&root.0).unwrap().uid();
        let manager_path = root.write("etc/loom/loom.toml", manager());
        root.write(
            "usr/lib/loom/services/svc.toml",
            &service("/usr/bin/vendor"),
        );
        root.write("etc/loom/services/svc.toml", &service("/usr/bin/admin"));
        let layers = [
            (root.0.join("usr/lib/loom/services"), uid),
            (root.0.join("etc/loom/services"), uid),
        ];

        let snapshot =
            ConfigLoader::load(&manager_path, None, &layers, ManagerScope::System, uid).unwrap();
        assert_eq!(
            snapshot.services()[&ServiceId::new("svc").unwrap()]
                .process
                .command[0],
            "/usr/bin/admin"
        );
    }

    #[test]
    fn rejects_group_writable_and_symlinked_files() {
        let root = TestRoot::new();
        let uid = fs::metadata(&root.0).unwrap().uid();
        let manager_path = root.write("loom.toml", manager());
        fs::set_permissions(&manager_path, fs::Permissions::from_mode(0o664)).unwrap();
        assert!(matches!(
            ConfigLoader::load(&manager_path, None, &[], ManagerScope::System, uid),
            Err(LoadError::Unsafe { .. })
        ));

        let real = root.write("real.toml", manager());
        let linked = root.0.join("linked.toml");
        symlink(real, &linked).unwrap();
        assert!(ConfigLoader::load(&linked, None, &[], ManagerScope::System, uid).is_err());
        assert_eq!(fs::metadata(&root.0).unwrap().uid(), uid);
    }

    #[test]
    fn missing_user_manager_uses_memory_default() {
        let root = TestRoot::new();
        let uid = fs::metadata(&root.0).unwrap().uid();
        let snapshot = ConfigLoader::load(
            &root.0.join("missing.toml"),
            Some(DEFAULT_USER_MANAGER),
            &[],
            ManagerScope::User,
            uid,
        )
        .unwrap();
        assert!(snapshot.services().is_empty());
        assert_eq!(snapshot.default_group().as_str(), "boot");
        assert_eq!(uid, fs::metadata(&root.0).unwrap().uid());
    }
}
