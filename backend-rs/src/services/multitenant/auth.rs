//! 多租户认证:邮箱 + 密码(argon2)+ JWT(access sub=user_id + refresh token)。
//!
//! 与现有 `AuthService`(API key + sub="webui" JWT)并存:本模块服务多租户用户体系,
//! 旧认证保留以兼容现有功能。access JWT 复用同一 HMAC secret(由 webui_api_key 派生),
//! 用 claims.typ="mt_access" 与旧 token 区分;refresh token 为随机串,仅存 SHA-256 哈希。

use crate::error::{AppError, ErrorCode};
use crate::db::entities::{auth_token, refresh_token, user};
use crate::services::multitenant::{new_id, now_ms};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::StatusCode;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::rngs::OsRng;
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveModelTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

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
    username: &str,
    email: &str,
    password: &str,
) -> Result<user::Model, AppError> {
    let username = username.trim().to_ascii_lowercase();
    if !is_valid_username(&username) {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "invalid username".into(),
            None,
        ));
    }
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
        .filter(Condition::any().add(user::Column::Email.eq(email.clone())).add(user::Column::Username.eq(username.clone())))
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
        username: Set(username.clone()),
        email: Set(email.clone()),
        password_hash: Set(hash),
        email_verified_at: Set(None),
        display_name: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        is_platform_admin: Set(false),
    };
    if let Err(e) = am.insert(db).await {
        // 并发同邮箱注册:find-then-insert 竞态下第二个 insert 撞 email 唯一约束。
        // 重查确认是 email 冲突 → 409(而非误报 500)。
        let exists = user::Entity::find()
            .filter(user::Column::Email.eq(email))
            .one(db)
            .await
            .map_err(|e| AppError::internal(format!("re-check user: {e}")))?;
        if exists.is_some() {
            return Err(AppError::business(
                ErrorCode::HttpConflict,
                StatusCode::CONFLICT,
                "email already registered".into(),
                None,
            ));
        }
        return Err(AppError::internal(format!("insert user: {e}")));
    }
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
    identifier: &str,
    password: &str,
) -> Result<(user::Model, AuthTokens), AppError> {
    let identifier = identifier.trim().to_ascii_lowercase();
    let user = user::Entity::find()
        .filter(Condition::any().add(user::Column::Email.eq(identifier.clone())).add(user::Column::Username.eq(identifier)))
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
    // 撤销旧 refresh:用**条件 update**(WHERE id AND revoked=false)而非 ActiveModel::update。
    // 后者是无条件写,并发同 token 时两个请求都通过上方校验、都 update revoked=true、各签一对
    // 新令牌 → 一次性轮转被打破(攻击者窃取 refresh 与合法客户端竞速可获持续会话,reuse
    // 检测失效)。条件 update 的 rows_affected=0 表示已被并发 revoke → 拒绝。
    use sea_orm::sea_query::Expr;
    let res = refresh_token::Entity::update_many()
        .col_expr(refresh_token::Column::Revoked, Expr::value(true))
        .filter(refresh_token::Column::Id.eq(rt.id.clone()))
        .filter(refresh_token::Column::Revoked.eq(false))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("revoke refresh token: {e}")))?;
    if res.rows_affected == 0 {
        // 并发:已被另一个请求 revoke(一次性轮转)。重用已撤销 token 视为失窃信号。
        tracing::warn!(
            user_id = %rt.user_id,
            "refresh token reuse detected (concurrent rotate rejected)"
        );
        return Err(AppError::unauthorized(
            ErrorCode::AuthInvalidToken,
            "refresh token already used",
        ));
    }
    issue_tokens(&rt.user_id, db, secret).await
}

/// 确保内置平台管理员存在；密码仅用于启动时生成 Argon2 哈希，不写入数据库。
pub async fn ensure_builtin_admin(db: &DatabaseConnection) -> Result<(), AppError> {
    let now = now_ms();
    let password_hash = hash_password("Codex@Agent+-")?;
    if let Some(existing) = user::Entity::find()
        .filter(user::Column::Username.eq("admin"))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query builtin admin: {e}")))?
    {
        let mut model: user::ActiveModel = existing.into();
        model.email = Set("admin@codex.local".into());
        model.password_hash = Set(password_hash);
        model.is_platform_admin = Set(true);
        model.updated_at = Set(now);
        model.update(db).await.map_err(|e| AppError::internal(format!("update builtin admin: {e}")))?;
        return Ok(());
    }
    let model = user::ActiveModel {
        id: Set(new_id()),
        username: Set("admin".into()),
        email: Set("admin@codex.local".into()),
        password_hash: Set(password_hash),
        email_verified_at: Set(Some(now)),
        display_name: Set(Some("内置管理员".into())),
        created_at: Set(now),
        updated_at: Set(now),
        is_platform_admin: Set(true),
    };
    model.insert(db).await.map_err(|e| AppError::internal(format!("create builtin admin: {e}")))?;
    Ok(())
}

fn is_valid_username(s: &str) -> bool {
    (3..=64).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-_".contains(c))
}

/// Token 摘要，用于数据库查询。
fn hash_login_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// 创建个人登录 Token，明文只返回一次。
pub async fn create_login_token(
    db: &DatabaseConnection,
    user_id: &str,
    name: &str,
    expires_at: i64,
) -> Result<(auth_token::Model, String), AppError> {
    let now = now_ms();
    let name = name.trim();
    if name.is_empty() || name.len() > 128 || expires_at <= now || expires_at > now + 365 * 24 * 60 * 60 * 1000 {
        return Err(AppError::business(ErrorCode::ValidationFieldInvalid, StatusCode::BAD_REQUEST, "invalid token name or expiration".into(), None));
    }
    let raw = format!("cwx_{}", Uuid::new_v4().simple());
    let model = auth_token::ActiveModel {
        id: Set(new_id()),
        user_id: Set(user_id.to_string()),
        name: Set(name.to_string()),
        token_hash: Set(hash_login_token(&raw)),
        token_prefix: Set(raw.chars().take(12).collect()),
        created_at: Set(now),
        expires_at: Set(expires_at),
        revoked_at: Set(None),
        last_used_at: Set(None),
    };
    let saved = model.insert(db).await.map_err(|e| AppError::internal(format!("create auth token: {e}")))?;
    Ok((saved, raw))
}

/// 通过个人登录 Token 签发标准 JWT 会话。
pub async fn login_with_token(
    db: &DatabaseConnection,
    secret: &str,
    raw: &str,
) -> Result<(user::Model, AuthTokens), AppError> {
    let now = now_ms();
    let token = auth_token::Entity::find()
        .filter(auth_token::Column::TokenHash.eq(hash_login_token(raw)))
        .one(db).await.map_err(|e| AppError::internal(format!("query auth token: {e}")))?;
    let token = token.filter(|t| t.revoked_at.is_none() && t.expires_at > now).ok_or_else(|| AppError::unauthorized(ErrorCode::AuthInvalidToken, "invalid authentication token"))?;
    let mut active: auth_token::ActiveModel = token.clone().into();
    active.last_used_at = Set(Some(now));
    active.update(db).await.map_err(|e| AppError::internal(format!("update auth token: {e}")))?;
    let user = user::Entity::find_by_id(token.user_id).one(db).await.map_err(|e| AppError::internal(format!("query token user: {e}")))?.ok_or_else(|| AppError::unauthorized(ErrorCode::AuthInvalidToken, "invalid authentication token"))?;
    let tokens = issue_tokens(&user.id, db, secret).await?;
    Ok((user, tokens))
}

/// 列出用户登录 Token 元数据。
pub async fn list_login_tokens(db: &DatabaseConnection, user_id: &str) -> Result<Vec<auth_token::Model>, AppError> {
    auth_token::Entity::find().filter(auth_token::Column::UserId.eq(user_id.to_string())).order_by_desc(auth_token::Column::CreatedAt).all(db).await.map_err(|e| AppError::internal(format!("list auth tokens: {e}")))
}
/// 撤销登录 Token。
pub async fn revoke_login_token(db: &DatabaseConnection, user_id: &str, id: &str) -> Result<(), AppError> {
    use sea_orm::sea_query::Expr;
    auth_token::Entity::update_many()
        .col_expr(auth_token::Column::RevokedAt, Expr::value(now_ms()))
        .filter(auth_token::Column::Id.eq(id))
        .filter(auth_token::Column::UserId.eq(user_id))
        .filter(auth_token::Column::RevokedAt.is_null())
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("revoke auth token: {e}")))?;
    Ok(())
}



/// 邮箱校验(M1 起步够用;后续可换更严格规则)。
/// 验证规则:
/// 1. 包含且仅包含一个 @
/// 2. 长度 5-255
/// 3. @ 后面包含 .（域名部分）
/// 4. @ 前面有内容（本地部分）
/// 5. . 不在开头或结尾
fn is_valid_email(s: &str) -> bool {
    if s.len() < 5 || s.len() > 255 {
        return false;
    }
    let parts: Vec<&str> = s.split('@').collect();
    if parts.len() != 2 {
        return false;
    }
    let local = parts[0];
    let domain = parts[1];
    // 本地部分不能为空
    if local.is_empty() {
        return false;
    }
    // 域名部分必须包含 .，且不在开头或结尾
    if domain.is_empty() || !domain.contains('.') {
        return false;
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }
    true
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
        // 有效邮箱
        assert!(is_valid_email("a@b.co"));
        assert!(is_valid_email("user@example.com"));
        assert!(is_valid_email("test.email@domain.org"));
        assert!(is_valid_email("user+tag@example.com"));

        // 无效邮箱
        assert!(!is_valid_email("noat")); // 没有 @
        assert!(!is_valid_email("a@b")); // 没有 .
        assert!(!is_valid_email("")); // 空
        assert!(!is_valid_email("@b.co")); // 没有本地部分
        assert!(!is_valid_email("a@.co")); // 域名以 . 开头
        assert!(!is_valid_email("a@b.c.")); // 域名以 . 结尾
        assert!(!is_valid_email("a@b")); // TLD 太短
        assert!(!is_valid_email("a@@b.co")); // 多个 @
    }
}

#[cfg(test)]
mod auth_extra_tests {
    use super::*;

    #[test]
    fn username_validation() {
        assert!(is_valid_username("admin"));
        assert!(is_valid_username("alice.dev"));
        assert!(is_valid_username("user_name-1"));
        assert!(!is_valid_username("ab"));
        assert!(!is_valid_username("Admin"));
        assert!(!is_valid_username("has space"));
        assert!(!is_valid_username(""));
    }

    #[test]
    fn login_token_hash_stable_and_no_leak() {
        let raw = "cwx_aaaaaaaaaaaa111111111111";
        let h1 = hash_login_token(raw);
        let h2 = hash_login_token(raw);
        assert_eq!(h1, h2);
        assert!(!h1.contains(raw));
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn login_token_prefix_length() {
        let raw = "cwx_0123456789abcdef";
        let prefix: String = raw.chars().take(12).collect();
        assert_eq!(prefix, "cwx_01234567");
    }
}
