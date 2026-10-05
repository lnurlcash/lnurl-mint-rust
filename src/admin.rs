//! The operator's API, on a separate listen address: cln-mint's RPC methods
//! as HTTP, and running the node (addresses, channels, balances, payments).
//! Bodies are JSON. Authenticated by `Authorization: Bearer <ADMIN_TOKEN>`,
//! or by a session from the web UI's login (see `admin_ui`), served at `/`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use lnurlcash_core::recoverable::encode_cx1;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    admin_ui::{self, Auth},
    spend,
    state::AppState,
};

pub fn router(state: AppState, token: String) -> Router {
    let auth = Arc::new(Auth::new(token));
    let api = Router::new()
        .route("/info", get(info))
        .route("/note/{note}", get(note))
        .route("/pending", get(pending))
        .route("/reconcile", post(reconcile))
        .route("/users", get(users))
        .route("/node/balance", get(balance))
        .route("/node/address", post(new_address))
        .route("/node/peers", get(peers).post(connect))
        .route("/node/channels", get(channels).post(open_channel))
        .route("/node/channels/close", post(close_channel))
        .route("/node/invoice", post(invoice))
        .route("/node/invoice/{payment_hash}", get(invoice_status))
        .route("/node/pay", post(pay))
        .route("/node/payment/{payment_hash}", get(payment))
        .route("/node/send", post(send))
        .route("/qr", get(admin_ui::qr))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&auth),
            admin_ui::require_auth,
        ))
        .with_state(state);
    // the page and its login are public; everything they call is not
    let ui = Router::new()
        .route("/", get(admin_ui::index))
        .route("/admin.js", get(admin_ui::script))
        .route("/admin.css", get(admin_ui::stylesheet))
        .route("/login", post(admin_ui::login))
        .route("/logout", post(admin_ui::logout))
        .with_state(auth);
    api.merge(ui)
        .layer(middleware::from_fn(admin_ui::security_headers))
}

type AdminResult = Result<Json<Value>, (StatusCode, String)>;

fn internal(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// A node operation that failed: the operator gets the whole reason.
fn refused(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("{err:#}"))
}

async fn info(State(state): State<AppState>) -> AdminResult {
    let s = &state.settings;
    let (base, host) = s.public_base_url_and_host(None);
    Ok(Json(json!({
        "base_url": base,
        "onion_url": s.onion_url,
        "lightning_address": format!("{}@{host}", s.username),
        "lnurl": lnurlcash_core::to_bech32_lnurl(&format!("{base}/.well-known/lnurlp/{}", s.username))
            .map(|l| l.to_ascii_uppercase()),
        "withdraw_link": format!("{base}/w"),
        "spend_domains": s.spend_domains(),
        "mint_pubkey": state.mint_pubkey(),
        "lightning": state.ln.ready().err().map(|e| e.to_string()).unwrap_or_else(|| "ready".into()),
        "min_sendable_msat": s.min_sendable(),
        "max_sendable_msat": s.max_sendable_msat,
        "base_fee_msat": s.base_fee_msat,
        "fee_percent_ppm": s.fee_percent_ppm,
        "sunset_mint": s.sunset_mint,
        "stats": state.store.stats().map_err(internal)?,
    })))
}

async fn note(State(state): State<AppState>, Path(note): Path<String>) -> AdminResult {
    let note_id = spend::note_id_of_ref(&note).ok_or((
        StatusCode::BAD_REQUEST,
        "not a cp1 or a bearer note's hash".to_string(),
    ))?;
    let Some(record) = state.store.note_record(&note_id).map_err(internal)? else {
        let unpaid_mint = state
            .store
            .pending_mint_by_note_id(&note_id)
            .map_err(internal)?;
        return Ok(Json(
            json!({"note_id": note_id, "status": "unknown", "unpaid_mint": unpaid_mint}),
        ));
    };
    let status = match (record.spent, record.pending) {
        (true, _) => "spent",
        (false, true) => "pending",
        (false, false) => "outstanding",
    };
    Ok(Json(json!({
        "note_id": note_id,
        "status": status,
        "amount_msat": record.amount_msat,
        "locked_at": record.locked_at,
    })))
}

async fn pending(State(state): State<AppState>) -> AdminResult {
    let melts = state.store.pending_melts().map_err(internal)?;
    Ok(Json(json!({"pending_melts": melts})))
}

async fn reconcile(State(state): State<AppState>) -> AdminResult {
    state
        .reconcile_pending_melts()
        .map(Json)
        .map_err(|e| internal(e.reason()))
}

async fn users(State(state): State<AppState>) -> AdminResult {
    let users: Vec<Value> = state
        .store
        .list_usernames()
        .map_err(internal)?
        .into_iter()
        .map(|(username, branch_hex, next_index)| {
            let cx1 = hex::decode(&branch_hex)
                .ok()
                .filter(|b| b.len() == 64)
                .map(|b| encode_cx1(b[..32].try_into().unwrap(), b[32..].try_into().unwrap()));
            json!({"username": username, "cx1": cx1, "next_index": next_index})
        })
        .collect();
    Ok(Json(json!({"users": users})))
}

// ---- the node ----

async fn balance(State(state): State<AppState>) -> AdminResult {
    state.ln.balance().map(Json).map_err(refused)
}

async fn new_address(State(state): State<AppState>) -> AdminResult {
    let address = state.ln.new_address().map_err(refused)?;
    Ok(Json(json!({"address": address})))
}

async fn peers(State(state): State<AppState>) -> AdminResult {
    state.ln.peers().map(Json).map_err(refused)
}

#[derive(Deserialize)]
struct Connect {
    /// pubkey@host:port
    peer: String,
}

async fn connect(State(state): State<AppState>, Json(req): Json<Connect>) -> AdminResult {
    let id = state.ln.connect(&req.peer).await.map_err(refused)?;
    Ok(Json(json!({"connected": id})))
}

async fn channels(State(state): State<AppState>) -> AdminResult {
    state.ln.channels().map(Json).map_err(refused)
}

#[derive(Deserialize)]
struct OpenChannel {
    /// pubkey, or pubkey@host:port to connect first
    peer: String,
    amount_sat: u64,
    #[serde(default)]
    public: bool,
}

async fn open_channel(State(state): State<AppState>, Json(req): Json<OpenChannel>) -> AdminResult {
    let channel_id = state
        .ln
        .open_channel(&req.peer, req.amount_sat, req.public)
        .await
        .map_err(refused)?;
    Ok(Json(json!({"channel_id": channel_id})))
}

#[derive(Deserialize)]
struct CloseChannel {
    channel_id: String,
    #[serde(default)]
    force: bool,
}

async fn close_channel(
    State(state): State<AppState>,
    Json(req): Json<CloseChannel>,
) -> AdminResult {
    state
        .ln
        .close_channel(&req.channel_id, req.force)
        .map_err(refused)?;
    Ok(Json(json!({"status": "closing"})))
}

#[derive(Deserialize)]
struct NewInvoice {
    amount_msat: u64,
    #[serde(default)]
    description: String,
}

/// An invoice paying the node itself, crediting no note: to take in
/// liquidity, or to test a route.
async fn invoice(State(state): State<AppState>, Json(req): Json<NewInvoice>) -> AdminResult {
    let invoice = state
        .ln
        .operator_invoice(req.amount_msat, &req.description)
        .map_err(refused)?;
    Ok(Json(
        json!({"bolt11": invoice.bolt11, "payment_hash": invoice.payment_hash}),
    ))
}

async fn invoice_status(
    State(state): State<AppState>,
    Path(payment_hash): Path<String>,
) -> AdminResult {
    match state
        .store
        .operator_invoice_paid(&payment_hash)
        .map_err(internal)?
    {
        Some(paid) => Ok(Json(json!({"paid": paid}))),
        None => Err((StatusCode::NOT_FOUND, "no such operator invoice".into())),
    }
}

#[derive(Deserialize)]
struct Pay {
    bolt11: String,
    max_fee_msat: Option<u64>,
}

/// Pay from the node's own liquidity. The outcome arrives later: poll
/// `/node/payment/<hash>`.
async fn pay(State(state): State<AppState>, Json(req): Json<Pay>) -> AdminResult {
    let invoice = state.ln.decode_invoice(&req.bolt11).map_err(refused)?;
    let amount = invoice
        .amount_msat
        .ok_or_else(|| refused(anyhow::anyhow!("the invoice names no amount")))?;
    let max_fee = req
        .max_fee_msat
        .unwrap_or_else(|| state.settings.melt_fee_limit_msat(amount));
    state.ln.pay(&req.bolt11, max_fee).map_err(|e| match e {
        crate::ln::PayError::NotSent(reason) => refused(anyhow::anyhow!(reason)),
        crate::ln::PayError::InFlight => refused(anyhow::anyhow!("already in flight")),
    })?;
    Ok(Json(json!({"payment_hash": invoice.payment_hash})))
}

async fn payment(State(state): State<AppState>, Path(payment_hash): Path<String>) -> AdminResult {
    let status = state.ln.payment_status(&payment_hash).map_err(refused)?;
    Ok(Json(
        json!({"status": format!("{status:?}").to_ascii_lowercase()}),
    ))
}

#[derive(Deserialize)]
struct Send {
    address: String,
    /// Everything the wallet holds when absent.
    amount_sat: Option<u64>,
}

async fn send(State(state): State<AppState>, Json(req): Json<Send>) -> AdminResult {
    let txid = state
        .ln
        .send_onchain(&req.address, req.amount_sat)
        .map_err(refused)?;
    Ok(Json(json!({"txid": txid})))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, Response, header},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        db::NoteStore,
        ln::{Ln, Network},
        state::test_settings,
    };

    const HOST: &str = "127.0.0.1:8112";

    fn app() -> Router {
        let state = AppState::new(
            test_settings(),
            std::sync::Arc::new(NoteStore::in_memory().unwrap()),
            Ln::new(Network::Regtest),
        );
        router(state, "s3cret".into())
    }

    async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
        app.clone().oneshot(req).await.unwrap()
    }

    fn get(path: &str) -> axum::http::request::Builder {
        Request::builder().uri(path).header(header::HOST, HOST)
    }

    fn post(path: &str, origin: Option<&str>) -> axum::http::request::Builder {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json");
        match origin {
            Some(origin) => req.header(header::ORIGIN, origin),
            None => req,
        }
    }

    async fn status(auth: Option<&str>) -> StatusCode {
        let mut req = get("/pending");
        if let Some(auth) = auth {
            req = req.header(header::AUTHORIZATION, auth);
        }
        send(&app(), req.body(Body::empty()).unwrap())
            .await
            .status()
    }

    #[tokio::test]
    async fn the_token_is_required() {
        assert_eq!(status(None).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(Some("Bearer nope")).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(Some("s3cret")).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(Some("Bearer s3cret")).await, StatusCode::OK);
    }

    /// Log in from the page's own origin; the session cookie it sets.
    async fn login(app: &Router, token: &str) -> Result<String, StatusCode> {
        let res = send(
            app,
            post("/login", Some("http://127.0.0.1:8112"))
                .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                .unwrap(),
        )
        .await;
        if res.status() != StatusCode::OK {
            return Err(res.status());
        }
        let cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        Ok(cookie.split(';').next().unwrap().to_string())
    }

    #[tokio::test]
    async fn a_session_from_the_login_works_like_the_token() {
        let app = app();
        assert_eq!(login(&app, "nope").await, Err(StatusCode::UNAUTHORIZED));
        let cookie = login(&app, "s3cret").await.unwrap();

        let res = send(
            &app,
            get("/pending")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        // a cookie that was never issued opens nothing
        let forged = get("/pending")
            .header(header::COOKIE, "mint_admin=00")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, forged).await.status(), StatusCode::UNAUTHORIZED);

        // a change needs the page's own origin
        let reconcile = |origin| {
            post("/reconcile", origin)
                .header(header::COOKIE, &cookie)
                .body(Body::from("{}"))
                .unwrap()
        };
        assert_eq!(
            send(&app, reconcile(Some("http://127.0.0.1:8112")))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            send(&app, reconcile(Some("https://evil.example")))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(&app, reconcile(None)).await.status(),
            StatusCode::FORBIDDEN
        );

        // logging out ends the session
        let out = post("/logout", Some("http://127.0.0.1:8112"))
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        assert!(
            send(&app, out).await.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        let res = send(
            &app,
            get("/pending")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_login_from_another_origin_is_refused() {
        let res = send(
            &app(),
            post("/login", Some("https://evil.example"))
                .body(Body::from(r#"{"token":"s3cret"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn the_page_is_public_and_locked_down() {
        let app = app();
        for (path, kind) in [
            ("/", "text/html"),
            ("/admin.js", "text/javascript"),
            ("/admin.css", "text/css"),
        ] {
            let res = send(&app, get(path).body(Body::empty()).unwrap()).await;
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            let headers = res.headers();
            assert!(
                headers[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with(kind)
            );
            let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
            assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
        // the API behind it is not
        let res = send(&app, get("/info").body(Body::empty()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        // and QR codes need a session too
        let res = send(&app, get("/qr?data=bc1q").body(Body::empty()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn qr_codes_are_svg() {
        let req = get("/qr?data=bitcoin%3Abcrt1qtest")
            .header(header::AUTHORIZATION, "Bearer s3cret")
            .body(Body::empty())
            .unwrap();
        let res = send(&app(), req).await;
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/svg+xml");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(body.starts_with(b"<?xml") || body.windows(4).any(|w| w == b"<svg"));
    }
}
