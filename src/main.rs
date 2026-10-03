//! lnurl-mint: an LNURLcash mint (LUD-25 bearer notes, LUD-26 derivation and
//! Lightning Address auto-mint) that is its own Lightning node.

use std::time::Duration;

use anyhow::Context;
use clap::Parser;

mod admin;
mod admin_ui;
mod config;
mod db;
mod ln;
mod lnurl;
mod mint;
mod spend;
mod state;
#[cfg(test)]
mod vectors;

/// How often melts left pending are looked at again.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // a missing .env is fine; a malformed one is not
    if let Err(e) = dotenvy::dotenv() {
        if !e.not_found() {
            return Err(e).context("could not read .env");
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = config::Config::parse();
    let settings = config.settings()?;

    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("could not create {}", config.data_dir.display()))?;
    let database_path = config.database_path();
    let store = db::NoteStore::open(&database_path.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", database_path.display()))?;
    log::info!("using database {}", database_path.display());
    let store = std::sync::Arc::new(store);

    let ln = match config.node_config()? {
        Some(node) => ln::Ln::start(config.network, node, std::sync::Arc::clone(&store)).await?,
        None => {
            log::warn!(
                "no BITCOIND_RPC: running without a Lightning node, minting and melting are unavailable"
            );
            ln::Ln::new(config.network)
        }
    };
    let state = state::AppState::new(settings, store, ln);

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("could not listen on {}", config.listen))?;
    let (base, host) = state.settings.public_base_url_and_host(None);
    log::info!(
        "serving on {} as {base}, address {}@{host}",
        config.listen,
        state.settings.username
    );
    let server = tokio::spawn(
        axum::serve(listener, lnurl::router(state.clone()).into_make_service())
            .with_graceful_shutdown(shutdown_signal())
            .into_future(),
    );

    if let Some(token) = config.admin_token.clone() {
        let listener = tokio::net::TcpListener::bind(config.admin_listen)
            .await
            .with_context(|| format!("could not listen on {}", config.admin_listen))?;
        log::info!("admin API on {}", config.admin_listen);
        let router = admin::router(state.clone(), token);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router.into_make_service()).await {
                log::error!("admin API stopped: {e}");
            }
        });
    } else {
        log::info!("admin API off: set ADMIN_TOKEN to serve it");
    }

    let reconciler_state = state.clone();
    tokio::spawn(async move {
        loop {
            if reconciler_state.ln.ready().is_ok() {
                if let Err(e) = reconciler_state.reconcile_pending_melts() {
                    log::warn!("reconciling pending melts failed: {}", e.reason());
                }
            }
            tokio::time::sleep(RECONCILE_INTERVAL).await;
        }
    });

    server.await.context("server task panicked")??;
    state.ln.stop().await;
    log::info!("shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
