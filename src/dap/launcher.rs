//! `microscope dap-launcher`, which a client runs in a terminal to launch a
//! program whose streams belong to that terminal. It waits until the adapter
//! traces it and then executes the program, so the adapter sees the program
//! from its first instruction.
//!
//! It must stay single-threaded, or the exec could happen on a thread the
//! adapter does not trace, so it runs before any async runtime starts.

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

/// Executes the program, returning only when that is impossible.
pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let args = LauncherArgs::parse_from(arguments);
    let error = launch(&args);
    eprintln!(
        "microscope: cannot run {}: {error}",
        args.command[0].to_string_lossy()
    );

    // The shell's status for a command that could not be executed.
    ExitCode::from(127)
}

/// Executes the program once the adapter writes that it traces this
/// process. The socket closes on exec.
fn launch(args: &LauncherArgs) -> io::Error {
    let mut adapter = match UnixStream::connect(&args.connect) {
        Ok(adapter) => adapter,
        Err(error) => return error,
    };

    let mut traced = [0];

    match adapter.read(&mut traced) {
        Ok(1) => {
            // Unrandomized, as a program the adapter launches itself is.
            if let Ok(persona) = personality::get() {
                let _ = personality::set(persona | Persona::ADDR_NO_RANDOMIZE);
            }

            Command::new(&args.command[0])
                .args(&args.command[1..])
                .exec()
        }
        Ok(_) => io::Error::other("the debugger stopped before it could debug it"),
        Err(error) => error,
    }
}
