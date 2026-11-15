use std::ffi::OsString;
use std::io::{self, Read as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use clap::Parser;
use nix::sys::personality::{self, Persona};

/// Runs a program in a terminal for the microscope debug adapter.
#[derive(Parser)]
#[command(name = "microscope dap-launcher")]
struct LauncherArgs {
    /// The adapter's socket, which identifies this process by its peer
    /// credentials.
    #[arg(long, value_name = "SOCKET")]
    connect: PathBuf,

    /// The program and its arguments.
    #[arg(last = true, required = true, value_name = "PROGRAM")]
    command: Vec<OsString>,
}

pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let args = LauncherArgs::parse_from(arguments);
    let error = launch(&args);
    eprintln!(
        "microscope: cannot run {}: {error}",
        args.command
            .first()
            .map_or(std::borrow::Cow::Borrowed("the program"), |program| program
                .to_string_lossy())
    );

    ExitCode::from(127)
}

fn launch(args: &LauncherArgs) -> io::Error {
    let mut adapter = match UnixStream::connect(&args.connect) {
        Ok(adapter) => adapter,
        Err(error) => return error,
    };

    let mut traced = [0];

    match adapter.read(&mut traced) {
        Ok(1) => {
            if let Ok(persona) = personality::get() {
                let _ = personality::set(persona | Persona::ADDR_NO_RANDOMIZE);
            }

            let Some((program, arguments)) = args.command.split_first() else {
                return io::Error::other("no program was given");
            };
            Command::new(program).args(arguments).exec()
        }
        Ok(_) => io::Error::other("the debugger stopped before it could debug it"),
        Err(error) => error,
    }
}
