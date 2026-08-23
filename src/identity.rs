// SPDX-License-Identifier: BSD-2-Clause

use std::{collections::BTreeSet, fs, io, path::Path};

use thiserror::Error;

use crate::model::{
    IdentitySelection, IdentitySpec, ManagerScope, ProcessDefinition, ResolvedIdentity,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAccount {
    pub name: String,
    pub home: String,
    pub shell: String,
    pub identity: ResolvedIdentity,
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("invalid {file} entry on line {line}: {reason}")]
    InvalidEntry {
        file: &'static str,
        line: usize,
        reason: String,
    },
    #[error("unknown user {0}")]
    UnknownUser(String),
    #[error("unknown UID {0}")]
    UnknownUid(u32),
    #[error("unknown group {0}")]
    UnknownGroup(String),
    #[error("unknown GID {0}")]
    UnknownGid(u32),
    #[error("user service cannot select UID {requested}; manager UID is {manager}")]
    UserUidMismatch { requested: u32, manager: u32 },
    #[error("user service cannot select GID {requested}; manager GID is {manager}")]
    UserGidMismatch { requested: u32, manager: u32 },
}

#[derive(Clone, Debug)]
pub struct AccountDatabase {
    users: Vec<UserRecord>,
    groups: Vec<GroupRecord>,
}

impl AccountDatabase {
    /// Loads local account records beneath a target root without invoking NSS.
    ///
    /// # Errors
    ///
    /// Returns an error when either file cannot be read or contains a malformed
    /// non-comment record.
    pub fn load(root: &Path) -> Result<Self, IdentityError> {
        let passwd_path = root.join("etc/passwd");
        let group_path = root.join("etc/group");
        let passwd = fs::read_to_string(&passwd_path).map_err(|source| IdentityError::Read {
            path: passwd_path.display().to_string(),
            source,
        })?;
        let group = fs::read_to_string(&group_path).map_err(|source| IdentityError::Read {
            path: group_path.display().to_string(),
            source,
        })?;
        Ok(Self {
            users: parse_passwd(&passwd)?,
            groups: parse_group(&group)?,
        })
    }

    /// Resolves one process identity and its deterministic account environment.
    ///
    /// User managers may only select their own UID and GID. Supplementary groups
    /// include explicit entries and local group memberships for a named user.
    ///
    /// # Errors
    ///
    /// Returns an error when an identity is unknown or a user service attempts
    /// to change to another account.
    pub fn resolve(
        &self,
        process: &ProcessDefinition,
        scope: ManagerScope,
        manager: &ResolvedIdentity,
    ) -> Result<ResolvedAccount, IdentityError> {
        let user = match &process.user {
            IdentitySelection::Manager | IdentitySelection::UserPrimary => self
                .users
                .iter()
                .find(|user| user.uid == manager.uid)
                .ok_or(IdentityError::UnknownUid(manager.uid))?,
            IdentitySelection::Explicit(spec) => self.resolve_user(spec)?,
        };
        let gid = match &process.group {
            IdentitySelection::Manager => manager.gid,
            IdentitySelection::UserPrimary => user.gid,
            IdentitySelection::Explicit(spec) => self.resolve_group(spec)?.gid,
        };

        if scope == ManagerScope::User && user.uid != manager.uid {
            return Err(IdentityError::UserUidMismatch {
                requested: user.uid,
                manager: manager.uid,
            });
        }
        if scope == ManagerScope::User && gid != manager.gid {
            return Err(IdentityError::UserGidMismatch {
                requested: gid,
                manager: manager.gid,
            });
        }

        let mut supplementary = BTreeSet::new();
        if matches!(process.user, IdentitySelection::Manager) {
            supplementary.extend(manager.supplementary_groups.iter().copied());
        }
        supplementary.extend(
            self.groups
                .iter()
                .filter(|group| group.members.iter().any(|member| member == &user.name))
                .map(|group| group.gid),
        );
        for group in &process.supplementary_groups {
            supplementary.insert(self.resolve_group(group)?.gid);
        }
        supplementary.remove(&gid);

        Ok(ResolvedAccount {
            name: user.name.clone(),
            home: user.home.clone(),
            shell: user.shell.clone(),
            identity: ResolvedIdentity {
                uid: user.uid,
                gid,
                supplementary_groups: supplementary.into_iter().collect(),
            },
        })
    }

    fn resolve_user(&self, spec: &IdentitySpec) -> Result<&UserRecord, IdentityError> {
        match spec {
            IdentitySpec::Name(name) => self
                .users
                .iter()
                .find(|user| &user.name == name)
                .ok_or_else(|| IdentityError::UnknownUser(name.clone())),
            IdentitySpec::Id(uid) => self
                .users
                .iter()
                .find(|user| user.uid == *uid)
                .ok_or(IdentityError::UnknownUid(*uid)),
        }
    }

    fn resolve_group(&self, spec: &IdentitySpec) -> Result<&GroupRecord, IdentityError> {
        match spec {
            IdentitySpec::Name(name) => self
                .groups
                .iter()
                .find(|group| &group.name == name)
                .ok_or_else(|| IdentityError::UnknownGroup(name.clone())),
            IdentitySpec::Id(gid) => self
                .groups
                .iter()
                .find(|group| group.gid == *gid)
                .ok_or(IdentityError::UnknownGid(*gid)),
        }
    }
}

#[derive(Clone, Debug)]
struct UserRecord {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
}

#[derive(Clone, Debug)]
struct GroupRecord {
    name: String,
    gid: u32,
    members: Vec<String>,
}

fn parse_passwd(source: &str) -> Result<Vec<UserRecord>, IdentityError> {
    records(source)
        .map(|(line, record)| {
            let fields = record.split(':').collect::<Vec<_>>();
            if fields.len() != 7 || fields[0].is_empty() {
                return Err(invalid("passwd", line, "expected seven fields"));
            }
            Ok(UserRecord {
                name: fields[0].to_owned(),
                uid: parse_id("passwd", line, "UID", fields[2])?,
                gid: parse_id("passwd", line, "GID", fields[3])?,
                home: fields[5].to_owned(),
                shell: fields[6].to_owned(),
            })
        })
        .collect()
}

fn parse_group(source: &str) -> Result<Vec<GroupRecord>, IdentityError> {
    records(source)
        .map(|(line, record)| {
            let fields = record.split(':').collect::<Vec<_>>();
            if fields.len() != 4 || fields[0].is_empty() {
                return Err(invalid("group", line, "expected four fields"));
            }
            Ok(GroupRecord {
                name: fields[0].to_owned(),
                gid: parse_id("group", line, "GID", fields[2])?,
                members: fields[3]
                    .split(',')
                    .filter(|member| !member.is_empty())
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect()
}

fn records(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
}

fn parse_id(
    file: &'static str,
    line: usize,
    field: &str,
    value: &str,
) -> Result<u32, IdentityError> {
    value
        .parse()
        .map_err(|_| invalid(file, line, format!("invalid {field}")))
}

fn invalid(file: &'static str, line: usize, reason: impl Into<String>) -> IdentityError {
    IdentityError::InvalidEntry {
        file,
        line,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::model::{ProcessType, Readiness};

    use super::*;

    fn database() -> AccountDatabase {
        AccountDatabase {
            users: parse_passwd(
                "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:100:Alice:/home/alice:/bin/zsh\n",
            )
            .unwrap(),
            groups: parse_group("root:x:0:\nusers:x:100:alice\naudio:x:18:alice\nvideo:x:27:\n")
                .unwrap(),
        }
    }

    fn process(user: IdentitySelection, group: IdentitySelection) -> ProcessDefinition {
        ProcessDefinition {
            command: vec!["/bin/true".into()],
            kind: ProcessType::Simple,
            readiness: Readiness::Exec,
            user,
            group,
            supplementary_groups: vec![IdentitySpec::Name("video".into())],
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
            umask: 0o022,
        }
    }

    #[test]
    fn resolves_local_account_and_supplementary_groups() {
        let account = database()
            .resolve(
                &process(
                    IdentitySelection::Explicit(IdentitySpec::Name("alice".into())),
                    IdentitySelection::Explicit(IdentitySpec::Name("users".into())),
                ),
                ManagerScope::System,
                &ResolvedIdentity {
                    uid: 0,
                    gid: 0,
                    supplementary_groups: vec![],
                },
            )
            .unwrap();

        assert_eq!(account.identity.uid, 1000);
        assert_eq!(account.identity.gid, 100);
        assert_eq!(account.identity.supplementary_groups, vec![18, 27]);
        assert_eq!(account.home, "/home/alice");
    }

    #[test]
    fn user_scope_rejects_identity_change() {
        let result = database().resolve(
            &process(
                IdentitySelection::Explicit(IdentitySpec::Name("root".into())),
                IdentitySelection::Explicit(IdentitySpec::Name("root".into())),
            ),
            ManagerScope::User,
            &ResolvedIdentity {
                uid: 1000,
                gid: 100,
                supplementary_groups: vec![18],
            },
        );
        assert!(matches!(result, Err(IdentityError::UserUidMismatch { .. })));
    }

    #[test]
    fn rejects_malformed_account_records() {
        assert!(parse_passwd("broken").is_err());
        assert!(parse_group("broken").is_err());
    }
}
