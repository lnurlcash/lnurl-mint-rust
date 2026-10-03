//! The admin web UI: a login that trades the admin token for a session
//! cookie, and the page itself (HTML, CSS and JS compiled into the binary).
//!
//! The session cookie is `HttpOnly` and `SameSite=Strict`, so the page's own
//! script never holds a credential. A request authenticated by it may only
//! change something (any method but GET) when it comes from this origin.
//! Sessions live in memory: a restart logs everyone out. Scripts keep using
//! `Authorization: Bearer <ADMIN_TOKEN>`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

const COOKIE: &str = "mint_admin";
const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 3600);
/// What a wrong token costs, so guessing one is slow too.
const FAILED_LOGIN_DELAY: Duration = Duration::from_millis(500);

const INDEX_HTML: &str = include_str!("admin_ui/index.html");
const ADMIN_JS: &str = include_str!("admin_ui/admin.js");
const ADMIN_CSS: &str = include_str!("admin_ui/admin.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; connect-src 'self'; base-uri 'none'; \
                   form-action 'self'; frame-ancestors 'none'";

#[derive(Debug)]
pub struct Auth {
    token: String,
    sessions: Mutex<HashMap<String, Instant>>,
}

impl Auth {
    pub fn new(token: String) -> Self {
        Auth {
            token,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Compares digests, so the check takes the same time however much of a
    /// wrong token matches.
    fn token_matches(&self, presented: &str) -> bool {
        Sha256::digest(presented.as_bytes()) == Sha256::digest(self.token.as_bytes())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn new_session(&self) -> String {
        let id = hex::encode(rand::random::<[u8; 32]>());
        let now = Instant::now();
        let mut sessions = self.lock();
        sessions.retain(|_, expires| *expires > now);
        sessions.insert(id.clone(), now + SESSION_LIFETIME);
        id
    }

    fn session_valid(&self, id: &str) -> bool {
        self.lock()
            .get(id)
            .is_some_and(|expires| *expires > Instant::now())
    }

    fn end_session(&self, id: &str) {
        self.lock().remove(id);
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE).then(|| value.to_string())
        })
}

/// Whether a cookie-authenticated request may change something: it comes
/// from this origin. Browsers send `Origin` on every POST.
fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
    match (origin, host) {
        (Some(origin), Some(host)) => {
            origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) == Some(host)
        }
        _ => false,
    }
}

/// The API's gate: a bearer token, or a session from the login.
pub async fn require_auth(State(auth): State<Arc<Auth>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        if !auth.token_matches(bearer) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        return next.run(req).await;
    }
    if !session_cookie(headers).is_some_and(|id| auth.session_valid(&id)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if req.method() != Method::GET && req.method() != Method::HEAD && !same_origin(headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    next.run(req).await
}

#[derive(Deserialize)]
pub struct Login {
    token: String,
}

pub async fn login(
    State(auth): State<Arc<Auth>>,
    headers: HeaderMap,
    Json(body): Json<Login>,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    if !auth.token_matches(body.token.trim()) {
        tokio::time::sleep(FAILED_LOGIN_DELAY).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "wrong token"}))).into_response();
    }
    let id = auth.new_session();
    // Secure once a TLS proxy says the browser came over https; the admin
    // API itself only speaks plain HTTP, normally on loopback
    let secure = headers
        .get("x-forwarded-proto")
        .is_some_and(|p| p.as_bytes().eq_ignore_ascii_case(b"https"));
    let cookie = format!(
        "{COOKIE}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_LIFETIME.as_secs(),
        if secure { "; Secure" } else { "" }
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(json!({"status": "OK"})),
    )
        .into_response()
}

pub async fn logout(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    if let Some(id) = session_cookie(&headers) {
        auth.end_session(&id);
    }
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
        )],
        Json(json!({"status": "OK"})),
    )
        .into_response()
}

pub async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML).into_response()
}

pub async fn script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        ADMIN_JS,
    )
        .into_response()
}

pub async fn stylesheet() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], ADMIN_CSS).into_response()
}

#[derive(Deserialize)]
pub struct QrQuery {
    data: String,
}

/// An SVG QR code: addresses and invoices, for scanning off the screen.
pub async fn qr(Query(q): Query<QrQuery>) -> Response {
    if q.data.len() > 2000 {
        return (StatusCode::BAD_REQUEST, "too long").into_response();
    }
    let Ok(code) = qrcode::QrCode::new(q.data.as_bytes()) else {
        return (StatusCode::BAD_REQUEST, "cannot encode").into_response();
    };
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#111"))
        .light_color(qrcode::render::svg::Color("#fff"))
        .build();
    ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
}

/// Headers every admin response carries.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}
