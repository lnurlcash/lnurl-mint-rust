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
mod whoami;

/// How often melts left pending are looked at again.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
/// How often bitcoind's cookie is checked for a new one.
const COOKIE_POLL_INTERVAL: Duration = Duration::from_secs(5);

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
    // first, before the database or the node: a second mint on this data
    // directory stops here, without touching either
    let socket_path = config.admin_socket();
    let admin_socket = admin::bind_socket(&socket_path)?;
    let database_path = config.database_path();
    let store = db::NoteStore::open(&database_path.to_string_lossy()).map_err(|e| {
        let dir = database_path.parent().unwrap_or(&config.data_dir);
        anyhow::anyhow!(
            "could not open {}: {e}{}",
            database_path.display(),
            whoami::write_hint(dir, &database_path)
        )
    })?;
    log::info!("using database {}", database_path.display());
    let store = std::sync::Arc::new(store);

    let node_config = config.node_config()?;
    // only bitcoind's cookie changes under a running mint
    let cookie = node_config.as_ref().and_then(|n| match &n.chain {
        ln::ChainSource::Bitcoind(ln::BitcoindConfig {
            auth: ln::BitcoindAuth::Cookie(path),
            ..
        }) => Some(path.clone()),
        _ => None,
    });
    let ln = match node_config {
        Some(node) => ln::Ln::start(node, std::sync::Arc::clone(&store)).await?,
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
    // ldk-node keeps the RPC credentials it started with: when bitcoind
    // writes a new cookie, stop cleanly and leave the restart to the
    // supervisor (systemd, docker's restart policy)
    let cookie_changed = std::sync::Arc::new(tokio::sync::Notify::new());
    let restart = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(path) = cookie {
        let (notify, restart) = (cookie_changed.clone(), restart.clone());
        tokio::spawn(async move {
            watch_cookie(&path).await;
            log::warn!(
                "{} changed: bitcoind restarted, shutting down to reconnect with the new cookie",
                path.display()
            );
            restart.store(true, std::sync::atomic::Ordering::SeqCst);
            notify.notify_one();
        });
    }
    let server = tokio::spawn(
        axum::serve(listener, lnurl::router(state.clone()).into_make_service())
            .with_graceful_shutdown(async move {
                tokio::select! {
                    () = shutdown_signal() => {},
                    () = cookie_changed.notified() => {},
                }
            })
            .into_future(),
    );

    // lnurl-mint-cli's way in, always: the API on a socket only this user opens
    log::info!("admin socket at {}", socket_path.display());
    let socket_router = admin::api(state.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(admin_socket, socket_router.into_make_service()).await {
            log::error!("admin socket stopped: {e}");
        }
    });

    if let Some(token) = config.admin_token.clone() {
        let listener = tokio::net::TcpListener::bind(config.admin_listen)
            .await
            .with_context(|| format!("could not listen on {}", config.admin_listen))?;
        log::info!("admin API and web UI on {}", config.admin_listen);
        let router = admin::router(state.clone(), token);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router.into_make_service()).await {
                log::error!("admin API stopped: {e}");
            }
        });
    } else {
        log::info!("admin HTTP API and web UI off: set ADMIN_TOKEN to serve them");
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
    let _ = std::fs::remove_file(&socket_path);
    if restart.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("bitcoind's cookie changed: restart the mint to pick it up");
    }
    log::info!("shut down");
    Ok(())
}

/// Returns once bitcoind's cookie holds something other than at startup. A
/// missing cookie (bitcoind stopped) is waited out.
async fn watch_cookie(path: &std::path::Path) {
    let initial = std::fs::read_to_string(path).unwrap_or_default();
    loop {
        tokio::time::sleep(COOKIE_POLL_INTERVAL).await;
        match std::fs::read_to_string(path) {
            Ok(now) if !now.trim().is_empty() && now != initial => return,
            _ => {}
        }
    }
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
