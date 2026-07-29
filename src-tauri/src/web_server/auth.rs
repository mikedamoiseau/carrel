use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::net::SocketAddr;

use super::WebState;
use crate::db;
use crate::error::{FolioError, FolioResult};

const KEYRING_SERVICE: &str = "folio-web-server";
const KEYRING_USER: &str = "pin";
const SESSION_TTL_SECS: u64 = 86400; // 24 hours

/// Per-IP rate limiter for login attempts.
pub struct RateLimiter {
    attempts: std::sync::Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
    max_attempts: usize,
    window_secs: u64,
}

impl RateLimiter {
    pub fn new(max_attempts: usize, window_secs: u64) -> Self {
        Self {
            attempts: std::sync::Mutex::new(std::collections::HashMap::new()),
            max_attempts,
            window_secs,
        }
    }

    /// Atomically check and record a login attempt for an IP.
    /// Returns `true` if the attempt is allowed, `false` if rate-limited.
    /// The attempt is recorded under the same lock to prevent TOCTOU races.
    pub fn attempt(&self, ip: &str) -> bool {
        let mut map = self.attempts.lock().unwrap();
        let window = std::time::Duration::from_secs(self.window_secs);
        let times = map.entry(ip.to_string()).or_default();
        times.retain(|t| t.elapsed() < window);
        if times.len() >= self.max_attempts {
            return false;
        }
        times.push(std::time::Instant::now());
        true
    }

    /// Clear attempts for an IP (call after successful login).
    pub fn clear(&self, ip: &str) {
        let mut map = self.attempts.lock().unwrap();
        map.remove(ip);
    }
}

/// PIN strength levels returned by [`validate_pin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PinStrength {
    Weak,
    Fair,
    Strong,
}

/// Common PINs that are trivially guessable.
const COMMON_PINS: &[&str] = &[
    "0000", "1111", "2222", "3333", "4444", "5555", "6666", "7777", "8888", "9999", "1234", "4321",
    "0123", "3210", "1212", "6969", "1122", "2001", "1984", "2000", "1010", "2580", "0852",
];

/// Validate a PIN and return its strength, or an error message if rejected.
pub fn validate_pin(pin: &str) -> Result<PinStrength, &'static str> {
    if pin.len() < 4 {
        return Err("PIN must be at least 4 digits");
    }
    if pin.len() > 8 {
        return Err("PIN must be at most 8 digits");
    }
    if !pin.chars().all(|c| c.is_ascii_digit()) {
        return Err("PIN must contain only digits");
    }

    if COMMON_PINS.contains(&pin) {
        return Err("PIN is too common");
    }

    let chars: Vec<char> = pin.chars().collect();
    let all_same = chars.windows(2).all(|w| w[0] == w[1]);
    if all_same {
        return Err("PIN must not be all the same digit");
    }

    let digits: Vec<i32> = chars.iter().map(|&c| c as i32 - '0' as i32).collect();
    let ascending = digits.windows(2).all(|w| (w[1] - w[0]).rem_euclid(10) == 1);
    let descending = digits.windows(2).all(|w| (w[0] - w[1]).rem_euclid(10) == 1);
    if ascending || descending {
        return Err("PIN must not be a sequential pattern");
    }

    let bytes = pin.as_bytes();
    let len = bytes.len();
    for sub_len in 2..=len / 2 {
        if len.is_multiple_of(sub_len)
            && bytes
                .chunks(sub_len)
                .all(|chunk| chunk == &bytes[..sub_len])
        {
            return Err("PIN must not be a repeating pattern");
        }
    }

    if pin.len() >= 6 {
        Ok(PinStrength::Strong)
    } else {
        Ok(PinStrength::Fair)
    }
}

/// Hash a PIN using SHA-256.
pub fn hash_pin(pin: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pin.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Store the PIN hash in the OS keychain.
pub fn store_pin(pin: &str) -> FolioResult<()> {
    let hash = hash_pin(pin);
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    entry
        .set_password(&hash)
        .map_err(|e| FolioError::internal(format!("keychain: {e}")))
}

/// Load the PIN hash from the OS keychain (None if not set).
pub fn load_pin_hash() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    entry.get_password().ok()
}

/// Verify a PIN against the stored hash.
pub fn verify_pin(pin: &str, stored_hash: &str) -> bool {
    hash_pin(pin) == stored_hash
}

/// Create a new session token and store it. Returns an error if the session store is unavailable.
pub fn create_session(state: &WebState) -> FolioResult<String> {
    let token = uuid::Uuid::new_v4().to_string();
    let mut sessions = state
        .sessions
        .lock()
        .map_err(|_| FolioError::internal("Session store unavailable"))?;
    sessions.insert(token.clone(), std::time::Instant::now());
    Ok(token)
}

/// Check if a session token is valid (exists and not expired).
pub fn validate_session(state: &WebState, token: &str) -> bool {
    if let Ok(mut sessions) = state.sessions.lock() {
        // Clean expired sessions opportunistically
        let ttl = std::time::Duration::from_secs(SESSION_TTL_SECS);
        sessions.retain(|_, created| created.elapsed() < ttl);

        sessions.contains_key(token)
    } else {
        false
    }
}

/// Extract a bearer token from the Authorization header.
fn extract_bearer(req: &Request<Body>) -> Option<String> {
    let header = req.headers().get("authorization")?.to_str().ok()?;
    header.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Extract a PIN from HTTP Basic Auth (for OPDS clients).
fn extract_basic_pin(req: &Request<Body>) -> Option<String> {
    use base64::Engine;
    let header = req.headers().get("authorization")?.to_str().ok()?;
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    // Format: "username:password" — we only care about the password (PIN)
    text.split_once(':').map(|(_, pin)| pin.to_string())
}

/// Extract session token from cookie.
fn extract_cookie_token(req: &Request<Body>) -> Option<String> {
    let cookie_header = req.headers().get("cookie")?.to_str().ok()?;
    for part in cookie_header.split(';') {
        let trimmed = part.trim();
        if let Some(token) = trimmed.strip_prefix("folio_session=") {
            return Some(token.to_string());
        }
    }
    None
}

/// Generate a QR code as an SVG string for the given URL.
pub fn generate_qr_svg(url: &str) -> FolioResult<String> {
    use qrcode::QrCode;
    let code =
        QrCode::new(url.as_bytes()).map_err(|e| FolioError::internal(format!("QR encode: {e}")))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .build();
    Ok(svg)
}

/// Which authentication mechanism produced a login attempt.
#[derive(Clone, Copy)]
pub enum WebAuthMethod {
    Session,
    Basic,
}

impl WebAuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            WebAuthMethod::Session => "session",
            WebAuthMethod::Basic => "basic",
        }
    }
}

/// The result of a login attempt.
#[derive(Clone, Copy)]
pub enum LoginOutcome {
    Success,
    InvalidPin,
    RateLimited,
}

impl LoginOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            LoginOutcome::Success => "success",
            LoginOutcome::InvalidPin => "invalid_pin",
            LoginOutcome::RateLimited => "rate_limited",
        }
    }
}

/// Record a web-server login attempt in `web_session_log`.
///
/// Best-effort: a DB failure must never block or fail a login. Errors are
/// logged via `tracing::warn!` and swallowed. Never stores the PIN or its hash.
pub fn log_login_attempt(
    conn: &rusqlite::Connection,
    ip: &str,
    user_agent: Option<&str>,
    method: WebAuthMethod,
    outcome: LoginOutcome,
) {
    let entry = crate::models::WebSessionEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        ip: ip.to_string(),
        method: method.as_str().to_string(),
        outcome: outcome.as_str().to_string(),
        user_agent: user_agent.map(|s| s.to_string()),
    };
    if let Err(e) = db::insert_web_session_log(conn, &entry) {
        tracing::warn!(error = %e, "failed to record web login attempt");
        return;
    }
    let _ = db::prune_web_session_log(conn, 5000, 90);
}

/// Auth middleware — checks for valid session or Basic Auth PIN.
pub async fn auth_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<WebState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();

    // Allow unauthenticated access to login-related routes, health check, and
    // static shell assets (Item 9: manifest/sw/icons must also be public — a
    // PIN-protected setup would otherwise 401 the PWA shell before the user
    // ever logs in). Finding 11: the shell-asset paths are shared with
    // web_ui::routes() via PUBLIC_SHELL_ASSETS rather than duplicated here.
    if path == "/api/auth"
        || path == "/api/health"
        || super::web_ui::PUBLIC_SHELL_ASSETS.contains(&path)
    {
        return next.run(req).await;
    }

    // If no PIN is configured, allow open access (user hasn't set up auth yet).
    // Poisoned mutex → fail closed (500), never open access.
    let has_pin = match state.pin_hash.lock() {
        Ok(guard) => guard.is_some(),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();
        }
    };

    if !has_pin {
        return next.run(req).await;
    }

    // Check bearer token (from header or cookie)
    if let Some(token) = extract_bearer(&req).or_else(|| extract_cookie_token(&req)) {
        if validate_session(&state, &token) {
            return next.run(req).await;
        }
    }

    // Check HTTP Basic Auth (for OPDS clients) — rate-limited like /api/auth
    if let Some(pin) = extract_basic_pin(&req) {
        let client_ip = addr.ip().to_string();
        let user_agent = req
            .headers()
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if !state.login_limiter.attempt(&client_ip) {
            if let Ok(conn) = state.conn() {
                log_login_attempt(
                    &conn,
                    &client_ip,
                    user_agent.as_deref(),
                    WebAuthMethod::Basic,
                    LoginOutcome::RateLimited,
                );
            }
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many login attempts. Try again later.",
            )
                .into_response();
        }

        let valid = state
            .pin_hash
            .lock()
            .ok()
            .and_then(|h| h.as_ref().map(|hash| verify_pin(&pin, hash)))
            .unwrap_or(false);

        if valid {
            state.login_limiter.clear(&client_ip);
            return next.run(req).await;
        }

        // Basic-Auth credential present but invalid — record the failure.
        if let Ok(conn) = state.conn() {
            log_login_attempt(
                &conn,
                &client_ip,
                user_agent.as_deref(),
                WebAuthMethod::Basic,
                LoginOutcome::InvalidPin,
            );
        }
    }

    (StatusCode::UNAUTHORIZED, "Authentication required").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    fn test_state() -> WebState {
        // Create a minimal WebState for testing (no real DB needed for auth tests)
        let pool =
            crate::db::create_pool(&std::path::PathBuf::from(":memory:")).expect("in-memory DB");
        WebState {
            pool: Arc::new(Mutex::new(pool)),
            data_dir: std::path::PathBuf::from("/tmp"),
            cache_dir: std::env::temp_dir(),
            pin_hash: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            login_limiter: Arc::new(RateLimiter::new(5, 300)),
            active_profile_name: Arc::new(Mutex::new("default".to_string())),
            unlocked_profiles: Arc::new(Mutex::new(HashSet::from(["default".to_string()]))),
            private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
        }
    }

    #[test]
    fn test_hash_pin_deterministic() {
        let h1 = hash_pin("1234");
        let h2 = hash_pin("1234");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_pin_different_inputs() {
        let h1 = hash_pin("1234");
        let h2 = hash_pin("5678");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_verify_pin_correct() {
        let hash = hash_pin("1234");
        assert!(verify_pin("1234", &hash));
    }

    #[test]
    fn test_verify_pin_wrong() {
        let hash = hash_pin("1234");
        assert!(!verify_pin("9999", &hash));
    }

    #[test]
    fn test_create_and_validate_session() {
        let state = test_state();
        let token = create_session(&state).unwrap();
        assert!(validate_session(&state, &token));
    }

    #[test]
    fn test_validate_session_unknown_token() {
        let state = test_state();
        assert!(!validate_session(&state, "nonexistent-token"));
    }

    #[test]
    fn test_generate_qr_svg() {
        let svg = generate_qr_svg("http://192.168.1.10:7788").unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn test_pin_hash_in_state() {
        let state = test_state();
        let hash = hash_pin("4321");
        *state.pin_hash.lock().unwrap() = Some(hash.clone());

        let stored = state.pin_hash.lock().unwrap();
        assert_eq!(stored.as_ref().unwrap(), &hash);
        assert!(verify_pin("4321", stored.as_ref().unwrap()));
    }

    // R2-2: Rate limiting
    #[test]
    fn test_rate_limiter_allows_initial_attempts() {
        let limiter = RateLimiter::new(3, 300);
        let ip = "192.168.1.10".to_string();
        assert!(limiter.attempt(&ip));
        assert!(limiter.attempt(&ip));
        assert!(limiter.attempt(&ip));
    }

    #[test]
    fn test_rate_limiter_blocks_after_max() {
        let limiter = RateLimiter::new(3, 300);
        let ip = "192.168.1.10".to_string();
        assert!(limiter.attempt(&ip));
        assert!(limiter.attempt(&ip));
        assert!(limiter.attempt(&ip));
        assert!(!limiter.attempt(&ip)); // 4th attempt blocked
    }

    #[test]
    fn test_rate_limiter_independent_per_ip() {
        let limiter = RateLimiter::new(2, 300);
        let ip1 = "192.168.1.10".to_string();
        let ip2 = "192.168.1.11".to_string();
        assert!(limiter.attempt(&ip1));
        assert!(limiter.attempt(&ip1));
        assert!(!limiter.attempt(&ip1));
        assert!(limiter.attempt(&ip2)); // different IP, not blocked
    }

    #[test]
    fn test_rate_limiter_clear_resets() {
        let limiter = RateLimiter::new(2, 300);
        let ip = "192.168.1.10".to_string();
        assert!(limiter.attempt(&ip));
        assert!(limiter.attempt(&ip));
        assert!(!limiter.attempt(&ip));
        limiter.clear(&ip);
        assert!(limiter.attempt(&ip)); // allowed again after clear
    }

    // R2-5: create_session should return Result
    #[test]
    fn test_create_session_returns_result() {
        let state = test_state();
        let result = create_session(&state);
        assert!(result.is_ok());
        let token = result.unwrap();
        assert!(!token.is_empty());
        assert!(validate_session(&state, &token));
    }

    #[test]
    fn test_validate_pin_rejects_short() {
        assert!(validate_pin("12").is_err());
        assert!(validate_pin("123").is_err());
    }

    #[test]
    fn test_validate_pin_rejects_non_digits() {
        assert!(validate_pin("abcd").is_err());
        assert!(validate_pin("12ab").is_err());
    }

    #[test]
    fn test_validate_pin_rejects_common() {
        assert!(validate_pin("1234").is_err());
        assert!(validate_pin("0000").is_err());
    }

    #[test]
    fn test_validate_pin_rejects_all_same() {
        assert!(validate_pin("5555").is_err());
        assert!(validate_pin("77777").is_err());
    }

    #[test]
    fn test_validate_pin_rejects_sequential() {
        assert!(validate_pin("4567").is_err());
        assert!(validate_pin("9876").is_err());
    }

    #[test]
    fn test_validate_pin_rejects_wraparound_sequential() {
        assert!(validate_pin("7890").is_err());
        assert!(validate_pin("8901").is_err());
        assert!(validate_pin("0987").is_err());
        assert!(validate_pin("1098").is_err());
    }

    #[test]
    fn test_validate_pin_rejects_repeating_pattern() {
        assert!(validate_pin("121212").is_err());
        assert!(validate_pin("343434").is_err());
        assert!(validate_pin("11221122").is_err());
        assert!(validate_pin("1313").is_err());
    }

    #[test]
    fn test_validate_pin_accepts_fair() {
        let result = validate_pin("8347");
        assert_eq!(result, Ok(PinStrength::Fair));
    }

    #[test]
    fn test_validate_pin_accepts_strong() {
        let result = validate_pin("834719");
        assert_eq!(result, Ok(PinStrength::Strong));
    }

    #[test]
    fn web_auth_method_as_str() {
        assert_eq!(WebAuthMethod::Session.as_str(), "session");
        assert_eq!(WebAuthMethod::Basic.as_str(), "basic");
    }

    #[test]
    fn login_outcome_as_str() {
        assert_eq!(LoginOutcome::Success.as_str(), "success");
        assert_eq!(LoginOutcome::InvalidPin.as_str(), "invalid_pin");
        assert_eq!(LoginOutcome::RateLimited.as_str(), "rate_limited");
    }

    #[test]
    fn log_login_attempt_inserts_row() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::init_db(&dir.path().join("t.db")).unwrap();
        log_login_attempt(
            &conn,
            "198.51.100.4",
            Some("curl/8.0"),
            WebAuthMethod::Basic,
            LoginOutcome::InvalidPin,
        );
        let rows = crate::db::get_web_session_log(&conn, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ip, "198.51.100.4");
        assert_eq!(rows[0].method, "basic");
        assert_eq!(rows[0].outcome, "invalid_pin");
        assert_eq!(rows[0].user_agent.as_deref(), Some("curl/8.0"));
    }
}
