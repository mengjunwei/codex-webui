//! 多租户认证:邮箱 + 密码(argon2)+ JWT(access sub=user_id + refresh token)。
//!
//! 与现有 `AuthService`(API key + sub="webui" JWT)并存:本模块服务多租户用户体系,
//! 旧认证保留以兼容现有功能。access JWT 复用同一 HMAC secret(由 webui_api_key 派生),
//! 用 claims.typ="mt_access" 与旧 token 区分;refresh token 为随机串,仅存 SHA-256 哈希。

use crate::error::{AppError, ErrorCode};
use crate::db::entities::{refresh_token, user};
use crate::services::multitenant::{new_id, now_ms};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::StatusCode;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::rngs::OsRng;
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// access token 有效期:15 分钟。
const ACCESS_TTL_SECS: i64 = 15 * 60;
/// refresh token 有效期:7 天。
const REFRESH_TTL_SECS: i64 = 7 * 24 * 60 * 60;
/// access token 的 typ 标识(用于和旧 sub="webui" token 区分)。
const TOKEN_TYP: &str = "mt_access";

#[derive(Serialize, Deserialize)]
struct MtClaims {
    sub: String,
    exp: usize,
    iat: usize,
    typ: String,
}

/// 登录/注册/刷新成功后返回的令牌对。
#[derive(Debug, Serialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// access token 有效期(秒)。
    pub expires_in: i64,
}

// ── 密码(argon2)──────────────────────────────────────────────────────────

/// 计算 argon2 密码哈希(PHC 字符串,含盐与参数,可直接存库)。
pub fn hash_password(plain: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| AppError::internal(format!("password hash error: {e}")))?;
    Ok(hash.to_string())
}

/// 校验明文密码与 PHC 哈希是否匹配。
pub fn verify_password(plain: &str, encoded: &str) -> bool {
    let parsed = match PasswordHash::new(encoded) {
        Ok(p) => p,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

// ── JWT ──────────────────────────────────────────────────────────────────

/// 签发 access token(sub=user_id)。
fn sign_access(user_id: &str, secret: &str) -> Result<String, AppError> {
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = MtClaims {
        sub: user_id.to_string(),
        iat: now,
        exp: now + ACCESS_TTL_SECS as usize,
        typ: TOKEN_TYP.to_string(),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::internal(format!("jwt sign error: {e}")))
}

/// 校验 access token,返回 user_id。失败返回 401。
pub fn verify_access(token: &str, secret: &str) -> Result<String, AppError> {
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    let data = decode::<MtClaims>(token, &DecodingKey::from_secret(secret.as_bytes()), &v)
        .map_err(|_| AppError::unauthorized(ErrorCode::AuthInvalidToken, "invalid access token"))?;
    if data.claims.typ != TOKEN_TYP {
        return Err(AppError::unauthorized(
            ErrorCode::AuthInvalidToken,
            "invalid token type",
        ));
    }
    Ok(data.claims.sub)
}

// ── refresh token ────────────────────────────────────────────────────────

fn generate_refresh() -> (String, String) {
    let raw = uuid::Uuid::new_v4().to_string();
    let hash = hash_refresh(&raw);
    (raw, hash)
}

fn hash_refresh(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// 为指定用户签发新的令牌对,并把 refresh 哈希落库。
pub async fn issue_tokens(
    user_id: &str,
    db: &DatabaseConnection,
    secret: &str,
) -> Result<AuthTokens, AppError> {
    let access = sign_access(user_id, secret)?;
    let (refresh_raw, refresh_hash) = generate_refresh();
    let now = now_ms();
    let am = refresh_token::ActiveModel {
        id: Set(new_id()),
        user_id: Set(user_id.to_string()),
        token_hash: Set(refresh_hash),
        expires_at: Set(now + REFRESH_TTL_SECS * 1000),
        revoked: Set(false),
        created_at: Set(now),
    };
    am.insert(db)
        .await
        .map_err(|e| AppError::internal(format!("insert refresh token: {e}")))?;

    Ok(AuthTokens {
        access_token: access,
        refresh_token: refresh_raw,
        expires_in: ACCESS_TTL_SECS,
    })
}

// ── 业务:注册 / 登录 / 刷新 ─────────────────────────────────────────────

/// 注册新用户(邮箱 + 密码)。邮箱冲突 → 409,参数非法 → 400。
pub async fn register_user(
    db: &DatabaseConnection,
    email: &str,
    password: &str,
) -> Result<user::Model, AppError> {
    let email = email.trim().to_lowercase();
    if !is_valid_email(&email) {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "invalid email".into(),
            None,
        ));
    }
    if password.len() < 8 {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "password too short (min 8)".into(),
            None,
        ));
    }

    // 查重:邮箱已注册则返回 409。
    let existing = user::Entity::find()
        .filter(user::Column::Email.eq(email.clone()))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query existing user: {e}")))?;
    if existing.is_some() {
        return Err(AppError::business(
            ErrorCode::HttpConflict,
            StatusCode::CONFLICT,
            "email already registered".into(),
            None,
        ));
    }

    let hash = hash_password(password)?;
    let now = now_ms();
    let id = new_id();
    let am = user::ActiveModel {
        id: Set(id.clone()),
        email: Set(email),
        password_hash: Set(hash),
        email_verified_at: Set(None),
        display_name: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
    };
    am.insert(db)
        .await
        .map_err(|e| AppError::internal(format!("insert user: {e}")))?;
    // insert 已返回 Model,但因 sea_orm 跨方言行为统一(避免 RETURNING 差异),显式回查一次。
    user::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("reload user: {e}")))?
        .ok_or_else(|| AppError::internal("inserted user missing on reload".into()))
}

/// 邮箱 + 密码登录,成功返回令牌对。凭据无效 → 401。
pub async fn login(
    db: &DatabaseConnection,
    secret: &str,
    email: &str,
    password: &str,
) -> Result<(user::Model, AuthTokens), AppError> {
    let email = email.trim().to_lowercase();
    let user = user::Entity::find()
        .filter(user::Column::Email.eq(email))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query user: {e}")))?;
    let user = match user {
        Some(u) => u,
        None => {
            return Err(AppError::unauthorized(
                ErrorCode::AuthInvalidToken,
                "invalid credentials",
            ))
        }
    };
    if !verify_password(password, &user.password_hash) {
        return Err(AppError::unauthorized(
            ErrorCode::AuthInvalidToken,
            "invalid credentials",
        ));
    }
    let tokens = issue_tokens(&user.id, db, secret).await?;
    Ok((user, tokens))
}

/// 用 refresh token 换新令牌对(一次性轮转:旧 refresh 撤销)。无效/过期 → 401。
pub async fn refresh_tokens(
    db: &DatabaseConnection,
    secret: &str,
    refresh_raw: &str,
) -> Result<AuthTokens, AppError> {
    let h = hash_refresh(refresh_raw);
    let now = now_ms();
    let rt = refresh_token::Entity::find()
        .filter(refresh_token::Column::TokenHash.eq(h))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query refresh token: {e}")))?;
    let rt = match rt {
        Some(r) => r,
        None => {
            return Err(AppError::unauthorized(
                ErrorCode::AuthInvalidToken,
                "invalid refresh token",
            ))
        }
    };
    if rt.revoked || rt.expires_at < now {
        return Err(AppError::unauthorized(
            ErrorCode::AuthInvalidToken,
            "refresh token expired or revoked",
        ));
    }
    // 撤销旧 refresh(转 ActiveModel 更新 revoked 字段,避免 SQL 直写)。
    let mut am: refresh_token::ActiveModel = rt.clone().into();
    am.revoked = Set(true);
    am.update(db)
        .await
        .map_err(|e| AppError::internal(format!("revoke refresh token: {e}")))?;
    issue_tokens(&rt.user_id, db, secret).await
}

// ── 辅助 ─────────────────────────────────────────────────────────────────

/// 粗略邮箱校验(M1 起步够用;后续可换更严格规则)。
fn is_valid_email(s: &str) -> bool {
    s.matches('@').count() == 1 && s.len() >= 5 && s.len() <= 255 && s.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_and_verify_roundtrip() {
        let plain = "correct horse battery staple";
        let h = hash_password(plain).unwrap();
        assert!(verify_password(plain, &h));
        assert!(!verify_password("wrong password", &h));
    }

    #[test]
    fn access_token_sign_and_verify() {
        let secret = "test-secret";
        let user_id = new_id();
        let tok = sign_access(&user_id, secret).unwrap();
        let verified = verify_access(&tok, secret).unwrap();
        assert_eq!(verified, user_id);
        // 错密钥应失败。
        assert!(verify_access(&tok, "other-secret").is_err());
    }

    #[test]
    fn refresh_hash_is_stable() {
        let (raw, h1) = generate_refresh();
        let h2 = hash_refresh(&raw);
        assert_eq!(h1, h2);
    }

    #[test]
    fn email_validation() {
        assert!(is_valid_email("a@b.co"));
        assert!(!is_valid_email("noat"));
        assert!(!is_valid_email("a@b"));
        assert!(!is_valid_email(""));
    }
}
