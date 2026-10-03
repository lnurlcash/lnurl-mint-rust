//! Startup configuration: command-line flags, each also readable from the
//! environment (and a `.env` file) under lnurl-mint's own names.

use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{Context, anyhow, bail};
use clap::Parser;
use lightning::ln::msgs::SocketAddress;

use crate::{
    ln::{BitcoindAuth, BitcoindConfig, Network, NodeConfig},
    state::Settings,
};

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Config {
    /// Public base URL of the mint, e.g. https://mint.example. Its host is the
    /// domain `ck1` signatures bind to.
    #[arg(long, env = "BASE_URL")]
    pub base_url: String,

    /// A Tor base URL. Spends bound to its host are accepted too.
    #[arg(long, env = "ONION_URL")]
    pub onion_url: Option<String>,

    /// HTTP listen address for the LNURL endpoints.
    #[arg(long, env = "LISTEN", default_value = "127.0.0.1:8111")]
    pub listen: SocketAddr,

    /// Listen address for the admin API. Served only when ADMIN_TOKEN is set.
    #[arg(long, env = "ADMIN_LISTEN", default_value = "127.0.0.1:8112")]
    pub admin_listen: SocketAddr,

    /// Bearer token the admin API requires.
    #[arg(long, env = "ADMIN_TOKEN", hide_env_values = true)]
    pub admin_token: Option<String>,

    /// Directory for the seed, the node's state and the note database.
    #[arg(long, env = "DATA_DIR", default_value = "data")]
    pub data_dir: PathBuf,

    /// Note database (default: <DATA_DIR>/mint.sqlite3). Its schema is
    /// lnurl-mint's.
    #[arg(long, env = "DATABASE_PATH")]
    pub database_path: Option<PathBuf>,

    /// bitcoin, testnet, testnet4, signet or regtest.
    #[arg(long, env = "NETWORK", default_value = "bitcoin")]
    pub network: Network,

    /// bitcoind's RPC, host or host:port. Unset, the mint runs without a
    /// Lightning node: rotate, split and merge work, minting and melting don't.
    #[arg(long, env = "BITCOIND_RPC")]
    pub bitcoind_rpc: Option<String>,

    #[arg(long, env = "BITCOIND_RPC_USER")]
    pub bitcoind_rpc_user: Option<String>,

    #[arg(long, env = "BITCOIND_RPC_PASSWORD", hide_env_values = true)]
    pub bitcoind_rpc_password: Option<String>,

    /// bitcoind's .cookie file, instead of a user and password.
    #[arg(long, env = "BITCOIND_RPC_COOKIE")]
    pub bitcoind_rpc_cookie: Option<PathBuf>,

    /// Where the Lightning node accepts peers.
    #[arg(long, env = "LN_LISTEN", default_value = "0.0.0.0:9735")]
    pub ln_listen: SocketAddr,

    /// The node's alias, announced with public channels (default: TITLE).
    #[arg(long, env = "LN_ALIAS")]
    pub ln_alias: Option<String>,

    /// Comma-separated host:port addresses announced with public channels.
    #[arg(long, env = "LN_ANNOUNCE_ADDRESSES", value_delimiter = ',')]
    pub ln_announce_addresses: Vec<String>,

    /// The mint's own Lightning Address username (`_` always works too).
    #[arg(long, env = "USERNAME", default_value = "mint")]
    pub username: String,

    /// Smallest mint payment accepted, in msat.
    #[arg(long, env = "MIN_SENDABLE_MSAT", default_value_t = 10_000)]
    pub min_sendable_msat: u64,

    /// Largest mint payment accepted, in msat.
    #[arg(long, env = "MAX_SENDABLE_MSAT", default_value_t = 1_000_000_000)]
    pub max_sendable_msat: u64,

    /// Flat mint fee in msat, also charged on every split.
    #[arg(long, env = "BASE_FEE_MSAT", default_value_t = 1000)]
    pub base_fee_msat: u64,

    /// Proportional mint fee in parts per million (at most 100000).
    #[arg(long, env = "FEE_PERCENT_PPM", default_value_t = 0)]
    pub fee_percent_ppm: u64,

    /// Smallest value a freshly minted note may have, net of fees.
    #[arg(long, env = "MIN_MINT_MSAT", default_value_t = 10_000)]
    pub min_mint_msat: u64,

    /// Most notes a single callback may name.
    #[arg(long, env = "MAX_K1S", default_value_t = 100)]
    pub max_k1s: usize,

    /// Wind the mint down: refuse mints and splits, keep rotate, merge and melt.
    #[arg(long, env = "SUNSET_MINT", default_value_t = false, action = clap::ArgAction::Set)]
    pub sunset_mint: bool,

    /// Planned shutdown date to advertise (ISO-8601).
    #[arg(long, env = "SUNSET_DATE")]
    pub sunset_date: Option<String>,

    /// Serve LUD-21 /verify for mint invoices and melts.
    #[arg(long, env = "VERIFY_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub verify_enabled: bool,

    /// Let wallets register a Lightning Address against a cx1 branch (LUD-26).
    #[arg(long, env = "USERNAME_REGISTRATION_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub username_registration_enabled: bool,

    /// Serve /.well-known/nostr.json for registered usernames.
    #[arg(long, env = "NIP05_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub nip05_enabled: bool,

    /// Title of the web page.
    #[arg(long, env = "TITLE", default_value = "lnurl-mint")]
    pub title: String,

    /// Description on the web page.
    #[arg(long, env = "DESCRIPTION", default_value = "A minimal LNURLcash mint.")]
    pub description: String,
}

impl Config {
    pub fn database_path(&self) -> PathBuf {
        self.database_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join("mint.sqlite3"))
    }

    /// The Lightning node's configuration, or `None` without bitcoind.
    pub fn node_config(&self) -> anyhow::Result<Option<NodeConfig>> {
        let Some(rpc) = &self.bitcoind_rpc else {
            return Ok(None);
        };
        let (host, port) = match rpc.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().context("BITCOIND_RPC port")?),
            None => (rpc.clone(), self.network.default_rpc_port()),
        };
        let auth = match (
            &self.bitcoind_rpc_cookie,
            &self.bitcoind_rpc_user,
            &self.bitcoind_rpc_password,
        ) {
            (Some(cookie), _, _) => BitcoindAuth::Cookie(cookie.clone()),
            (None, Some(user), Some(password)) => {
                BitcoindAuth::UserPass(user.clone(), password.clone())
            }
            _ => {
                bail!("BITCOIND_RPC needs BITCOIND_RPC_COOKIE, or BITCOIND_RPC_USER and _PASSWORD")
            }
        };
        let announce_addresses = self
            .ln_announce_addresses
            .iter()
            .filter(|a| !a.is_empty())
            .map(|a| {
                SocketAddress::from_str(a).map_err(|_| anyhow!("not an address to announce: {a}"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Some(NodeConfig {
            data_dir: self.data_dir.clone(),
            network: self.network.bitcoin(),
            bitcoind: BitcoindConfig { host, port, auth },
            listen: self.ln_listen,
            alias: self.ln_alias.clone().unwrap_or_else(|| self.title.clone()),
            announce_addresses,
        }))
    }

    pub fn settings(&self) -> anyhow::Result<Settings> {
        let settings = Settings {
            base_url: self.base_url.clone(),
            onion_url: self.onion_url.clone(),
            username: self.username.to_ascii_lowercase(),
            min_sendable_msat: self.min_sendable_msat,
            max_sendable_msat: self.max_sendable_msat,
            base_fee_msat: self.base_fee_msat,
            fee_percent_ppm: self.fee_percent_ppm,
            min_mint_msat: self.min_mint_msat,
            max_k1s: self.max_k1s,
            sunset_mint: self.sunset_mint,
            sunset_date: self.sunset_date.clone(),
            verify_enabled: self.verify_enabled,
            username_registration_enabled: self.username_registration_enabled,
            nip05_enabled: self.nip05_enabled,
            title: self.title.clone(),
            description: self.description.clone(),
        };
        settings.validate()?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_lnurl_mint() {
        let config =
            Config::try_parse_from(["lnurl-mint", "--base-url", "https://mint.example"]).unwrap();
        let s = config.settings().unwrap();
        assert_eq!(s.min_sendable_msat, 10_000);
        assert_eq!(s.base_fee_msat, 1000);
        assert!(s.verify_enabled && s.username_registration_enabled && s.nip05_enabled);
        assert!(!s.sunset_mint);
        assert_eq!(config.database_path(), PathBuf::from("data/mint.sqlite3"));
    }

    #[test]
    fn booleans_take_a_value() {
        let config = Config::try_parse_from([
            "lnurl-mint",
            "--base-url",
            "https://mint.example",
            "--verify-enabled",
            "false",
        ])
        .unwrap();
        assert!(!config.verify_enabled);
    }

    #[test]
    fn the_node_needs_credentials() {
        let parse = |args: &[&str]| {
            let mut all = vec!["lnurl-mint", "--base-url", "https://mint.example"];
            all.extend_from_slice(args);
            Config::try_parse_from(all).unwrap().node_config()
        };
        assert!(parse(&[]).unwrap().is_none());
        assert!(parse(&["--bitcoind-rpc", "127.0.0.1"]).is_err());
        let node = parse(&[
            "--network",
            "regtest",
            "--bitcoind-rpc",
            "127.0.0.1",
            "--bitcoind-rpc-cookie",
            "/tmp/.cookie",
            "--ln-announce-addresses",
            "203.0.113.1:9735,mint.example:9735",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(node.bitcoind.port, 18443);
        assert_eq!(node.announce_addresses.len(), 2);
        assert_eq!(node.alias, "lnurl-mint");
    }

    #[test]
    fn a_bad_base_url_is_refused() {
        let config = Config::try_parse_from(["lnurl-mint", "--base-url", "not a url"]).unwrap();
        assert!(config.settings().is_err());
    }
}
