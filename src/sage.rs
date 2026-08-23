// SPDX-License-Identifier: BSD-2-Clause

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{IdentitySpec, ManagerScope, ServiceDefinition, ServiceId};

#[derive(Debug, Error)]
pub enum SageError {
    #[error("invalid Sage service TOML: {0}")]
    Parse(#[from] toml_edit::de::Error),
    #[error("unsupported Sage service schema {0}")]
    Schema(u32),
    #[error("invalid Sage service {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("failed to serialize native service: {0}")]
    Serialize(#[from] toml_edit::ser::Error),
    #[error("generated native service is invalid: {0}")]
    Generated(#[from] crate::model::ConfigError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledService {
    pub id: ServiceId,
    pub toml: String,
}

/// Compiles one init-independent Sage service specification into native Loom
/// TOML without executing a shell or consulting Sage state.
///
/// # Errors
///
/// Returns an error for malformed/unsupported Sage input, unresolved runtime
/// channels, unsupported daemon types/before edges, invalid v1 shell words, or
/// an invalid generated Loom definition.
pub fn compile_service(source: &str) -> Result<CompiledService, SageError> {
    let raw: RawDocument = toml_edit::de::from_str(source)?;
    if !matches!(raw.schema_version, 1 | 2) {
        return Err(SageError::Schema(raw.schema_version));
    }
    let service = raw.service;
    let id = ServiceId::new(service.name.clone())?;
    if !service.before.is_empty() {
        return Err(SageError::Invalid {
            field: "service.before",
            reason: "Loom uses after edges only".into(),
        });
    }
    if service
        .runtime
        .as_deref()
        .is_some_and(|runtime| !runtime.is_empty())
    {
        return Err(SageError::Invalid {
            field: "service.runtime",
            reason: "Sage must resolve channel bindings before compilation".into(),
        });
    }
    if service.pid_file.is_some() {
        return Err(SageError::Invalid {
            field: "service.pid_file",
            reason: "Loom supervises foreground processes and does not read PID files".into(),
        });
    }
    let process_type = validate_process_type(service.process_type.as_deref())?;
    let command = match raw.schema_version {
        1 => {
            if service.command.is_some() {
                return Err(SageError::Invalid {
                    field: "service.command",
                    reason: "schema v1 requires exec_start".into(),
                });
            }
            split_v1("service.exec_start", service.exec_start.as_deref())?
        }
        2 => service.command.clone().ok_or_else(|| SageError::Invalid {
            field: "service.command",
            reason: "schema v2 requires an argv array".into(),
        })?,
        _ => unreachable!("schema checked above"),
    };
    let stop = command_field(
        raw.schema_version,
        "service stop command",
        service.stop_command,
        service.exec_stop.as_deref(),
    )?;
    let reload = command_field(
        raw.schema_version,
        "service reload command",
        service.reload_command,
        service.exec_reload.as_deref(),
    )?;
    let restart = validate_restart(service.restart)?;
    let readiness = validate_readiness(service.readiness)?;

    let native = NativeDocument {
        schema_version: 1,
        description: service.description,
        process: NativeProcess {
            command,
            process_type,
            readiness,
            user: service.user,
            group: service.group,
            working_directory: service.working_directory,
            environment: service.environment,
        },
        dependencies: NativeDependencies {
            requires: service.requires,
            wants: service.wants,
            after: service.after,
            conflicts: service.conflicts,
        },
        supervision: NativeSupervision { restart },
        actions: NativeActions { stop, reload },
    };
    let toml = toml_edit::ser::to_string_pretty(&native)?;
    ServiceDefinition::parse(id.clone(), &toml, ManagerScope::System)?;
    Ok(CompiledService { id, toml })
}

fn validate_process_type(process_type: Option<&str>) -> Result<&'static str, SageError> {
    match process_type.unwrap_or("simple") {
        "simple" => Ok("simple"),
        "oneshot" => Ok("oneshot"),
        other => Err(SageError::Invalid {
            field: "service.type",
            reason: format!("unsupported process type {other:?}"),
        }),
    }
}

fn validate_restart(restart: Option<String>) -> Result<String, SageError> {
    let restart = restart.unwrap_or_else(|| "no".into());
    if matches!(restart.as_str(), "no" | "always" | "on-failure") {
        Ok(restart)
    } else {
        Err(SageError::Invalid {
            field: "service.restart",
            reason: format!("unsupported policy {restart:?}"),
        })
    }
}

fn validate_readiness(readiness: Option<String>) -> Result<String, SageError> {
    let readiness = readiness.unwrap_or_else(|| "exec".into());
    if matches!(readiness.as_str(), "exec" | "notify") {
        Ok(readiness)
    } else {
        Err(SageError::Invalid {
            field: "service.readiness",
            reason: format!("unsupported readiness {readiness:?}"),
        })
    }
}

fn split_v1(field: &'static str, command: Option<&str>) -> Result<Vec<String>, SageError> {
    let command = command.ok_or_else(|| SageError::Invalid {
        field,
        reason: "schema v1 requires this command".into(),
    })?;
    shlex::split(command).ok_or_else(|| SageError::Invalid {
        field,
        reason: "contains unmatched quotes or invalid shell words".into(),
    })
}

fn command_field(
    schema: u32,
    field: &'static str,
    argv: Option<Vec<String>>,
    legacy: Option<&str>,
) -> Result<Option<Vec<String>>, SageError> {
    match schema {
        1 => legacy
            .map(|command| split_v1(field, Some(command)))
            .transpose(),
        2 => {
            if legacy.is_some() {
                Err(SageError::Invalid {
                    field,
                    reason: "schema v2 requires an argv array".into(),
                })
            } else {
                Ok(argv)
            }
        }
        _ => unreachable!("schema checked by caller"),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    schema_version: u32,
    service: RawService,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    name: String,
    #[serde(default)]
    description: String,
    command: Option<Vec<String>>,
    exec_start: Option<String>,
    stop_command: Option<Vec<String>>,
    exec_stop: Option<String>,
    reload_command: Option<Vec<String>>,
    exec_reload: Option<String>,
    user: Option<IdentitySpec>,
    group: Option<IdentitySpec>,
    #[serde(rename = "type")]
    process_type: Option<String>,
    readiness: Option<String>,
    #[serde(default)]
    restart: Option<String>,
    #[serde(default, alias = "working_dir")]
    working_directory: Option<String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    wants: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    before: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
    runtime: Option<String>,
    pid_file: Option<String>,
}

#[derive(Serialize)]
struct NativeDocument {
    schema_version: u32,
    description: String,
    process: NativeProcess,
    dependencies: NativeDependencies,
    supervision: NativeSupervision,
    actions: NativeActions,
}

#[derive(Serialize)]
struct NativeProcess {
    command: Vec<String>,
    #[serde(rename = "type")]
    process_type: &'static str,
    readiness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<IdentitySpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<IdentitySpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_directory: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    environment: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct NativeDependencies {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requires: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    wants: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    after: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
}

#[derive(Serialize)]
struct NativeSupervision {
    restart: String,
}

#[derive(Serialize)]
struct NativeActions {
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reload: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_existing_v1_without_a_shell() {
        let source = r#"
schema_version = 1
[service]
name = "dbus"
description = "D-Bus"
exec_start = "/usr/bin/dbus-daemon --system --nofork"
restart = "on-failure"
after = ["localmount"]
"#;
        let compiled = compile_service(source).unwrap();
        let native =
            ServiceDefinition::parse(compiled.id, &compiled.toml, ManagerScope::System).unwrap();
        assert_eq!(
            native.process.command,
            ["/usr/bin/dbus-daemon", "--system", "--nofork"]
        );
    }

    #[test]
    fn compiles_v2_argv_and_actions() {
        let source = r#"
schema_version = 2
[service]
name = "worker"
command = ["/usr/bin/worker", "--foreground"]
stop_command = ["/usr/bin/workerctl", "stop"]
readiness = "notify"
requires = ["db"]
environment = { MODE = "production" }
"#;
        let compiled = compile_service(source).unwrap();
        assert!(compiled.toml.contains("readiness = \"notify\""));
        assert!(compiled.toml.contains("stop = ["));
    }

    #[test]
    fn rejects_forking_and_unresolved_runtime() {
        let forking = r#"
schema_version = 2
[service]
name = "bad"
command = ["/usr/bin/bad"]
type = "forking"
"#;
        let runtime = r#"
schema_version = 2
[service]
name = "bad"
command = ["/usr/bin/bad"]
runtime = "runtime/java:21"
"#;
        assert!(compile_service(forking).is_err());
        assert!(compile_service(runtime).is_err());
    }
}
