use clap::{Args, Parser, Subcommand};

mod client;
mod server;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the LSP server executable
    #[arg(
        long,
        alias = "ra-mux-server",
        env = "RA_MUX_SERVER",
        default_value = "rust-analyzer"
    )]
    server_path: String,

    /// No command defaults to client
    #[command(subcommand)]
    command: Cmd,

    /// Arguments passed to the LSP server
    rest: Vec<String>,
}

#[derive(Args, Debug)]
struct ServerArgs {
    /// Dump all communication with the client
    #[arg(long)]
    dump: bool,
}

#[derive(Subcommand, Debug, Default)]
enum Cmd {
    #[default]
    /// Connect to a ra-mux server
    Client,

    /// Start a ra-mux server
    Server(ServerArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Client => client::main(cli.server_path, cli.rest).await,
        Cmd::Server(args) => server::main(args.dump).await,
    }
}
