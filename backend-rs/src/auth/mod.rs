//! AuthService — JWT signing/verification + API key validation.
//!
//! Parity with `src/auth/auth.service.ts`:
//! - JWT secret: HMAC-SHA256(key=WEBUI_API_KEY, msg="codex-webui-jwt").hexdigest
//! - Algorithm: HS256 | TTL: 86400s | Subject: "webui"
//! - Payload: `{ sub, iat, exp }`
//! - authenticate_token: try JWT → if invalid + looksLikeJwt → warn;
//!   try API key (timing-safe) → fallbackAccepted; else → invalidToken
//!
//! `LoginResponse` / `LoginRequest` also live here (dto parity with `auth/dto/auth.dto.ts`).

pub mod middleware;

use crate::error::AppError;
use hmac::{Hmac, Mac};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const SUBJECT: &str = "webui";
const TTL_SECONDS: i64 = 24 * 60 * 60;
const SECRET_CONTEXT: &[u8] = b"codex-webui-jwt";

// ── JWT payload ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
    iat: usize,
}

// ── Auth result ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AuthResult {
    pub ok: bool,
    pub auth_type: Option<String>, // "jwt" or "apiKey"
}

// ── DTOs (parity with auth/dto/auth.dto.ts) ──────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    #[serde(rename = "apiKey")]
    pub api_key: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "expiresIn")]
    pub expires_in: i64,
}

// ── AuthService ──────────────────────────────────────────────────────────────

pub struct AuthService {
    api_key: String,
    jwt_secret: String,
}

impl AuthService {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            jwt_secret: derive_secret(api_key),
        }
    }

    /// Expose the derived JWT secret (used by parity tests).
    pub fn jwt_secret(&self) -> &str {
        &self.jwt_secret
    }

    /// Timing-safe comparison of the candidate against the deployment API key.
    pub fn validate_api_key(&self, candidate: &str) -> bool {
        if candidate.is_empty() {
            return false;
        }
        let a = candidate.as_bytes();
        let b = self.api_key.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        // ConstantTimeEq returns 1 on equal; `bool::from` converts correctly.
        a.ct_eq(b).into()
    }

    /// Sign a short-lived JWT for the single-user WebUI session.
    pub fn sign_jwt(&self) -> Result<LoginResponse, AppError> {
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            sub: SUBJECT.into(),
            iat: now,
            exp: now + TTL_SECONDS as usize,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .map_err(|e| AppError::internal(format!("jwt sign error: {e}")))?;

        Ok(LoginResponse {
            access_token: token,
            expires_in: TTL_SECONDS,
        })
    }

    /// Verify a JWT and confirm `sub == "webui"`. Returns `Ok(false)` on any
    /// failure rather than an error (mirrors the TS try/catch → false pattern).
    pub fn verify_jwt(&self, token: &str) -> Result<bool, AppError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.sub = Some(SUBJECT.to_string());
        validation.validate_exp = true;

        match decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.jwt_secret.as_bytes()),
            &validation,
        ) {
            Ok(data) => Ok(data.claims.sub == SUBJECT),
            Err(_) => Ok(false),
        }
    }

    /// Authenticate a bearer token: JWT first, then raw API key fallback.
    ///
    /// Parity with `auth.service.ts:authenticateToken`:
    /// - JWT verify → ok: `{ ok: true, auth_type: "jwt" }`
    /// - If token looks like JWT but failed verify → log warn (no error)
    /// - API key (timing-safe) → `{ ok: true, auth_type: "apiKey" }`
    /// - Neither → `{ ok: false }`
    pub fn authenticate_token(&self, token: Option<&str>, _request_id: Option<&str>) -> AuthResult {
        let token = match token {
            Some(t) if !t.trim().is_empty() => t.trim(),
            _ => return AuthResult { ok: false, auth_type: None },
        };

        // Prefer JWT.
        if self.verify_jwt(token).unwrap_or(false) {
            return AuthResult {
                ok: true,
                auth_type: Some("jwt".into()),
            };
        }

        // If it looks like a JWT but verification failed, log a warning.
        if looks_like_jwt(token) {
            tracing::warn!(auth_type = "jwt", reason = "verifyFailed", "auth");
        }

        // API key fallback (timing-safe).
        if self.validate_api_key(token) {
            tracing::info!(auth_type = "apiKey", reason = "fallbackAccepted", "auth");
            return AuthResult {
                ok: true,
                auth_type: Some("apiKey".into()),
            };
        }

        AuthResult {
            ok: false,
            auth_type: None,
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Derive the JWT signing secret from the deployment API key.
/// Matches `auth.service.ts:deriveJwtSecret`:
///   `HMAC-SHA256(key=WEBUI_API_KEY, msg='codex-webui-jwt').hex`
fn derive_secret(api_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).expect("hmac key");
    mac.update(SECRET_CONTEXT);
    hex::encode(mac.finalize().into_bytes())
}

/// A token "looks like a JWT" if it has exactly 3 dot-separated parts.
fn looks_like_jwt(token: &str) -> bool {
    token.split('.').count() == 3
}
