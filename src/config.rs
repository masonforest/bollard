use clap::Args;

#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Postgres connection string
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,
    /// Node secret key, 32 bytes hex (see `bollard keygen`)
    #[arg(long, env = "BOLLARD_KEY")]
    pub key: String,
    /// Address to listen on
    #[arg(long, env = "LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: String,
    /// Public IP other nodes reach this node at (goes into lottery entries)
    #[arg(long, env = "PUBLIC_IP")]
    pub public_ip: Option<String>,
    /// Public port other nodes reach this node at
    #[arg(long, env = "PUBLIC_PORT", default_value_t = 8080)]
    pub public_port: u16,
    /// Bitcoin JSON-RPC endpoint
    #[arg(long, env = "BITCOIN_RPC", default_value = "https://bitcoin.publicnode.com")]
    pub bitcoin_rpc: String,
    /// Ethereum JSON-RPC endpoint
    #[arg(long, env = "ETH_RPC", default_value = "https://ethereum-rpc.publicnode.com")]
    pub eth_rpc: String,
    /// Bitcoin height the network starts at. The genesis leader leads this epoch.
    #[arg(long, env = "GENESIS_HEIGHT")]
    pub genesis_height: u32,
    /// Genesis leader's public key, 33 bytes hex
    #[arg(long, env = "GENESIS_LEADER")]
    pub genesis_leader: String,
    /// Where to reach the genesis leader, e.g. http://1.2.3.4:8080 (not needed if this node is it)
    #[arg(long, env = "GENESIS_LEADER_URL")]
    pub genesis_leader_url: Option<String>,
    /// Seconds between Bitcoin polls
    #[arg(long, env = "POLL_SECS", default_value_t = 10)]
    pub poll_secs: u64,
    /// Seconds between settlement checks when leading
    #[arg(long, env = "SETTLE_SECS", default_value_t = 15)]
    pub settle_secs: u64,
    /// Serve this directory of static files (the web/ folder) alongside the API
    #[arg(long, env = "WEB_DIR")]
    pub web_dir: Option<String>,
}
