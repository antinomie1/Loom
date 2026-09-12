// SPDX-License-Identifier: BSD-2-Clause

use std::{env, ffi::OsString, fs, path::PathBuf, process::ExitCode};

use lexopt::prelude::*;
use loom::{
    config_edit::atomic_write,
    linux::{mount_api_filesystems, notify_launcher, rescue_loop, shutdown_system},
    loader::ConfigLoader,
    manager::{Manager, ManagerMode, ManagerOptions, ShutdownAction},
    sage::compile_service,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if std::process::id() == 1 => rescue_loop(&error.to_string()),
        Err(error) => {
            eprintln!("loom: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut parser = lexopt::Parser::from_env();
    let mut mode = ManagerMode::System;
    let mut root = PathBuf::from("/");
    let mut config_home = None;
    let mut runtime_dir = None;
    let mut command = None;
    let mut sage_input = None;
    let mut output = None;
    let mut ready_fd = None;
    while let Some(argument) = parser.next()? {
        match argument {
            Long("ready-fd") => ready_fd = Some(parser.value()?.to_string_lossy().parse::<i32>()?),
            Long("user") => mode = ManagerMode::User,
            Long("system") => mode = ManagerMode::System,
            Long("root") => root = parser.value()?.into(),
            Long("config-home") => config_home = Some(PathBuf::from(parser.value()?)),
            Long("runtime-dir") => runtime_dir = Some(PathBuf::from(parser.value()?)),
            Long("from-sage") => sage_input = Some(PathBuf::from(parser.value()?)),
            Long("output") => output = Some(parser.value()?),
            Long("help") | Short('h') => {
                print_help();
                return Ok(());
            }
            Long("version") | Short('V') => {
                println!("loom {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Value(value) if command.is_none() => command = Some(value),
            _ => return Err(argument.unexpected().into()),
        }
    }

    if let Some(command) = command {
        return run_offline(
            &command,
            mode,
            &root,
            config_home.as_deref(),
            runtime_dir.as_deref(),
            sage_input.as_deref(),
            output.as_deref(),
        );
    }

    if mode == ManagerMode::System && std::process::id() == 1 && root == std::path::Path::new("/") {
        mount_api_filesystems()?;
    }

    let options = match mode {
        ManagerMode::System => {
            let mut options = ManagerOptions::system(root);
            if let Some(runtime_dir) = runtime_dir {
                options.runtime_dir = runtime_dir;
            }
            options
        }
        ManagerMode::User => {
            let config_home = config_home
                .or_else(|| env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
                .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
                .ok_or("user manager requires XDG_CONFIG_HOME or HOME")?;
            let runtime_dir = runtime_dir
                .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
                .ok_or("user manager requires XDG_RUNTIME_DIR")?;
            ManagerOptions::user(root, config_home, runtime_dir)
        }
    };

    if ready_fd.is_some() && mode != ManagerMode::User {
        return Err("--ready-fd is restricted to user managers".into());
    }
    let manager = Manager::new(options)?;
    if let Some(fd) = ready_fd {
        notify_launcher(fd)?;
    }
    let action = manager.run()?;
    if mode == ManagerMode::System && std::process::id() == 1 {
        match action {
            ShutdownAction::Reboot => shutdown_system(true)?,
            ShutdownAction::Poweroff => shutdown_system(false)?,
            ShutdownAction::Exit => {}
        }
    }
    Ok(())
}

fn run_offline(
    command: &OsString,
    mode: ManagerMode,
    root: &std::path::Path,
    config_home: Option<&std::path::Path>,
    runtime_dir: Option<&std::path::Path>,
    sage_input: Option<&std::path::Path>,
    output: Option<&std::ffi::OsStr>,
) -> Result<(), Box<dyn std::error::Error>> {
    match command.to_str() {
        Some("validate") => {
            match mode {
                ManagerMode::System => {
                    let _ = ConfigLoader::load_system(root)?;
                }
                ManagerMode::User => {
                    let config_home =
                        config_home.ok_or("validate --user requires --config-home")?;
                    let runtime_dir =
                        runtime_dir.ok_or("validate --user requires --runtime-dir")?;
                    let identity = loom::linux::current_identity()?;
                    let _ = ConfigLoader::load_user(root, config_home, runtime_dir, identity.uid)?;
                }
            }
            println!("configuration is valid");
            Ok(())
        }
        Some("compile-service") => {
            let input = sage_input.ok_or("compile-service requires --from-sage FILE")?;
            let output = output.ok_or("compile-service requires --output FILE or -")?;
            let compiled = compile_service(&fs::read_to_string(input)?)?;
            if output == "-" {
                print!("{}", compiled.toml);
            } else {
                atomic_write(&PathBuf::from(output), &compiled.toml, 0o644)?;
            }
            Ok(())
        }
        _ => Err(format!("unknown loom command {}", command.to_string_lossy()).into()),
    }
}

fn print_help() {
    println!(
        "Loom init and service manager\n\n\
         Usage: loom [--system|--user] [OPTIONS]\n       loom validate [--root DIR]\n       loom compile-service --from-sage FILE --output FILE\n\n\
         Options:\n  \
           --root DIR          Alternate system root\n  \
           --config-home DIR   User configuration root\n  \
           --runtime-dir DIR   Runtime directory\n  \
         -h, --help              Show help\n  \
         -V, --version           Show version"
    );
}
