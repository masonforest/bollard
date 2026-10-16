mod api;
mod apply;
mod bitcoin_watch;
mod burn_watch;
mod chain;
mod client;
mod config;
mod crypto;
mod db;
mod enter;
mod fb;
mod fbio;
mod ledger;
mod lottery;
mod node;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "bollard", version, about = "Bollard: a cross-chain order book finalized on Bitcoin")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a node. It follows by default and leads when its lottery entry wins.
    Run(config::RunArgs),
    /// Generate a key. The same key is a node key, a Bollard account and a Bitcoin address.
    Keygen,
    /// Broadcast a lottery entry on Bitcoin carrying this node's address and log tip.
    Enter(enter::EnterArgs),
    /// Build, sign and submit a transaction to a node.
    Tx(client::TxArgs),
    /// Print an account's keyless burn addresses: send coins there to bring them onto Bollard.
    BurnAddress {
        /// The account's public key, hex (33 bytes secp256k1 or 65 bytes P-256)
        #[arg(long)]
        account: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,hyper=warn".into()))
        .init();
    match Cli::parse().cmd {
        Cmd::Run(args) => node::run(args).await,
        Cmd::Keygen => client::keygen(),
        Cmd::Enter(args) => enter::enter(args).await,
        Cmd::Tx(args) => client::tx(args).await,
        Cmd::BurnAddress { account } => {
            let pk = hex::decode(account)?;
            anyhow::ensure!(crypto::valid_account(&pk), "not an account key");
            println!("bitcoin  {}", chain::burn::btc(&pk));
            println!("ethereum {}", chain::burn::eth(&pk));
            Ok(())
        }
    }
}
