use crate::App;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);

pub struct Sessions {
    map: Mutex<HashMap<String, u64>>, // token -> expiry epoch secs
    path: String,
    failures: Mutex<VecDeque<Instant>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

impl Sessions {
    pub fn load(path: &str) -> Self {
        let map: HashMap<String, u64> = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Sessions {
            map: Mutex::new(map),
            path: path.to_string(),
            failures: Mutex::new(VecDeque::new()),
        }
    }

    fn save(&self, map: &HashMap<String, u64>) {
        let _ = std::fs::write(&self.path, serde_json::to_string(map).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &self.path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }

    pub fn create(&self) -> String {
        use rand::Rng;
        let token: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(48)
            .map(char::from)
            .collect();
        let mut map = self.map.lock().unwrap();
        map.retain(|_, exp| *exp > now_secs());
        map.insert(token.clone(), now_secs() + SESSION_TTL_SECS);
        self.save(&map);
        token
    }

    pub fn check(&self, token: &str) -> bool {
        let map = self.map.lock().unwrap();
        map.get(token).is_some_and(|exp| *exp > now_secs())
    }

    pub fn remove(&self, token: &str) {
        let mut map = self.map.lock().unwrap();
        map.remove(token);
        self.save(&map);
    }

    fn throttled(&self) -> bool {
        let mut f = self.failures.lock().unwrap();
        while f.front().is_some_and(|t| t.elapsed() > FAILURE_WINDOW) {
            f.pop_front();
        }
        f.len() >= MAX_FAILURES
    }

    fn record_failure(&self) {
        self.failures.lock().unwrap().push_back(Instant::now());
    }
}

/// Constant-time comparison (fast-fails on length mismatch, but the byte comparison itself doesn't short-circuit).
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookies.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("session=") {
            return Some(v.to_string());
        }
    }
    None
}

#[derive(serde::Deserialize)]
pub struct LoginReq {
    password: String,
}

pub async fn login(
    State(app): State<Arc<App>>,
    Json(body): Json<LoginReq>,
) -> Response {
    if app.sessions.throttled() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "too many attempts, try again later"})),
        )
            .into_response();
    }
    if !ct_eq(&body.password, &app.cfg.password) {
        app.sessions.record_failure();
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "wrong password"})),
        )
            .into_response();
    }
    let token = app.sessions.create();
    let cookie = format!(
        "session={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}"
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

pub async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(t) = session_token(&headers) {
        app.sessions.remove(&t);
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            "session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0".to_string(),
        )],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

pub async fn me() -> Json<serde_json::Value> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    Json(serde_json::json!({"ok": true, "home": home, "hostname": host}))
}

/// Auth middleware: everything under /api and /ws requires a valid session, except /api/login.
/// (Static assets are handled outside this layer; the SPA shell itself is public and contains no user data.)
pub async fn auth_mw(
    State(app): State<Arc<App>>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/api/login" {
        return next.run(req).await;
    }
    // Requests arriving over the local UNIX socket are pre-trusted by the OS
    // (mode 0600 in $XDG_RUNTIME_DIR). Skip session check.
    if req.extensions().get::<crate::TrustedLocal>().is_some() {
        return next.run(req).await;
    }
    let ok = session_token(req.headers())
        .map(|t| app.sessions.check(&t))
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "not logged in"})),
        )
            .into_response();
    }
    next.run(req).await
}
