//! AuthService —— JWT 签名/校验 + API key 校验。
//!
//! 与 `src/auth/auth.service.ts` 对齐：
//! - JWT 密钥：HMAC-SHA256(key=WEBUI_API_KEY, msg="codex-webui-jwt").hexdigest
//! - 算法：HS256 | TTL：86400s | 主题："webui"
//! - 载荷：`{ sub, iat, exp }`
//! - authenticate_token：先尝试 JWT → 若无效且 looksLikeJwt 则告警；
//!   再尝试 API key（恒定时间比较）→ fallbackAccepted；否则 → invalidToken
//!
//! `LoginResponse` / `LoginRequest` 也定义于此（DTO 与 `auth/dto/auth.dto.ts` 对齐）。

use crate::error::AppError;
use hmac::{Hmac, Mac};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const SUBJECT: &str = "webui";
const TTL_SECONDS: i64 = 24 * 60 * 60;
const SECRET_CONTEXT: &[u8] = b"codex-webui-jwt";

// ── JWT 载荷 ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
    iat: usize,
}

// ── 认证结果 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AuthResult {
    pub ok: bool,
    pub auth_type: Option<String>, // "jwt" 或 "apiKey"
}

// ── DTO（与 auth/dto/auth.dto.ts 对齐）──────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct LoginRequest {
    #[serde(rename = "apiKey")]
    pub api_key: String,
}

#[derive(Serialize, utoipa::ToSchema)]
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

    /// 暴露派生出的 JWT 密钥（供对齐测试使用）。
    pub fn jwt_secret(&self) -> &str {
        &self.jwt_secret
    }

    /// 将候选项与部署的 API key 进行恒定时间比较。
    pub fn validate_api_key(&self, candidate: &str) -> bool {
        if candidate.is_empty() {
            return false;
        }
        let a = candidate.as_bytes();
        let b = self.api_key.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        // ConstantTimeEq 在相等时返回 1；`bool::from` 会正确转换。
        a.ct_eq(b).into()
    }

    /// 为单用户 WebUI 会话签发短期 JWT。
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

    /// 校验 JWT 签名 + 过期时间。接受任何有效 JWT(单租户 sub="webui" 或多租户 sub=user_id)。
    /// WebSocket 认证只需要"token 有效",不需要区分用户类型。
    pub fn verify_jwt(&self, token: &str) -> Result<bool, AppError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
        validation.leeway = 0;

        match decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.jwt_secret.as_bytes()),
            &validation,
        ) {
            Ok(_) => Ok(true), // 签名+exp 通过即可
            Err(_) => Ok(false),
        }
    }

    /// 认证 bearer token：先 JWT，再回退到原始 API key。
    ///
    /// 与 `auth.service.ts:authenticateToken` 对齐：
    /// - JWT 校验通过 → ok：`{ ok: true, auth_type: "jwt" }`
    /// - 若 token 形似 JWT 但校验失败 → 记录 warn（不返回错误）
    /// - API key（恒定时间比较）→ `{ ok: true, auth_type: "apiKey" }`
    /// - 二者皆不匹配 → `{ ok: false }`
    pub fn authenticate_token(&self, token: Option<&str>, _request_id: Option<&str>) -> AuthResult {
        let token = match token {
            Some(t) if !t.trim().is_empty() => t.trim(),
            _ => return AuthResult { ok: false, auth_type: None },
        };

        // 优先 JWT。
        if self.verify_jwt(token).unwrap_or(false) {
            return AuthResult {
                ok: true,
                auth_type: Some("jwt".into()),
            };
        }

        // 若形似 JWT 但校验失败，记录一条警告。
        if looks_like_jwt(token) {
            tracing::warn!(auth_type = "jwt", reason = "verifyFailed", "auth");
        }

        // 回退到 API key（恒定时间比较）。
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

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 从部署的 API key 派生 JWT 签名密钥。
/// 对应 `auth.service.ts:deriveJwtSecret`：
///   `HMAC-SHA256(key=WEBUI_API_KEY, msg='codex-webui-jwt').hex`
fn derive_secret(api_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).expect("hmac key");
    mac.update(SECRET_CONTEXT);
    hex::encode(mac.finalize().into_bytes())
}

/// 当 token 恰好由 3 个以点分隔的部分组成时，即视为“形似 JWT”。
fn looks_like_jwt(token: &str) -> bool {
    token.split('.').count() == 3
}
