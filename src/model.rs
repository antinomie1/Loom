// SPDX-License-Identifier: BSD-2-Clause

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_START_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_STOP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_RESTART_LIMIT: u32 = 5;
const DEFAULT_RESTART_WINDOW_MS: u64 = 300_000;
const DEFAULT_RESTART_RESET_MS: u64 = 300_000;
const DEFAULT_RESTART_BACKOFF_MAX_MS: u64 = 30_000;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceId(String);

impl ServiceId {
    /// Creates a validated service identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidServiceId`] when the value is empty, too
    /// long, or contains a character outside Loom's identifier grammar.
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        let valid = value.len() <= 128
            && value.bytes().enumerate().all(|(index, byte)| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => true,
                b'_' | b'.' | b'@' | b'-' => index != 0,
                _ => false,
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(ConfigError::InvalidServiceId(value))
        }
    }

    /// Derives an identifier from a `.toml` service filename.
    ///
    /// # Errors
    ///
    /// Returns an error when the path has no UTF-8 filename, lacks the `.toml`
    /// suffix, or its stem is not a valid service identifier.
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ConfigError::InvalidServicePath(path.to_path_buf()))?;
        let name = file_name
            .strip_suffix(".toml")
            .ok_or_else(|| ConfigError::InvalidServicePath(path.to_path_buf()))?;
        Self::new(name)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagerScope {
    System,
    User,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceDefinition {
    pub id: ServiceId,
    pub description: String,
    pub process: ProcessDefinition,
    pub dependencies: Dependencies,
    pub supervision: Supervision,
    pub actions: Actions,
    pub io: IoDefinition,
}

impl ServiceDefinition {
    /// Parses and validates one native Loom service definition.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed TOML, unsupported schemas, unknown fields,
    /// or values that violate the service-definition invariants.
    pub fn parse(id: ServiceId, source: &str, scope: ManagerScope) -> Result<Self, ConfigError> {
        let raw: RawServiceDefinition = toml_edit::de::from_str(source)?;
        require_schema(raw.schema_version)?;

        let process = ProcessDefinition::from_raw(raw.process, scope)?;
        let dependencies = Dependencies::from_raw(raw.dependencies, &id)?;
        let supervision = Supervision::from_raw(&raw.supervision)?;
        if process.kind == ProcessType::Oneshot && supervision.restart == RestartPolicy::Always {
            return Err(ConfigError::InvalidField {
                field: "supervision.restart",
                reason: "oneshot services cannot restart after successful completion".into(),
            });
        }

        Ok(Self {
            id,
            description: raw.description,
            process,
            dependencies,
            supervision,
            actions: Actions::from_raw(raw.actions)?,
            io: IoDefinition::from_raw(raw.io)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessDefinition {
    pub command: Vec<String>,
    pub kind: ProcessType,
    pub readiness: Readiness,
    pub user: IdentitySelection,
    pub group: IdentitySelection,
    pub supplementary_groups: Vec<IdentitySpec>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub umask: u32,
}

impl ProcessDefinition {
    fn from_raw(raw: RawProcess, scope: ManagerScope) -> Result<Self, ConfigError> {
        validate_command("process.command", &raw.command)?;
        require_absolute("process.working_directory", &raw.working_directory)?;
        if raw.umask > 0o777 {
            return Err(ConfigError::InvalidField {
                field: "process.umask",
                reason: "must be between 0o000 and 0o777".into(),
            });
        }
        for identity in raw
            .user
            .iter()
            .chain(raw.group.iter())
            .chain(raw.supplementary_groups.iter())
        {
            validate_identity(identity)?;
        }
        let mut groups = HashSet::with_capacity(raw.supplementary_groups.len());
        if raw
            .supplementary_groups
            .iter()
            .any(|group| !groups.insert(group))
        {
            return Err(ConfigError::InvalidField {
                field: "process.supplementary_groups",
                reason: "duplicate group".into(),
            });
        }
        for (key, value) in &raw.environment {
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return Err(ConfigError::InvalidField {
                    field: "process.environment",
                    reason: format!("invalid environment entry {key:?}"),
                });
            }
        }

        let readiness = match (raw.kind, raw.readiness) {
            (ProcessType::Oneshot, RawReadiness::Notify) => {
                return Err(ConfigError::InvalidField {
                    field: "process.readiness",
                    reason: "oneshot readiness is successful process completion".into(),
                });
            }
            (ProcessType::Oneshot, RawReadiness::Exec) => Readiness::Completion,
            (ProcessType::Simple, RawReadiness::Exec) => Readiness::Exec,
            (ProcessType::Simple, RawReadiness::Notify) => Readiness::Notify,
        };

        let user_was_explicit = raw.user.is_some();
        let user = raw.user.map_or_else(
            || match scope {
                ManagerScope::System => {
                    IdentitySelection::Explicit(IdentitySpec::Name("root".into()))
                }
                ManagerScope::User => IdentitySelection::Manager,
            },
            IdentitySelection::Explicit,
        );
        let group = raw.group.map_or_else(
            || match scope {
                ManagerScope::User => IdentitySelection::Manager,
                ManagerScope::System if user_was_explicit => IdentitySelection::UserPrimary,
                ManagerScope::System => {
                    IdentitySelection::Explicit(IdentitySpec::Name("root".into()))
                }
            },
            IdentitySelection::Explicit,
        );

        Ok(Self {
            command: raw.command,
            kind: raw.kind,
            readiness,
            user,
            group,
            supplementary_groups: raw.supplementary_groups,
            working_directory: raw.working_directory,
            environment: raw.environment,
            umask: raw.umask,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProcessType {
    Simple,
    Oneshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    Exec,
    Notify,
    Completion,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(untagged)]
pub enum IdentitySpec {
    Name(String),
    Id(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentitySelection {
    Manager,
    UserPrimary,
    Explicit(IdentitySpec),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedIdentity {
    pub uid: u32,
    pub gid: u32,
    pub supplementary_groups: Vec<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Dependencies {
    pub requires: Vec<ServiceId>,
    pub wants: Vec<ServiceId>,
    pub after: Vec<ServiceId>,
    pub conflicts: Vec<ServiceId>,
}

impl Dependencies {
    fn from_raw(raw: RawDependencies, owner: &ServiceId) -> Result<Self, ConfigError> {
        Self::from_lists(owner, raw.requires, raw.wants, raw.after, raw.conflicts)
    }

    pub(crate) fn from_lists(
        owner: &ServiceId,
        requires: Vec<String>,
        wants: Vec<String>,
        after: Vec<String>,
        conflicts: Vec<String>,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            requires: parse_dependency_list("dependencies.requires", requires, owner)?,
            wants: parse_dependency_list("dependencies.wants", wants, owner)?,
            after: parse_dependency_list("dependencies.after", after, owner)?,
            conflicts: parse_dependency_list("dependencies.conflicts", conflicts, owner)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    No,
    OnFailure,
    Always,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Supervision {
    pub restart: RestartPolicy,
    pub start_timeout_ms: u64,
    pub stop_timeout_ms: u64,
    pub restart_limit: u32,
    pub restart_window_ms: u64,
    pub restart_reset_ms: u64,
    pub restart_backoff_max_ms: u64,
}

impl Supervision {
    fn from_raw(raw: &RawSupervision) -> Result<Self, ConfigError> {
        if raw.restart_limit > 0 && raw.restart_window_ms == 0 {
            return Err(ConfigError::InvalidField {
                field: "supervision.restart_window_ms",
                reason: "must be non-zero when restart_limit is non-zero".into(),
            });
        }
        Ok(Self {
            restart: raw.restart,
            start_timeout_ms: raw.start_timeout_ms,
            stop_timeout_ms: raw.stop_timeout_ms,
            restart_limit: raw.restart_limit,
            restart_window_ms: raw.restart_window_ms,
            restart_reset_ms: raw.restart_reset_ms,
            restart_backoff_max_ms: raw.restart_backoff_max_ms,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Actions {
    pub stop: Option<Vec<String>>,
    pub reload: Option<Vec<String>>,
}

impl Actions {
    fn from_raw(raw: RawActions) -> Result<Self, ConfigError> {
        if let Some(command) = &raw.stop {
            validate_command("actions.stop", command)?;
        }
        if let Some(command) = &raw.reload {
            validate_command("actions.reload", command)?;
        }
        Ok(Self {
            stop: raw.stop,
            reload: raw.reload,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IoDefinition {
    pub stdout: OutputTarget,
    pub stderr: OutputTarget,
}

impl IoDefinition {
    fn from_raw(raw: RawIo) -> Result<Self, ConfigError> {
        Ok(Self {
            stdout: OutputTarget::from_raw("io.stdout", raw.stdout)?,
            stderr: OutputTarget::from_raw("io.stderr", raw.stderr)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputTarget {
    Console,
    Null,
    Append(PathBuf),
    Socket(PathBuf),
}

impl OutputTarget {
    fn from_raw(field: &'static str, raw: RawOutput) -> Result<Self, ConfigError> {
        match raw {
            RawOutput::Name(RawOutputName::Console) => Ok(Self::Console),
            RawOutput::Name(RawOutputName::Null) => Ok(Self::Null),
            RawOutput::Append { append } => {
                require_absolute(field, &append)?;
                Ok(Self::Append(append))
            }
            RawOutput::Socket { socket } => {
                require_absolute(field, &socket)?;
                Ok(Self::Socket(socket))
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid service identifier {0:?}")]
    InvalidServiceId(String),
    #[error("service path must end in a valid .toml filename: {0}")]
    InvalidServicePath(PathBuf),
    #[error("unsupported schema version {found}; supported version is {SCHEMA_VERSION}")]
    UnsupportedSchema { found: u32 },
    #[error("invalid {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("duplicate definition for {0}")]
    DuplicateDefinition(ServiceId),
    #[error("service and group share the identifier {0}")]
    AmbiguousDefinition(ServiceId),
    #[error("default group {0} does not exist")]
    MissingDefaultGroup(ServiceId),
    #[error("{owner} references missing {relation} target {target}")]
    MissingDependency {
        owner: ServiceId,
        relation: &'static str,
        target: ServiceId,
    },
    #[error("dependency cycle: {}", display_cycle(.0))]
    DependencyCycle(Vec<ServiceId>),
    #[error("group {group} activates conflicting nodes {left} and {right}")]
    EnabledConflict {
        group: ServiceId,
        left: ServiceId,
        right: ServiceId,
    },
    #[error("invalid TOML: {0}")]
    Toml(#[from] toml_edit::de::Error),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServiceDefinition {
    schema_version: u32,
    #[serde(default)]
    description: String,
    process: RawProcess,
    #[serde(default)]
    dependencies: RawDependencies,
    #[serde(default)]
    supervision: RawSupervision,
    #[serde(default)]
    actions: RawActions,
    #[serde(default)]
    io: RawIo,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcess {
    command: Vec<String>,
    #[serde(default = "default_process_type", rename = "type")]
    kind: ProcessType,
    #[serde(default)]
    readiness: RawReadiness,
    user: Option<IdentitySpec>,
    group: Option<IdentitySpec>,
    #[serde(default)]
    supplementary_groups: Vec<IdentitySpec>,
    #[serde(default = "default_working_directory")]
    working_directory: PathBuf,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default = "default_umask")]
    umask: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawReadiness {
    #[default]
    Exec,
    Notify,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependencies {
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    wants: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSupervision {
    #[serde(default = "default_restart_policy")]
    restart: RestartPolicy,
    #[serde(default = "default_start_timeout")]
    start_timeout_ms: u64,
    #[serde(default = "default_stop_timeout")]
    stop_timeout_ms: u64,
    #[serde(default = "default_restart_limit")]
    restart_limit: u32,
    #[serde(default = "default_restart_window")]
    restart_window_ms: u64,
    #[serde(default = "default_restart_reset")]
    restart_reset_ms: u64,
    #[serde(default = "default_restart_backoff_max")]
    restart_backoff_max_ms: u64,
}

impl Default for RawSupervision {
    fn default() -> Self {
        Self {
            restart: default_restart_policy(),
            start_timeout_ms: DEFAULT_START_TIMEOUT_MS,
            stop_timeout_ms: DEFAULT_STOP_TIMEOUT_MS,
            restart_limit: DEFAULT_RESTART_LIMIT,
            restart_window_ms: DEFAULT_RESTART_WINDOW_MS,
            restart_reset_ms: DEFAULT_RESTART_RESET_MS,
            restart_backoff_max_ms: DEFAULT_RESTART_BACKOFF_MAX_MS,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActions {
    stop: Option<Vec<String>>,
    reload: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIo {
    #[serde(default)]
    stdout: RawOutput,
    #[serde(default)]
    stderr: RawOutput,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawOutput {
    Name(RawOutputName),
    Append { append: PathBuf },
    Socket { socket: PathBuf },
}

impl Default for RawOutput {
    fn default() -> Self {
        Self::Name(RawOutputName::Console)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawOutputName {
    Console,
    Null,
}

pub(crate) fn require_schema(found: u32) -> Result<(), ConfigError> {
    if found == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ConfigError::UnsupportedSchema { found })
    }
}

fn validate_identity(identity: &IdentitySpec) -> Result<(), ConfigError> {
    if let IdentitySpec::Name(name) = identity
        && (name.is_empty() || name.contains([':', '\0']))
    {
        return Err(ConfigError::InvalidField {
            field: "process identity",
            reason: format!("invalid account name {name:?}"),
        });
    }
    Ok(())
}

fn validate_command(field: &'static str, command: &[String]) -> Result<(), ConfigError> {
    let Some(executable) = command.first() else {
        return Err(ConfigError::InvalidField {
            field,
            reason: "command must not be empty".into(),
        });
    };
    require_absolute(field, Path::new(executable))?;
    if command.iter().any(|argument| argument.contains('\0')) {
        return Err(ConfigError::InvalidField {
            field,
            reason: "arguments must not contain NUL".into(),
        });
    }
    Ok(())
}

fn require_absolute(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::InvalidField {
            field,
            reason: format!("path must be absolute: {}", path.display()),
        })
    }
}

fn parse_dependency_list(
    field: &'static str,
    values: Vec<String>,
    owner: &ServiceId,
) -> Result<Vec<ServiceId>, ConfigError> {
    let mut seen = HashSet::with_capacity(values.len());
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        let id = ServiceId::new(value)?;
        if &id == owner {
            return Err(ConfigError::InvalidField {
                field,
                reason: "a service cannot depend on itself".into(),
            });
        }
        if !seen.insert(id.clone()) {
            return Err(ConfigError::InvalidField {
                field,
                reason: format!("duplicate service {id}"),
            });
        }
        parsed.push(id);
    }
    Ok(parsed)
}

const fn default_process_type() -> ProcessType {
    ProcessType::Simple
}

fn default_working_directory() -> PathBuf {
    PathBuf::from("/")
}

const fn default_umask() -> u32 {
    0o022
}

const fn default_restart_policy() -> RestartPolicy {
    RestartPolicy::No
}

const fn default_start_timeout() -> u64 {
    DEFAULT_START_TIMEOUT_MS
}

const fn default_stop_timeout() -> u64 {
    DEFAULT_STOP_TIMEOUT_MS
}

const fn default_restart_limit() -> u32 {
    DEFAULT_RESTART_LIMIT
}

const fn default_restart_window() -> u64 {
    DEFAULT_RESTART_WINDOW_MS
}

const fn default_restart_reset() -> u64 {
    DEFAULT_RESTART_RESET_MS
}

const fn default_restart_backoff_max() -> u64 {
    DEFAULT_RESTART_BACKOFF_MAX_MS
}

fn display_cycle(cycle: &[ServiceId]) -> String {
    cycle
        .iter()
        .map(ServiceId::as_str)
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
schema_version = 1
[process]
command = ["/usr/bin/example", "--foreground"]
"#;

    #[test]
    fn parses_minimal_system_service_with_deterministic_defaults() {
        let service = ServiceDefinition::parse(
            ServiceId::new("example").unwrap(),
            MINIMAL,
            ManagerScope::System,
        )
        .unwrap();

        assert_eq!(service.process.kind, ProcessType::Simple);
        assert_eq!(service.process.readiness, Readiness::Exec);
        assert_eq!(service.process.working_directory, Path::new("/"));
        assert_eq!(service.process.umask, 0o022);
        assert_eq!(
            service.process.user,
            IdentitySelection::Explicit(IdentitySpec::Name("root".into()))
        );
        assert_eq!(service.supervision.start_timeout_ms, 30_000);
        assert_eq!(service.io.stdout, OutputTarget::Console);
    }

    #[test]
    fn user_service_defaults_to_manager_identity() {
        let service = ServiceDefinition::parse(
            ServiceId::new("example").unwrap(),
            MINIMAL,
            ManagerScope::User,
        )
        .unwrap();

        assert_eq!(service.process.user, IdentitySelection::Manager);
        assert_eq!(service.process.group, IdentitySelection::Manager);
    }

    #[test]
    fn rejects_unknown_fields() {
        let source = format!("{MINIMAL}\nmagic = true\n");
        let error = ServiceDefinition::parse(
            ServiceId::new("example").unwrap(),
            &source,
            ManagerScope::System,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_implicit_shell_command() {
        let source = r#"
schema_version = 1
[process]
command = ["echo hello | cat"]
"#;
        let error = ServiceDefinition::parse(
            ServiceId::new("example").unwrap(),
            source,
            ManagerScope::System,
        )
        .unwrap_err();

        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn converts_oneshot_readiness_to_completion() {
        let source = r#"
schema_version = 1
[process]
command = ["/usr/bin/mount", "-a"]
type = "oneshot"
"#;
        let service = ServiceDefinition::parse(
            ServiceId::new("localmount").unwrap(),
            source,
            ManagerScope::System,
        )
        .unwrap();

        assert_eq!(service.process.readiness, Readiness::Completion);
    }

    #[test]
    fn rejects_duplicate_and_self_dependencies() {
        let duplicate = r#"
schema_version = 1
[process]
command = ["/usr/bin/example"]
[dependencies]
requires = ["db", "db"]
"#;
        let self_dependency = r#"
schema_version = 1
[process]
command = ["/usr/bin/example"]
[dependencies]
after = ["example"]
"#;

        for source in [duplicate, self_dependency] {
            assert!(
                ServiceDefinition::parse(
                    ServiceId::new("example").unwrap(),
                    source,
                    ManagerScope::System,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn parses_structured_output_targets() {
        let source = r#"
schema_version = 1
[process]
command = ["/usr/bin/example"]
[io]
stdout = { append = "/var/log/example.log" }
stderr = { socket = "/run/logger.sock" }
"#;
        let service = ServiceDefinition::parse(
            ServiceId::new("example").unwrap(),
            source,
            ManagerScope::System,
        )
        .unwrap();

        assert_eq!(
            service.io.stdout,
            OutputTarget::Append(PathBuf::from("/var/log/example.log"))
        );
        assert_eq!(
            service.io.stderr,
            OutputTarget::Socket(PathBuf::from("/run/logger.sock"))
        );
    }
}
