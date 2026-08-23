// SPDX-License-Identifier: BSD-2-Clause

use std::{
    env,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
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
            ExitCode::from(2)
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
    while let Some(argument) = parser.next()? {
        match argument {
            Long("user") => user = true,
            Long("system") => user = false,
            Long("root") => root = parser.value()?.into(),
            Long("runtime-dir") => runtime_dir = Some(PathBuf::from(parser.value()?)),
            Long("now") => now = true,
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
    let operation = parse_operation(&command)?;
    let needs_target = matches!(
        operation,
        Operation::Start
            | Operation::Stop
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
        return Err(format!("{} requires a service name", command.to_string_lossy()).into());
    }
    if !needs_target && target.is_some() && operation != Operation::Status {
        return Err("unexpected service name".into());
    }
    if now && !matches!(operation, Operation::Enable | Operation::Disable) {
        return Err("--now is valid only with enable or disable".into());
    }

    let runtime_dir = if user {
        runtime_dir
            .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .ok_or("--user requires XDG_RUNTIME_DIR")?
    } else {
        runtime_dir.unwrap_or_else(|| root.join("run"))
    };
    let socket = runtime_dir.join("loom/control.sock");
    let connection = connect(&socket, user, &root, &runtime_dir)?;
    let request = Packet {
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
    connection.send(&request.encode()?)?;

    loop {
        let response = connection.receive()?.ok_or("manager disconnected")?;
        let response = Packet::decode(&response)?;
        if response.kind != MessageKind::Response || response.request_id != request.request_id {
            return Err("manager returned an unrelated response".into());
        }
        if !response.payload.is_empty() {
            print!("{}", String::from_utf8_lossy(&response.payload));
            if !response.payload.ends_with(b"\n") {
                println!();
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

    let _lock = StartupLock::acquire(&runtime_dir.join("loom-start.lock"))?;
    if let Ok(connection) = SeqPacketConnection::connect(socket) {
        return Ok(connection);
    }
    let executable = env::current_exe()?;
    let loom = executable
        .parent()
        .ok_or_else(|| io::Error::other("loomctl has no parent directory"))?
        .join("loom");
    let arguments = vec![
        OsString::from("--user"),
        OsString::from("--root"),
        root.as_os_str().to_owned(),
        OsString::from("--runtime-dir"),
        runtime_dir.as_os_str().to_owned(),
    ];
    let mut child = spawn_user_manager(&loom, &arguments)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(connection) = SeqPacketConnection::connect(socket) {
            return Ok(connection);
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "user manager exited during startup: {status}"
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "user manager did not create its control socket",
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
                   reset-failed timings critical-path reboot poweroff"
    );
}
