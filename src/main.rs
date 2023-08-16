use std::env;

use clap::{Parser, Subcommand};

mod client;
mod server;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    after_help = "\
The use of the `client` and `server` subcommands can be forced through
`RA_MUX_RUN_CLIENT=1` and `RA_MUX_RUN_SERVER=true` or any other non-falsey
value respectively. When the subcommand is set this way any args are treated as
though they are passed to this subcommand."
)]
struct Cli {
    /// No command defaults to client
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Parser, Debug, Default)]
struct ClientArgs {
    /// Path to the LSP server executable
    #[arg(
        long,
        alias = "ra-mux-server",
        env = "RA_MUX_SERVER",
        default_value = "rust-analyzer"
    )]
    server_path: String,

    /// Arguments passed to the LSP server
    server_args: Vec<String>,
}

#[derive(Parser, Debug)]
struct ServerArgs {
    /// Dump all communication with the client
    #[arg(long)]
    dump: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Connect to a ra-mux server
    Client(ClientArgs),

    /// Start a ra-mux server
    Server(ServerArgs),
}

impl Default for Cmd {
    fn default() -> Self {
        Cmd::Client(ClientArgs::default())
    }
}

fn maybe_var_truthy(var: &str) -> anyhow::Result<Option<bool>> {
    let maybe_bool = match env::var(var) {
        // Matches the truthy values from `clap::builder::BoolishValueParser`
        Ok(val) => Some(["y", "yes", "t", "true", "on", "1"].contains(&&*val)),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(val)) => {
            let lossy_val = val.to_string_lossy().into_owned();
            anyhow::bail!(
                "env var '{}' contained invalid unicode '{}'",
                var,
                lossy_val
            )
        }
    };

    Ok(maybe_bool)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    const CLIENT_ENV_VAR: &str = "RA_MUX_RUN_CLIENT";
    const SERVER_ENV_VAR: &str = "RA_MUX_RUN_SERVER";

    // Allow for running either the client or server through env vars, so that users can run them
    // from the unified binary through setting `rust-analyzer.server.extraEnv`
    let force_run_client = Some(true) == maybe_var_truthy(CLIENT_ENV_VAR)?;
    let force_run_server = Some(true) == maybe_var_truthy(SERVER_ENV_VAR)?;

    let command = match (force_run_client, force_run_server) {
        (true, true) => anyhow::bail!(
            "Only one of {} and {} can be set truthy, but both are",
            CLIENT_ENV_VAR,
            SERVER_ENV_VAR
        ),
        (true, false) => {
            let args = ClientArgs::parse();
            Cmd::Client(args)
        }
        (false, true) => {
            let args = ServerArgs::parse();
            Cmd::Server(args)
        }
        (false, false) => Cli::parse().command,
    };

    match command {
        Cmd::Client(args) => client::main(args.server_path, args.server_args).await,
        Cmd::Server(args) => server::main(args.dump).await,
    }
}
