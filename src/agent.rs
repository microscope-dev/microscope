use std::{path::PathBuf, process::ExitCode};

pub mod client;
pub mod cloud;
pub mod config;
pub mod control;
pub mod host;
pub mod inventory;
pub mod machine;
pub mod network;
pub mod poster;
pub mod recorder;
pub mod run;
#[cfg(test)]
mod scratch;
pub mod select;
pub mod split;
pub mod spool;
pub mod store;
pub mod symbols;
pub mod wire;

/// Registers this host with a Microscope server and keeps it registered.
#[derive(clap::Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: AgentCommand,
}

#[derive(clap::Subcommand)]
pub enum AgentCommand {
    /// Enroll this host, then refresh until SIGTERM or SIGINT. SIGHUP
    /// rereads the config.
    Run {
        /// Read FILE instead of /etc/microscope/agent.toml. MICROSCOPE_*
        /// environment variables override it.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },

    /// Print the server, agent id, key fingerprint, state, last refresh,
    /// and last error, from what the agent last wrote. Makes no network
    /// request. Exits 3 when no agent is running.
    Status {
        /// Read FILE instead of /etc/microscope/agent.toml.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,

        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[must_use]
pub fn main(args: &AgentArgs) -> ExitCode {
    let result = match &args.command {
        AgentCommand::Run { config } => run::run(config.as_deref()),
        AgentCommand::Status { config, json } => run::status(config.as_deref(), *json),
    };
    result.unwrap_or_else(|error| {
        eprintln!("error: {error:#}");
        ExitCode::from(2)
    })
}
