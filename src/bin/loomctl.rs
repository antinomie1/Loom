// SPDX-License-Identifier: BSD-2-Clause

use std::{
    env,
    ffi::OsString,
    io::{self, Write as _},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use lexopt::prelude::*;
use loom::{
    linux::reactor::SeqPacketConnection,
    linux::{StartupLock, spawn_user_manager},
    protocol::{MessageKind, Operation, Packet, StatusCode},
};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("loomctl: {error}");
            ExitCode::from(error.downcast_ref::<io::Error>().map_or(
                2,
                |error| match error.kind() {
                    io::ErrorKind::PermissionDenied => 3,
                    io::ErrorKind::TimedOut => 5,
                    _ => 4,
                },
            ))
        }
    }
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let mut parser = lexopt::Parser::from_env();
    let mut user = false;
    let mut root = PathBuf::from("/");
    let mut runtime_dir = None;
    let mut command = None;
    let mut target = None;
    let mut now = false;
    let mut dry_run = false;
    let mut force = false;
    let mut format_toml = false;
    while let Some(argument) = parser.next()? {
        match argument {
            Long("user") => user = true,
            Long("system") => user = false,
            Long("root") => root = parser.value()?.into(),
            Long("runtime-dir") => runtime_dir = Some(PathBuf::from(parser.value()?)),
            Long("now") => now = true,
            Long("dry-run") => dry_run = true,
            Long("force") => force = true,
            Long("format") => match parser.value()?.to_str() {
                Some("toml") => format_toml = true,
                Some("human") => format_toml = false,
                _ => return Err("--format must be human or toml".into()),
            },
            Long("help") | Short('h') => {
                print_help();
                return Ok(0);
            }
            Long("version") | Short('V') => {
                println!("loomctl {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            Value(value) if command.is_none() => command = Some(value),
            Value(value) if target.is_none() => target = Some(value),
            _ => return Err(argument.unexpected().into()),
        }
    }

    let command = command.ok_or("missing command")?;
    let mut operation = parse_operation(&command)?;
    if dry_run {
        if operation != Operation::Apply {
            return Err("--dry-run is valid only with apply".into());
        }
        operation = Operation::ApplyDryRun;
    }
    if force {
        if operation != Operation::Stop {
            return Err("--force is valid only with stop".into());
        }
        operation = Operation::StopForce;
    }
    validate_target(operation, target.as_ref(), now)?;

    let runtime_dir = if user {
        runtime_dir
            .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .ok_or("--user requires XDG_RUNTIME_DIR")?
    } else {
        runtime_dir.unwrap_or_else(|| root.join("run"))
    };
    let socket = runtime_dir.join("loom/control.sock");
    let connection = connect(
        &socket,
        user && operation != Operation::ApplyDryRun,
        &root,
        &runtime_dir,
    )?;
    let mut request = Packet {
        kind: MessageKind::Request,
        request_id: 1,
        operation,
        status: StatusCode::Ok,
        more: false,
        payload: target
            .map(|target| {
                let target = target.to_string_lossy();
                if now {
                    format!("now\n{target}").into_bytes()
                } else {
                    target.into_owned().into_bytes()
                }
            })
            .unwrap_or_default(),
    };
    if format_toml {
        request.payload.splice(..0, b"toml\n".iter().copied());
    }
    exchange(&connection, &request, format_toml)
}

fn validate_target(
    operation: Operation,
    target: Option<&OsString>,
    now: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let needs_target = matches!(
        operation,
        Operation::Start
            | Operation::Stop
            | Operation::StopForce
            | Operation::Restart
            | Operation::ReloadService
            | Operation::IsActive
            | Operation::IsEnabled
            | Operation::Dependencies
            | Operation::Enable
            | Operation::Disable
            | Operation::ResetFailed
    );
    if needs_target && target.is_none() {
        return Err(format!("{operation:?} requires a service name").into());
    }
    if !needs_target && target.is_some() && operation != Operation::Status {
        return Err("unexpected service name".into());
    }
    if now && !matches!(operation, Operation::Enable | Operation::Disable) {
        return Err("--now is valid only with enable or disable".into());
    }

    Ok(())
}

fn exchange(
    connection: &SeqPacketConnection,
    request: &Packet,
    format_toml: bool,
) -> Result<u8, Box<dyn std::error::Error>> {
    connection.send(&request.encode()?)?;

    loop {
        let response = connection.receive()?.ok_or("manager disconnected")?;
        let response = Packet::decode(&response)?;
        if response.kind != MessageKind::Response || response.request_id != request.request_id {
            return Err("manager returned an unrelated response".into());
        }
        if !response.payload.is_empty() {
            let mut output: Box<dyn io::Write> =
                if !format_toml && response.status != StatusCode::Ok {
                    Box::new(io::stderr())
                } else {
                    Box::new(io::stdout())
                };
            output.write_all(&response.payload)?;
            if !response.more && !response.payload.ends_with(b"\n") {
                output.write_all(b"\n")?;
            }
        }
        if !response.more {
            return Ok(exit_code(response.status));
        }
    }
}

fn connect(
    socket: &Path,
    user: bool,
    root: &Path,
    runtime_dir: &Path,
) -> io::Result<SeqPacketConnection> {
    match SeqPacketConnection::connect(socket) {
        Ok(connection) => return Ok(connection),
        Err(error) if user && is_absent(&error) => {}
        Err(error) => return Err(error),
    }

    let metadata = std::fs::symlink_metadata(runtime_dir)?;
    let uid = loom::linux::current_identity()?.uid;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe XDG_RUNTIME_DIR",
        ));
    }
    let _lock = StartupLock::acquire(&runtime_dir.join("loom-start.lock"))?;
    if let Ok(connection) = SeqPacketConnection::connect(socket) {
        return Ok(connection);
    }
    let executable = env::current_exe()?;
    let loom = manager_executable(&executable)?;
    let arguments = vec![
        OsString::from("--user"),
        OsString::from("--root"),
        root.as_os_str().to_owned(),
        OsString::from("--runtime-dir"),
        runtime_dir.as_os_str().to_owned(),
    ];
    spawn_user_manager(&loom, &arguments)?.wait_ready(Duration::from_secs(2))?;
    SeqPacketConnection::connect(socket)
}

fn manager_executable(client: &Path) -> io::Result<PathBuf> {
    let directory = client
        .parent()
        .ok_or_else(|| io::Error::other("loomctl has no parent"))?;
    if let Some(prefix) = directory.parent() {
        let installed = prefix.join("lib/loom/loom");
        if installed.is_file() {
            return Ok(installed);
        }
    }
    let sibling = directory.join("loom");
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Loom manager executable is not installed",
    ))
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn parse_operation(command: &OsString) -> Result<Operation, String> {
    match command.to_str() {
        Some("start") => Ok(Operation::Start),
        Some("stop") => Ok(Operation::Stop),
        Some("restart") => Ok(Operation::Restart),
        Some("reload-service") => Ok(Operation::ReloadService),
        Some("status") => Ok(Operation::Status),
        Some("list") => Ok(Operation::List),
        Some("is-active") => Ok(Operation::IsActive),
        Some("is-enabled") => Ok(Operation::IsEnabled),
        Some("dependencies") => Ok(Operation::Dependencies),
        Some("enable") => Ok(Operation::Enable),
        Some("disable") => Ok(Operation::Disable),
        Some("reload") => Ok(Operation::Reload),
        Some("apply") => Ok(Operation::Apply),
        Some("reset-failed") => Ok(Operation::ResetFailed),
        Some("timings") => Ok(Operation::Timings),
        Some("critical-path") => Ok(Operation::CriticalPath),
        Some("reboot") => Ok(Operation::Reboot),
        Some("poweroff") => Ok(Operation::Poweroff),
        _ => Err(format!("unknown command {}", command.to_string_lossy())),
    }
}

const fn exit_code(status: StatusCode) -> u8 {
    match status {
        StatusCode::Ok => 0,
        StatusCode::ServiceFailure | StatusCode::Internal => 1,
        StatusCode::InvalidRequest | StatusCode::NotFound | StatusCode::Conflict => 2,
        StatusCode::PermissionDenied => 3,
        StatusCode::ManagerUnavailable => 4,
        StatusCode::Timeout => 5,
    }
}

fn print_help() {
    println!(
        "Control Loom services\n\n\
         Usage: loomctl [--system|--user] COMMAND [SERVICE]\n\n\
         Commands: start stop restart reload-service status list is-active\n  \
                   is-enabled dependencies enable disable reload apply\n  \
                   reset-failed timings critical-path reboot poweroff\n\n\
         Options: --now (enable/disable), --dry-run (apply), --force (stop), --format human|toml"
    );
}
