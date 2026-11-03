//! The HTTP side: LNURL endpoints, always answering 200 with LUD-01's
//! `{"status": "ERROR", "reason"}` on failure.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

use crate::{
    mint::{MintError, MintResult},
    state::AppState,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(frontend))
        // LUD-26 registers at the very address it claims
        .route(
            "/.well-known/lnurlp/{username}",
            get(lnurlp)
                .post(register_username)
                .put(update_username)
                .delete(unregister_username)
                .options(preflight),
        )
        .route("/.well-known/lnurlw/{username}", get(lnurlw))
        .route("/.well-known/nostr.json", get(nostr_json))
        .route("/p/cb", get(pay_callback))
        .route("/p/{username}", get(pay_callback_for_username))
        .route("/verify/{payment_hash}", get(verify))
        .route("/w", get(withdraw_request))
        .route("/w/cb", get(withdraw_callback))
        .with_state(state)
}

fn respond(result: MintResult<Value>) -> Response {
    let body = result.unwrap_or_else(|e| json!({"status": "ERROR", "reason": e.reason()}));
    ([(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], Json(body)).into_response()
}

/// The CORS preflight a browser wallet's PUT or DELETE needs (LUD-26).
async fn preflight() -> Response {
    (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "GET, POST, PUT, DELETE, OPTIONS",
            ),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type"),
        ],
    )
        .into_response()
}

fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|h| h.to_str().ok())
}

fn owned_host(headers: &HeaderMap) -> Option<String> {
    request_host(headers).map(str::to_string)
}

/// Run `f` on the blocking pool: the store's lock, SQLite's fsyncs, the node's
/// calls and Bitcoin Core's interpreter must never hold an async worker, or
/// every other request waits behind them.
async fn blocking(f: impl FnOnce() -> MintResult<Value> + Send + 'static) -> MintResult<Value> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(MintError::Internal(e.into())))
}

/// The query as ordered pairs: `k1` repeats for a merge.
fn query_pairs(query: Option<String>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// A registration's parameters: the `application/x-www-form-urlencoded`
/// body LUD-26 sends them in, then the query.
fn form_pairs(body: &[u8], query: Option<String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(body).into_owned().collect();
    pairs.extend(query_pairs(query));
    pairs
}

fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn amount_param(pairs: &[(String, String)], name: &str) -> MintResult<Option<u64>> {
    match param(pairs, name) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| MintError::Reject(format!("Invalid {name}."))),
    }
}

fn required<'a>(pairs: &'a [(String, String)], name: &str) -> MintResult<&'a str> {
    param(pairs, name).ok_or_else(|| MintError::Reject(format!("Missing {name}.")))
}

async fn lnurlp(
    State(state): State<AppState>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Response {
    let host = owned_host(&headers);
    respond(blocking(move || state.pay_request(&username, host.as_deref())).await)
}

async fn lnurlw(
    State(state): State<AppState>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Response {
    let host = owned_host(&headers);
    respond(blocking(move || state.mint_address(&username, host.as_deref())).await)
}

async fn nostr_json(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let pairs = query_pairs(query);
    respond(blocking(move || state.nip05(param(&pairs, "name"))).await)
}

async fn pay_callback(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let pairs = query_pairs(query);
    let host = owned_host(&headers);
    respond(blocking(move || pay(&state, None, &pairs, host.as_deref())).await)
}

async fn pay_callback_for_username(
    State(state): State<AppState>,
    Path(username): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let pairs = query_pairs(query);
    let host = owned_host(&headers);
    respond(blocking(move || pay(&state, Some(&username), &pairs, host.as_deref())).await)
}

fn pay(
    state: &AppState,
    username: Option<&str>,
    pairs: &[(String, String)],
    host: Option<&str>,
) -> MintResult<Value> {
    if param(pairs, "nostr").is_some() {
        return Err(MintError::Reject(
            "Zaps are not offered for this address.".into(),
        ));
    }
    let amount = amount_param(pairs, "amount")?
        .ok_or_else(|| MintError::Reject("Missing amount.".into()))?;
    // a note in place of an invoice: LUD-26's internal transfer
    if let (Some(username), Some(k1)) = (username, param(pairs, "k1")) {
        return state.internal_transfer(username, amount, k1, host);
    }
    state.pay_callback(username, amount, param(pairs, "comment"), host)
}

async fn register_username(
    State(state): State<AppState>,
    Path(username): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body, query);
    let host = owned_host(&headers);
    respond(
        blocking(move || {
            state.register(
                &username,
                required(&pairs, "cx1")?,
                required(&pairs, "sig")?,
                param(&pairs, "npub"),
                host.as_deref(),
            )
        })
        .await,
    )
}

async fn update_username(
    State(state): State<AppState>,
    Path(username): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body, query);
    let host = owned_host(&headers);
    respond(
        blocking(move || {
            state.update(
                &username,
                required(&pairs, "cx1")?,
                required(&pairs, "sig")?,
                required(&pairs, "newsig")?,
                host.as_deref(),
            )
        })
        .await,
    )
}

async fn unregister_username(
    State(state): State<AppState>,
    Path(username): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pairs = form_pairs(&body, query);
    let host = owned_host(&headers);
    respond(
        blocking(move || {
            required(&pairs, "sig")
                .and_then(|sig| state.unregister(&username, sig, host.as_deref()))
        })
        .await,
    )
}

async fn verify(State(state): State<AppState>, Path(payment_hash): Path<String>) -> Response {
    respond(blocking(move || state.verify(&payment_hash.to_ascii_lowercase())).await)
}

async fn withdraw_request(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    // `amount` and `c` ride along on a note URL; they are ignored here
    let pairs = query_pairs(query);
    let host = owned_host(&headers);
    respond(
        blocking(move || {
            state.withdraw_request(param(&pairs, "k1"), param(&pairs, "p"), host.as_deref())
        })
        .await,
    )
}

async fn withdraw_callback(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let pairs = query_pairs(query);
    let host = owned_host(&headers);
    let result = blocking(move || {
        if param(&pairs, "p").is_some() {
            return Err(MintError::Reject(
                "p is only accepted at the informational endpoint.".into(),
            ));
        }
        let k1s: Vec<String> = pairs
            .iter()
            .filter(|(k, _)| k == "k1")
            .map(|(_, v)| v.clone())
            .collect();
        state.withdraw_callback(
            &k1s,
            param(&pairs, "pr"),
            amount_param(&pairs, "amount")?,
            param(&pairs, "p1"),
            param(&pairs, "p2"),
            host.as_deref(),
        )
    })
    .await;
    respond(result)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Two overlapping sheets: the copy button's icon.
const COPY_ICON: &str = "<svg viewBox=\"0 0 24 24\" fill=none stroke=currentColor stroke-width=2 \
     stroke-linecap=round stroke-linejoin=round aria-hidden=true><rect x=9 y=9 width=13 height=13 rx=\"2\"/>\
     <path d=\"M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1\"/></svg>";

/// A one-page frontend: the mint's Lightning Address and LNURL as a QR code,
/// and the node's connection strings.
async fn frontend(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let s = &state.settings;
    let (base, host) = s.public_base_url_and_host(request_host(&headers));
    let address = format!("{}@{host}", s.username);
    let lnurl =
        lnurlcash_core::to_bech32_lnurl(&format!("{base}/.well-known/lnurlp/{}", s.username))
            .map(|l| l.to_ascii_uppercase())
            .unwrap_or_default();
    let qr = qrcode::QrCode::new(format!("lightning:{lnurl}").as_bytes())
        .map(|code| {
            code.render::<qrcode::render::svg::Color>()
                .min_dimensions(240, 240)
                .dark_color(qrcode::render::svg::Color("#111"))
                .light_color(qrcode::render::svg::Color("#fff"))
                .build()
        })
        .unwrap_or_default();
    let blocking_state = state.clone();
    let (outstanding, uris) = tokio::task::spawn_blocking(move || {
        let outstanding = blocking_state
            .store
            .stats()
            .map_or(0, |st| st.outstanding_msat / 1000);
        (outstanding, blocking_state.ln.uris())
    })
    .await
    .unwrap_or_default();
    let node = if uris.is_empty() {
        String::new()
    } else {
        let rows: String = uris
            .iter()
            .map(|uri| {
                let uri = escape(uri);
                format!(
                    "<p class=uri><code>{uri}</code><button type=button class=copy \
                     data-copy=\"{uri}\" title=Copy aria-label=\"Copy connection string\">{COPY_ICON}</button></p>"
                )
            })
            .collect();
        format!("<h2>Connect to the node</h2>{rows}")
    };
    let mut fees = if s.has_fee() {
        format!("{} msat + {} ppm per mint", s.base_fee_msat, s.fee_ppm)
    } else {
        "none".into()
    };
    if s.address_fee() != 0 {
        fees = format!(
            "{fees} &middot; Address fee: {} msat per registration",
            s.address_fee()
        );
    }
    let sunset = if s.sunset_mint {
        "<p class=warn>This mint is winding down: minting and splitting are disabled.</p>"
    } else {
        ""
    };
    Html(format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>{title}</title>
<style>
:root{{color-scheme:light dark;--fg:#111;--bg:#fafafa;--muted:#666}}
@media (prefers-color-scheme:dark){{:root{{--fg:#eee;--bg:#161616;--muted:#999}}}}
body{{font:16px/1.5 system-ui,sans-serif;color:var(--fg);background:var(--bg);max-width:36rem;margin:2rem auto;padding:0 16px}}
.qr svg{{width:240px;height:240px;border-radius:8px}} code{{word-break:break-all}} .muted{{color:var(--muted)}} .warn{{color:#c33}}
h2{{font-size:1.1rem;margin-top:2rem}} .uri{{display:flex;gap:.5rem;align-items:flex-start}}
.copy{{flex:none;display:grid;place-items:center;width:2rem;height:2rem;padding:0;border:1px solid var(--muted);border-radius:6px;background:none;color:var(--fg);cursor:pointer}}
.copy svg{{width:1rem;height:1rem}} .copy.done{{border-color:#2a2;color:#2a2}}
</style></head><body>
<h1>{title}</h1><p>{description}</p>{sunset}
<p>Pay <b>{address}</b> from a wallet that can attach a LUD-12 comment to mint an
<a href="https://github.com/lnurl/luds/blob/luds/25.md">LNURLcash</a> bearer note.</p>
<div class=qr><a href="lightning:{lnurl}">{qr}</a></div>
<p><code>{lnurl}</code></p>
<p class=muted>Mint fee: {fees} &middot; Outstanding notes: {outstanding} sat</p>
{node}
<script>
document.addEventListener('click', async (e) => {{
  const b = e.target.closest('[data-copy]');
  if (!b) return;
  try {{
    await navigator.clipboard.writeText(b.dataset.copy);
    b.classList.add('done');
    b.title = 'Copied';
    setTimeout(() => {{ b.classList.remove('done'); b.title = 'Copy'; }}, 1500);
  }} catch {{
    getSelection().selectAllChildren(b.previousElementSibling);
  }}
}});
</script>
</body></html>"#,
        title = escape(&s.title),
        description = escape(&s.description),
    ))
    .into_response()
}

/// The protocol over HTTP, for everything that needs no Lightning: notes are
/// credited straight into the store, as a settled mint invoice would.
#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use lnurlcash_core::{
        cash::{CashNode, derive_cash_root, derive_service_root},
        recoverable::{
            Cw1, LEGACY_PURPOSE_LIGHTNING_ADDRESS, cash_node_to_cx1, derive_note_pubkey,
            derive_share_node, encode_ck1, encode_cp1, encode_cw1, encode_cx1, encode_legacy_cx1,
            legacy_branch_node, legacy_cx1_of, legacy_derive_note_pubkey, sign_note_ownership,
        },
        signature::sign_address_proof,
        spend::{bearer_cw1, bearer_leaf, tapleaf_hash, taproot_tweak, taproot_tweak_secret_key},
    };
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        db::{INVALID_K1, NoteStore},
        ln::{Ln, Network},
        spend,
        state::test_settings,
    };

    fn app() -> AppState {
        AppState::new(
            test_settings(),
            std::sync::Arc::new(NoteStore::in_memory().unwrap()),
            Ln::new(Network::Regtest),
        )
    }

    async fn call(state: &AppState, method: &str, uri: &str) -> Value {
        form(state, method, uri, "").await
    }

    /// A request with an `application/x-www-form-urlencoded` body, as
    /// LUD-26's registration sends one.
    async fn form(state: &AppState, method: &str, uri: &str, body: &str) -> Value {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "mint.example")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200, "LUD-01: errors are 200s too");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    async fn get(state: &AppState, uri: &str) -> Value {
        call(state, "GET", uri).await
    }

    fn reason(v: &Value) -> &str {
        assert_eq!(v["status"], "ERROR", "{v}");
        v["reason"].as_str().unwrap()
    }

    fn ok(v: &Value) {
        assert_eq!(v["status"], "OK", "{v}");
    }

    /// A bearer note: its spend (the preimage, hex) and its reference (`h`).
    fn bearer(seed: u8) -> (String, String) {
        let preimage = [seed; 32];
        (hex::encode(preimage), hex::encode(Sha256::digest(preimage)))
    }

    /// Credit the note `h` names with `amount_msat`, as a paid mint would.
    fn credit(state: &AppState, h: &str, amount_msat: u64) {
        let note_id = spend::note_id_of_ref(h).unwrap();
        let payment_hash = hex::encode(Sha256::digest(h.as_bytes()));
        state
            .store
            .create_mint(&payment_hash, "lnbcrt1", amount_msat, &note_id)
            .unwrap();
        state
            .store
            .settle_mint(&payment_hash, &|_, _| None)
            .unwrap();
    }

    async fn value_of(state: &AppState, k1: &str) -> Value {
        get(state, &format!("/w?k1={k1}")).await["maxWithdrawable"].clone()
    }

    #[tokio::test]
    async fn the_informational_request_never_burns() {
        let state = app();
        let (k1, h) = bearer(1);
        credit(&state, &h, 50_000);
        for _ in 0..2 {
            let v = get(&state, &format!("/w?k1={k1}&amount=999")).await;
            assert_eq!(v["tag"], "withdrawRequest");
            assert_eq!(v["k1"], k1, "echoes the secret it was queried with");
            assert_eq!(v["maxWithdrawable"], 50_000);
            assert_eq!(v["callback"], "https://mint.example/w/cb");
        }
        assert_eq!(
            get(&state, &format!("/w?p={h}")).await["maxWithdrawable"],
            50_000
        );
        assert_eq!(reason(&get(&state, "/w?k1=00").await), "Unknown note.");
    }

    #[tokio::test]
    async fn rotate_burns_and_answers_a_retry_with_the_same_result() {
        let state = app();
        let (k1, h) = bearer(1);
        let (k1b, hb) = bearer(2);
        credit(&state, &h, 50_000);
        let uri = format!("/w/cb?k1={k1}&p1={hb}");
        ok(&get(&state, &uri).await);
        assert_eq!(value_of(&state, &k1b).await, 50_000);
        // the same request again is the same rotate, not a double spend
        ok(&get(&state, &uri).await);
        assert_eq!(value_of(&state, &k1b).await, 50_000);
        // into anything else, the burned note is gone: LUD-25 answers a
        // burned note "already in use", which LUD-26 recovery counts as used
        let (_, hc) = bearer(3);
        let again = get(&state, &format!("/w/cb?k1={k1}&p1={hc}")).await;
        assert_eq!(reason(&again), "already in use");
        assert_eq!(
            reason(&get(&state, &format!("/w?k1={k1}")).await),
            "already in use"
        );
        assert_eq!(
            reason(&get(&state, &format!("/w?p={h}")).await),
            "already in use"
        );
    }

    #[tokio::test]
    async fn split_takes_the_base_fee_from_the_change() {
        let state = app();
        let (k1, h) = bearer(1);
        let (k1a, ha) = bearer(2);
        let (k1b, hb) = bearer(3);
        credit(&state, &h, 50_000);
        let v = get(
            &state,
            &format!("/w/cb?k1={k1}&amount=20000&p1={ha}&p2={hb}"),
        )
        .await;
        ok(&v);
        assert_eq!(value_of(&state, &k1a).await, 20_000);
        assert_eq!(value_of(&state, &k1b).await, 50_000 - 20_000 - 1000);
    }

    #[tokio::test]
    async fn split_bounds() {
        let state = app();
        let (k1, h) = bearer(1);
        let (_, ha) = bearer(2);
        let (_, hb) = bearer(3);
        credit(&state, &h, 50_000);
        let at = |amount: &str| format!("/w/cb?k1={k1}&amount={amount}&p1={ha}&p2={hb}");
        assert!(reason(&get(&state, &at("0")).await).starts_with("amount must be between"));
        assert!(reason(&get(&state, &at("50000")).await).starts_with("amount must be between"));
        assert_eq!(
            reason(&get(&state, &at("49500")).await),
            "insufficient value"
        );
        assert_eq!(
            reason(&get(&state, &format!("/w/cb?k1={k1}&amount=100&p1={ha}")).await),
            "missing p2"
        );
        // nothing above burned anything
        assert_eq!(value_of(&state, &k1).await, 50_000);
    }

    #[tokio::test]
    async fn merge_refunds_the_split_fees() {
        let state = app();
        let (k1a, ha) = bearer(1);
        let (k1b, hb) = bearer(2);
        let (k1c, hc) = bearer(3);
        credit(&state, &ha, 30_000);
        credit(&state, &hb, 19_000);
        ok(&get(&state, &format!("/w/cb?k1={k1a}&k1={k1b}&p1={hc}")).await);
        assert_eq!(value_of(&state, &k1c).await, 30_000 + 19_000 + 1000);
    }

    #[tokio::test]
    async fn one_bad_k1_and_nothing_happens() {
        let state = app();
        let (k1a, ha) = bearer(1);
        let (k1b, _) = bearer(2);
        let (_, hc) = bearer(3);
        credit(&state, &ha, 30_000);
        let v = get(&state, &format!("/w/cb?k1={k1a}&k1={k1b}&p1={hc}")).await;
        assert_eq!(reason(&v), INVALID_K1);
        assert_eq!(value_of(&state, &k1a).await, 30_000);
    }

    const SAME_NOTE: &str = "the same note is spent twice";

    /// Notes are compared as the `Q` they decode to, never as raw strings.
    #[tokio::test]
    async fn one_note_spent_twice_is_no_merge() {
        let state = app();
        let (k1a, ha) = bearer(1);
        let (k1b, hb) = bearer(2);
        let (_, hc) = bearer(3);
        credit(&state, &ha, 30_000);
        credit(&state, &hb, 20_000);
        // the same string twice
        let v = get(&state, &format!("/w/cb?k1={k1a}&k1={k1a}&p1={hc}")).await;
        assert_eq!(reason(&v), SAME_NOTE);
        // a hex preimage and its cw1, alongside a real second note
        let cw1a = encode_cw1(&bearer_cw1(&[1; 32]).unwrap()).unwrap();
        let uri = format!("/w/cb?k1={k1a}&k1={k1b}&k1={cw1a}&p1={hc}");
        assert_eq!(reason(&get(&state, &uri).await), SAME_NOTE);
        // and as a split
        let uri = format!("/w/cb?k1={cw1a}&k1={k1a}&amount=100&p1={hc}&p2={hb}");
        assert_eq!(reason(&get(&state, &uri).await), SAME_NOTE);
        // nothing was burned
        assert_eq!(value_of(&state, &k1a).await, 30_000);
        assert_eq!(value_of(&state, &k1b).await, 20_000);
        ok(&get(&state, &format!("/w/cb?k1={k1a}&k1={k1b}&p1={hc}")).await);
    }

    /// A key with a script tree: its `ck1` (tweaked key path) and its `cw1`
    /// (the leaf) are two valid spends of one `Q`.
    #[tokio::test]
    async fn a_ck1_and_a_cw1_of_one_note_are_no_merge() {
        let state = app();
        let internal_sk = [21u8; 32];
        let preimage = [22u8; 32];
        let internal = secp256k1::Keypair::from_secret_bytes(internal_sk)
            .unwrap()
            .x_only_public_key()
            .0
            .to_byte_array();
        let leaf = bearer_leaf(&Sha256::digest(preimage).into());
        let root = tapleaf_hash(&leaf, 0xc0);
        let tweak = taproot_tweak(&internal, &root).unwrap();
        let q = encode_cp1(&tweak.output_key);
        let tweaked_sk = taproot_tweak_secret_key(&internal_sk, &root).unwrap();
        let ck1 = encode_ck1(&sign_note_ownership(&tweaked_sk, "mint.example").unwrap());
        let mut control_block = vec![0xc0 | tweak.parity];
        control_block.extend_from_slice(&internal);
        let cw1 = encode_cw1(&Cw1 {
            locktime: 0,
            sequence: 0xffff_ffff,
            script: leaf.to_vec(),
            control_block,
            witness: vec![preimage.to_vec()],
        })
        .unwrap();
        credit(&state, &q, 30_000);
        // each opens the note on its own
        assert_eq!(value_of(&state, &ck1).await, 30_000);
        assert_eq!(value_of(&state, &cw1).await, 30_000);

        let (k1b, hb) = bearer(2);
        let (_, hc) = bearer(3);
        credit(&state, &hb, 20_000);
        for uri in [
            format!("/w/cb?k1={ck1}&k1={cw1}&p1={hc}"),
            format!("/w/cb?k1={cw1}&k1={k1b}&k1={ck1}&p1={hc}"),
            format!("/w/cb?k1={ck1}&k1={ck1}&p1={hc}"),
        ] {
            assert_eq!(reason(&get(&state, &uri).await), SAME_NOTE, "{uri}");
        }
        assert_eq!(value_of(&state, &ck1).await, 30_000);
        assert_eq!(value_of(&state, &k1b).await, 20_000);
        // a plain hex preimage and a ck1 are two notes: they merge
        ok(&get(&state, &format!("/w/cb?k1={k1b}&k1={ck1}&p1={hc}")).await);
        assert_eq!(value_of(&state, &bearer(3).0).await, 30_000 + 20_000 + 1000);
        assert_eq!(
            reason(&get(&state, &format!("/w?k1={cw1}")).await),
            "already in use"
        );
    }

    #[tokio::test]
    async fn p1_and_p2_must_be_two_notes() {
        let state = app();
        let (k1, h) = bearer(1);
        let (_, ha) = bearer(2);
        credit(&state, &h, 50_000);
        let q: [u8; 32] = hex::decode(spend::note_id_of_ref(&ha).unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let cp1 = encode_cp1(&q);
        for (p1, p2) in [(&ha, &ha), (&ha, &cp1), (&cp1, &ha)] {
            let uri = format!("/w/cb?k1={k1}&amount=100&p1={p1}&p2={p2}");
            assert_eq!(
                reason(&get(&state, &uri).await),
                "p1 and p2 are the same note"
            );
        }
        assert_eq!(value_of(&state, &k1).await, 50_000);
    }

    #[tokio::test]
    async fn outputs_must_be_fresh_and_present() {
        let state = app();
        let (k1a, ha) = bearer(1);
        let (_, hb) = bearer(2);
        credit(&state, &ha, 30_000);
        credit(&state, &hb, 30_000);
        let v = get(&state, &format!("/w/cb?k1={k1a}&p1={hb}")).await;
        assert_eq!(reason(&v), "already in use");
        assert_eq!(
            reason(&get(&state, &format!("/w/cb?k1={k1a}")).await),
            "missing p1"
        );
        let v = get(&state, &format!("/w/cb?k1={k1a}&p1={hb}&p=x")).await;
        assert!(reason(&v).starts_with("p is only accepted"));
        assert_eq!(value_of(&state, &k1a).await, 30_000);
    }

    #[tokio::test]
    async fn sunset_refuses_splits_but_not_rotates() {
        let mut settings = test_settings();
        settings.sunset_mint = true;
        let state = AppState::new(
            settings,
            std::sync::Arc::new(NoteStore::in_memory().unwrap()),
            Ln::new(Network::Regtest),
        );
        let (k1, h) = bearer(1);
        let (_, ha) = bearer(2);
        let (_, hb) = bearer(3);
        credit(&state, &h, 50_000);
        let v = get(&state, &format!("/w/cb?k1={k1}&amount=100&p1={ha}&p2={hb}")).await;
        assert!(reason(&v).contains("sunsetting"));
        let v = get(&state, &format!("/p/cb?amount=20000&comment={ha}")).await;
        assert!(reason(&v).contains("sunsetting"));
        ok(&get(&state, &format!("/w/cb?k1={k1}&p1={ha}")).await);
    }

    #[tokio::test]
    async fn no_node_no_mint_and_no_melt_and_no_reservation() {
        let state = app();
        let (k1, h) = bearer(1);
        let (_, hb) = bearer(2);
        credit(&state, &h, 50_000);
        let v = get(&state, &format!("/p/cb?amount=20000&comment={hb}")).await;
        assert_eq!(reason(&v), "Minting is temporarily unavailable.");
        let v = get(&state, &format!("/w/cb?k1={k1}&pr=lnbcrt1")).await;
        assert_eq!(reason(&v), "Melting is temporarily unavailable.");
        // the note was never reserved
        assert_eq!(value_of(&state, &k1).await, 50_000);
        let v = get(&state, &format!("/w/cb?k1={k1}&k1={k1}&pr=lnbcrt1")).await;
        assert!(reason(&v).starts_with("pr cannot be combined"));
    }

    #[tokio::test]
    async fn the_fixed_identity_answers_to_its_name_and_to_underscore() {
        let state = app();
        for name in ["mint", "MINT", "_"] {
            let v = get(&state, &format!("/.well-known/lnurlp/{name}")).await;
            assert_eq!(v["tag"], "payRequest");
            assert_eq!(v["callback"], "https://mint.example/p/cb");
            assert_eq!(v["commentAllowed"], 64);
            assert_eq!(v["withdrawLink"], "https://mint.example/w");
            let metadata: Value = serde_json::from_str(v["metadata"].as_str().unwrap()).unwrap();
            assert!(metadata.to_string().contains("Mint fees: 1000,2000"));
        }
        let v = get(&state, "/.well-known/lnurlp/nobody").await;
        assert_eq!(reason(&v), "unknown user");
    }

    /// A wallet's LUD-26 tree at mint.example: its root `S`, and its shares.
    fn service_root() -> CashNode {
        derive_service_root(&derive_cash_root(&[7; 32]).unwrap(), "mint.example").unwrap()
    }

    fn share(n: u32) -> (CashNode, String) {
        let node = derive_share_node(&service_root(), 0, n).unwrap();
        let cx1 = encode_cx1(&cash_node_to_cx1(&node).unwrap());
        (node, cx1)
    }

    fn proof(key: &CashNode, action: &str, username: &str, cx1: Option<&str>) -> String {
        hex::encode(
            sign_address_proof(&key.private_key, action, "mint.example", username, cx1).unwrap(),
        )
    }

    async fn register(state: &AppState, username: &str, n: u32) -> Value {
        let (key, cx1) = share(n);
        let sig = proof(&key, "register", username, Some(&cx1));
        form(
            state,
            "POST",
            &format!("/.well-known/lnurlp/{username}"),
            &format!("cx1={cx1}&sig={sig}"),
        )
        .await
    }

    #[tokio::test]
    async fn a_share_registers_at_its_address_and_never_twice() {
        let state = app();
        let (key0, cx1_0) = share(0);
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        let uri = "/.well-known/lnurlp/alice";

        // a proof for another action, username or cx1 proves nothing
        for sig in [
            proof(&key0, "unregister", "alice", None),
            proof(&key0, "register", "bob", Some(&cx1_0)),
            proof(&key0, "register", "alice", Some(&share(1).1)),
        ] {
            let bad = form(&state, "POST", uri, &format!("cx1={cx1_0}&sig={sig}")).await;
            assert_eq!(reason(&bad), "Invalid ownership signature.");
        }
        let sig = proof(&key0, "register", "alice", Some(&cx1_0));
        ok(&form(
            &state,
            "POST",
            uri,
            &format!("cx1={cx1_0}&sig={sig}&npub={npub}"),
        )
        .await);

        // an ordinary Lightning Address: no comment, no cx1 in sight, and a
        // +tag lands on the same share
        for name in ["Alice", "alice+coffee"] {
            let v = get(&state, &format!("/.well-known/lnurlp/{name}")).await;
            assert_eq!(v["callback"], "https://mint.example/p/alice", "{name}");
            assert_eq!(v["withdrawLink"], "https://mint.example/w");
            assert!(v.get("commentAllowed").is_none());
            let metadata = v["metadata"].as_str().unwrap();
            assert!(!metadata.contains("text/cpub") && !metadata.contains("cx1"));
            assert!(metadata.contains("Mint fees: 1000,2000"));
        }
        let v = get(&state, "/.well-known/nostr.json?name=alice").await;
        assert_eq!(
            v["names"]["alice"],
            "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
        );

        // never overwritten, and a share is never registered twice
        assert_eq!(
            reason(&register(&state, "alice", 1).await),
            "already registered"
        );
        assert_eq!(
            reason(&register(&state, "bob", 0).await),
            "cx1 already used"
        );
        // mint@ is the mint's own payRequest, and + is never in a username
        for name in ["mint", "a+b"] {
            assert_eq!(
                reason(&register(&state, name, 2).await),
                "Invalid or reserved username."
            );
        }
    }

    #[tokio::test]
    async fn a_put_moves_the_address_and_a_delete_frees_it() {
        let state = app();
        ok(&register(&state, "alice", 0).await);
        let (key0, _) = share(0);
        let (key1, cx1_1) = share(1);
        let uri = "/.well-known/lnurlp/alice";
        let update = |sig: &str, newsig: &str| format!("cx1={cx1_1}&sig={sig}&newsig={newsig}");

        let newsig = proof(&key1, "register", "alice", Some(&cx1_1));
        // the key on file must sign the update, over the new cx1
        for sig in [
            proof(&key1, "update", "alice", Some(&cx1_1)),
            proof(&key0, "update", "alice", Some(&share(2).1)),
        ] {
            let v = form(&state, "PUT", uri, &update(&sig, &newsig)).await;
            assert_eq!(reason(&v), "Invalid ownership signature.");
        }
        let sig = proof(&key0, "update", "alice", Some(&cx1_1));
        // and the new share its own register message
        let v = form(&state, "PUT", uri, &update(&sig, &sig)).await;
        assert_eq!(reason(&v), "Invalid ownership signature.");
        ok(&form(&state, "PUT", uri, &update(&sig, &newsig)).await);
        let v = form(
            &state,
            "PUT",
            "/.well-known/lnurlp/bob",
            &update(&sig, &newsig),
        )
        .await;
        assert_eq!(reason(&v), "unknown user");

        // only the key now on file frees it
        let old = proof(&key0, "unregister", "alice", None);
        assert_eq!(
            reason(&form(&state, "DELETE", uri, &format!("sig={old}")).await),
            "Invalid ownership signature."
        );
        let sig = proof(&key1, "unregister", "alice", None);
        ok(&form(&state, "DELETE", uri, &format!("sig={sig}")).await);
        assert_eq!(reason(&get(&state, uri).await), "unknown user");
        assert_eq!(
            get(&state, "/.well-known/nostr.json?name=alice").await["names"],
            json!({})
        );
        // free again, but neither share can be registered a second time
        assert_eq!(
            reason(&register(&state, "alice", 0).await),
            "cx1 already used"
        );
        assert_eq!(
            reason(&register(&state, "alice", 1).await),
            "cx1 already used"
        );
        ok(&register(&state, "alice", 2).await);
    }

    /// A regtest invoice for `amount_msat` to payment hash `[seed; 32]`, as
    /// the node would issue for an address fee.
    fn regtest_invoice(seed: u8, amount_msat: u64) -> (String, String) {
        use ldk_node::{
            bitcoin::{
                hashes::{Hash, sha256},
                secp256k1::{Secp256k1, SecretKey},
            },
            lightning_invoice::{Currency, InvoiceBuilder},
            lightning_types::payment::PaymentSecret,
        };
        let key = SecretKey::from_slice(&[7; 32]).unwrap();
        let hash = sha256::Hash::from_byte_array([seed; 32]);
        let invoice = InvoiceBuilder::new(Currency::Regtest)
            .description("Register".into())
            .payment_hash(hash)
            .payment_secret(PaymentSecret([1; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(amount_msat)
            .build_signed(|m| Secp256k1::new().sign_ecdsa_recoverable(m, &key))
            .unwrap();
        (invoice.to_string(), hex::encode([seed; 32]))
    }

    #[tokio::test]
    async fn an_address_fee_holds_the_username_until_it_is_paid() {
        let mut settings = test_settings();
        settings.address_fee_msat = 21_000;
        let state = AppState::new(
            settings,
            std::sync::Arc::new(NoteStore::in_memory().unwrap()),
            Ln::new(Network::Regtest),
        );
        // announced at mint@, the third Mint fees value
        let v = get(&state, "/.well-known/lnurlp/mint").await;
        assert!(
            v["metadata"]
                .as_str()
                .unwrap()
                .contains("Mint fees: 1000,2000,21000")
        );
        // no node, no invoice: nothing is held
        assert_eq!(
            reason(&register(&state, "alice", 0).await),
            "Registration is temporarily unavailable."
        );
        assert!(!state.store.username_taken("alice").unwrap());

        // held, as a registration the node issued an invoice for would be
        let (pr, hash) = regtest_invoice(9, 21_000);
        let (_, cx1) = share(1);
        let share1 = lnurlcash_core::recoverable::decode_cx1(&cx1).unwrap();
        state
            .store
            .hold_username(
                &hash,
                &pr,
                "alice",
                &crate::mint::Share::of(&share1),
                &hex::encode(share1.pubkey_x_only()),
                None,
                21_000,
                "address-fee:test",
                crate::db::now() + 3600,
            )
            .unwrap();
        // not published, and nobody else gets it meanwhile
        assert_eq!(
            reason(&register(&state, "alice", 2).await),
            "already registered"
        );
        let uri = "/.well-known/lnurlp/alice";
        assert_eq!(reason(&get(&state, uri).await), "unknown user");
        let (k1, h) = bearer(1);
        credit(&state, &h, 21_000);
        let transfer = format!("/p/alice?amount=21000&k1={k1}");
        assert_eq!(reason(&get(&state, &transfer).await), "unknown user");

        // a note of another value does not pay it
        let (k1b, hb) = bearer(2);
        credit(&state, &hb, 20_000);
        let v = get(&state, &format!("/w/cb?k1={k1b}&pr={pr}")).await;
        assert_eq!(reason(&v), "Invoice must be for exactly 20000 msat.");
        assert_eq!(value_of(&state, &k1b).await, 20_000);

        // melting a note into it pays it here, with no node at all
        let v = get(&state, &format!("/w/cb?k1={k1}&pr={pr}")).await;
        ok(&v);
        assert_eq!(v["verify"], format!("https://mint.example/verify/{hash}"));
        assert_eq!(
            reason(&get(&state, &format!("/w?k1={k1}")).await),
            "already in use"
        );
        assert_eq!(
            get(&state, uri).await["callback"],
            "https://mint.example/p/alice"
        );
        let v = get(&state, &format!("/verify/{hash}")).await;
        assert_eq!(
            (v["settled"].as_bool(), v["pr"].as_str()),
            (Some(true), Some(pr.as_str()))
        );
        // paid once
        let (k1c, hc) = bearer(3);
        credit(&state, &hc, 21_000);
        let v = get(&state, &format!("/w/cb?k1={k1c}&pr={pr}")).await;
        assert_eq!(reason(&v), "address fee invoice already paid or expired");
        assert_eq!(value_of(&state, &k1c).await, 21_000);
    }

    #[tokio::test]
    async fn a_browser_wallet_gets_its_preflight() {
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/.well-known/lnurlp/alice")
            .body(Body::empty())
            .unwrap();
        let res = router(app()).oneshot(req).await.unwrap();
        let headers = res.headers();
        assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let methods = headers[header::ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()
            .unwrap();
        assert!(methods.contains("PUT") && methods.contains("DELETE"));
    }

    #[tokio::test]
    async fn an_internal_transfer_credits_the_payees_next_share_note() {
        let state = app();
        ok(&register(&state, "alice", 0).await);
        let watched = lnurlcash_core::recoverable::decode_cx1(&share(0).1).unwrap();
        let note = |i| encode_cp1(&derive_note_pubkey(&watched, i).unwrap());

        let (k1, h) = bearer(1);
        credit(&state, &h, 30_000);
        // the whole note, never part of it
        let part = get(&state, &format!("/p/alice?amount=20000&k1={k1}")).await;
        assert!(reason(&part).contains("full value"));
        let uri = format!("/p/alice+tip?amount=30000&k1={k1}");
        ok(&get(&state, &uri).await);
        // free of fees, at share note 0, and the note it came from is burned
        assert_eq!(
            get(&state, &format!("/w?p={}", note(0))).await["maxWithdrawable"],
            30_000
        );
        assert_eq!(
            reason(&get(&state, &format!("/w?k1={k1}")).await),
            "already in use"
        );
        // a retry is answered as the first, and credits nothing more
        ok(&get(&state, &uri).await);
        assert_eq!(
            reason(&get(&state, &format!("/w?p={}", note(1))).await),
            "Unknown note."
        );

        // the next transfer skips a share note somebody already holds
        let (_, taken) = bearer(9);
        let (k1b, hb) = bearer(2);
        credit(&state, &hb, 20_000);
        let q1 = spend::note_id_of_ref(&note(1)).unwrap();
        let payment_hash = hex::encode(Sha256::digest(taken.as_bytes()));
        state
            .store
            .create_mint(&payment_hash, "lnbcrt1", 1, &q1)
            .unwrap();
        ok(&get(&state, &format!("/p/alice?amount=20000&k1={k1b}")).await);
        assert_eq!(
            get(&state, &format!("/w?p={}", note(2))).await["maxWithdrawable"],
            20_000
        );

        let (k1c, hc) = bearer(3);
        credit(&state, &hc, 20_000);
        assert_eq!(
            reason(&get(&state, &format!("/p/nobody?amount=20000&k1={k1c}")).await),
            "unknown user"
        );
        assert_eq!(
            reason(&get(&state, &format!("/p/mint?amount=20000&k1={k1c}")).await),
            "unknown user"
        );
    }

    /// A username registered before LUD-26 holds the old 64-byte cx1. It
    /// keeps auto-minting under the old purpose-2 tweak, and its owner moves
    /// it to a share with an update signed by the old branch key.
    #[tokio::test]
    async fn a_pre_lud26_address_keeps_working_until_it_is_moved() {
        let state = app();
        let legacy_node =
            legacy_branch_node(&derive_cash_root(&[7; 32]).unwrap(), "mint.example").unwrap();
        let legacy = legacy_cx1_of(&legacy_node).unwrap();
        let on_file = hex::encode([legacy.pubkey_x_only, legacy.chain_code].concat());
        state
            .store
            .register_username("alice", &on_file, &hex::encode(legacy.pubkey_x_only), None)
            .unwrap();
        assert_eq!(
            get(&state, "/.well-known/lnurlp/alice").await["callback"],
            "https://mint.example/p/alice"
        );

        let (k1, h) = bearer(1);
        credit(&state, &h, 30_000);
        ok(&get(&state, &format!("/p/alice?amount=30000&k1={k1}")).await);
        let old_note =
            legacy_derive_note_pubkey(&legacy, Some(LEGACY_PURPOSE_LIGHTNING_ADDRESS), 0).unwrap();
        assert_eq!(
            get(&state, &format!("/w?p={}", encode_cp1(&old_note))).await["maxWithdrawable"],
            30_000
        );
        // the old cx1 is never registered anew
        let v = form(
            &state,
            "POST",
            "/.well-known/lnurlp/bob",
            &format!("cx1={}&sig=00", encode_legacy_cx1(&legacy)),
        )
        .await;
        assert_eq!(reason(&v), "Invalid cx1.");

        let (key1, cx1_1) = share(1);
        let sig = proof(&legacy_node, "update", "alice", Some(&cx1_1));
        let newsig = proof(&key1, "register", "alice", Some(&cx1_1));
        ok(&form(
            &state,
            "PUT",
            "/.well-known/lnurlp/alice",
            &format!("cx1={cx1_1}&sig={sig}&newsig={newsig}"),
        )
        .await);
        let (k1b, hb) = bearer(2);
        credit(&state, &hb, 20_000);
        ok(&get(&state, &format!("/p/alice?amount=20000&k1={k1b}")).await);
        let watched = lnurlcash_core::recoverable::decode_cx1(&cx1_1).unwrap();
        let new_note = encode_cp1(&derive_note_pubkey(&watched, 0).unwrap());
        assert_eq!(
            get(&state, &format!("/w?p={new_note}")).await["maxWithdrawable"],
            20_000
        );
    }

    #[tokio::test]
    async fn verify_knows_only_its_own_invoices() {
        let state = app();
        assert_eq!(
            reason(&get(&state, &format!("/verify/{}", "ab".repeat(32))).await),
            "Not found"
        );
    }

    #[tokio::test]
    async fn the_front_page_renders() {
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let res = router(app()).oneshot(req).await.unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("mint@mint.example"));
        assert!(html.contains("<svg"));
        // no node, nothing to connect to
        assert!(!html.contains("Connect to the node"));
    }
}
