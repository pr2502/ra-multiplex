use std::env;

use clap::{Args, Parser, Subcommand};

mod client;
mod server;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// No command defaults to client
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Args, Debug)]
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

#[derive(Args, Debug)]
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Cmd::Server(args)) => server::main(args.dump).await,
        Some(Cmd::Client(args)) => client::main(args.server_path, args.server_args).await,
        None => {
            let server_path = env::var("RA_MUX_SERVER").unwrap_or_else(|_| "rust-analyzer".into());
            client::main(server_path, vec![]).await
        }
    }
}
