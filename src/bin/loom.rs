// SPDX-License-Identifier: BSD-2-Clause

use std::{env, path::PathBuf, process::ExitCode};

use lexopt::prelude::*;
use loom::{
    linux::shutdown_system,
    manager::{Manager, ManagerMode, ManagerOptions, ShutdownAction},
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
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
    while let Some(argument) = parser.next()? {
        match argument {
            Long("user") => mode = ManagerMode::User,
            Long("system") => mode = ManagerMode::System,
            Long("root") => root = parser.value()?.into(),
            Long("config-home") => config_home = Some(PathBuf::from(parser.value()?)),
            Long("runtime-dir") => runtime_dir = Some(PathBuf::from(parser.value()?)),
            Long("help") | Short('h') => {
                print_help();
                return Ok(());
            }
            Long("version") | Short('V') => {
                println!("loom {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => return Err(argument.unexpected().into()),
        }
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

    let action = Manager::new(options)?.run()?;
    if mode == ManagerMode::System && std::process::id() == 1 {
        match action {
            ShutdownAction::Reboot => shutdown_system(true)?,
            ShutdownAction::Poweroff => shutdown_system(false)?,
            ShutdownAction::Exit => {}
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "Loom init and service manager\n\n\
         Usage: loom [--system|--user] [OPTIONS]\n\n\
         Options:\n  \
           --root DIR          Alternate system root\n  \
           --config-home DIR   User configuration root\n  \
           --runtime-dir DIR   Runtime directory\n  \
         -h, --help              Show help\n  \
         -V, --version           Show version"
    );
}
