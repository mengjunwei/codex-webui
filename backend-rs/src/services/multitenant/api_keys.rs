//! team OpenAI API key 管理(BYOK):AES-256-GCM 加密存储 + 有效性验证 + 设置/查询。
//!
//! 注入 codex(写入 per-team CODEX_HOME/auth.json)在 M3 与进程池一起实现;
//! 本模块只负责 key 的安全存储与校验。主密钥由 `MASTER_KEY`(回退 webui_api_key)
//! 经 SHA-256 派生 32 字节,密文格式 hex(nonce || ciphertext)。

use crate::error::{AppError, ErrorCode};
use crate::db::entities::team_api_key;
use crate::services::multitenant::{new_id, now_ms};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use axum::http::StatusCode;
use rand::RngCore;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use sha2::{Digest, Sha256};

// ── 加解密 ───────────────────────────────────────────────────────────────

/// 加密明文 key,返回 hex(nonce || ciphertext)。
pub fn encrypt_key(plain: &str, master: &str) -> Result<String, AppError> {
    let key_bytes = Sha256::digest(master.as_bytes());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plain.as_bytes())
        .map_err(|e| AppError::internal(format!("encrypt api key: {e}")))?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(hex::encode(&out))
}

/// 解密 hex(nonce || ciphertext) 为明文 key。
pub fn decrypt_key(encoded: &str, master: &str) -> Result<String, AppError> {
    let key_bytes = Sha256::digest(master.as_bytes());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let raw = hex::decode(encoded).map_err(|e| AppError::internal(format!("hex decode: {e}")))?;
    if raw.len() < 13 {
        return Err(AppError::internal("ciphertext too short".into()));
    }
    let (nonce_bytes, ct) = raw.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let pt = cipher
        .decrypt(nonce, ct)
        .map_err(|e| AppError::internal(format!("decrypt api key: {e}")))?;
    String::from_utf8(pt).map_err(|e| AppError::internal(format!("utf8 decode: {e}")))
}

/// 明文 key 的尾 4 位提示(UI 显示用,如 …1234)。
pub fn key_hint(plain: &str) -> String {
    let n = plain.len();
    if n <= 4 {
        "…".into()
    } else {
        format!("…{}", &plain[n - 4..])
    }
}

// ── key 校验(调 OpenAI /v1/models)───────────────────────────────────────

/// 调 OpenAI 验证 key 有效性。无效或网络错误 → Err。
/// `base_url` 可选:为 None 时默认 https://api.openai.com;
/// 有值时使用配置的模型代理地址(如本地代理 127.0.0.1:15721)。
pub async fn validate_openai_key(key: &str, base_url: Option<&str>) -> Result<(), AppError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::internal(format!("http client: {e}")))?;
    let url = format!("{}/v1/models", base_url.unwrap_or("https://api.openai.com"));
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| {
            AppError::business(
                ErrorCode::HttpRequestFailed,
                StatusCode::BAD_GATEWAY,
                format!("validate key network error: {e}"),
                None,
            )
        })?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(AppError::business(
            ErrorCode::AuthInvalidApiKey,
            StatusCode::BAD_REQUEST,
            format!("API rejected the key (HTTP {})", resp.status().as_u16()),
            None,
        ))
    }
}

// ── 持久化 ───────────────────────────────────────────────────────────────

/// 设置 team 的 active key:验证 → 加密 → 旧的置 inactive → 插入新 active(事务)。
///
/// SeaORM 1.1 跨方言一致策略:在事务内查出所有 active key,
/// 逐条转 ActiveModel 翻 is_active=false 后 update;再 insert 新行。
/// 避免依赖方言特定的批量 UPDATE … SET is_active=FALSE(部分方言不支持)。
pub async fn set_team_api_key(
    db: &DatabaseConnection,
    team_id: &str,
    set_by: &str,
    plain_key: &str,
    provider: &str,
    master: &str,
) -> Result<team_api_key::Model, AppError> {
    // 验证 key 有效性(best-effort):调配置的模型代理 base_url(如有),否则默认 OpenAI。
    // 网络错误/本地代理不可达时记录警告但允许设置(本地代理不需要真实 OpenAI key)。
    let settings = crate::services::settings::SettingsReader::new(db, None);
    let base_url = settings.get_string("general.modelProviderBaseUrl").await;
    if let Err(e) = validate_openai_key(plain_key, base_url.as_deref()).await {
        tracing::warn!(error = %e, "API key validation failed (allowing anyway for local/custom providers)");
    }
    let enc = encrypt_key(plain_key, master)?;
    let hint = key_hint(plain_key);
    let now = now_ms();
    let id = new_id();
    let provider = if provider.trim().is_empty() {
        "openai"
    } else {
        provider.trim()
    };

    let k = db
        .transaction(|txn| {
            let id = id.clone();
            let enc = enc.clone();
            let hint = hint.clone();
            let set_by = set_by.to_string();
            let team_id = team_id.to_string();
            let provider = provider.to_string();
            Box::pin(async move {
                // 先把该 team 所有 active 行转 inactive(逐条 update,跨方言一致)。
                let active_rows = team_api_key::Entity::find()
                    .filter(team_api_key::Column::TeamId.eq(team_id.clone()))
                    .filter(team_api_key::Column::IsActive.eq(true))
                    .all(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("query active keys: {e}")))?;
                for row in active_rows {
                    let mut am: team_api_key::ActiveModel = row.into();
                    am.is_active = Set(false);
                    am.updated_at = Set(now);
                    ActiveModelTrait::update(am, txn)
                        .await
                        .map_err(|e| AppError::internal(format!("deactivate old key: {e}")))?;
                }
                let am = team_api_key::ActiveModel {
                    id: Set(id),
                    team_id: Set(team_id),
                    provider: Set(provider),
                    encrypted_key: Set(enc),
                    key_hint: Set(hint),
                    set_by: Set(set_by),
                    is_active: Set(true),
                    created_at: Set(now),
                    updated_at: Set(now),
                };
                am.insert(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("insert api key: {e}")))
            })
        })
        .await
        .map_err(|e| AppError::internal(format!("tx: {e}")))?;
    Ok(k)
}

/// 列出 team 的全部 key(历史 + active,按 created_at 倒序)。
pub async fn list_team_api_keys(
    db: &DatabaseConnection,
    team_id: &str,
) -> Result<Vec<team_api_key::Model>, AppError> {
    team_api_key::Entity::find()
        .filter(team_api_key::Column::TeamId.eq(team_id.to_string()))
        .order_by_desc(team_api_key::Column::CreatedAt)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list api keys: {e}")))
}

/// 取 team 当前 active key 并解密明文(供注入 codex 用)。
/// 密钥轮转(M6):解密优先用当前 master,失败回退 `master_previous`(旧 master 加密的 key)。
pub async fn get_active_plain_key(
    db: &DatabaseConnection,
    team_id: &str,
    master: &str,
    master_previous: Option<&str>,
) -> Result<Option<String>, AppError> {
    let row = team_api_key::Entity::find()
        .filter(team_api_key::Column::TeamId.eq(team_id.to_string()))
        .filter(team_api_key::Column::IsActive.eq(true))
        .order_by_desc(team_api_key::Column::CreatedAt)
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query active key: {e}")))?;
    match row {
        Some(r) => {
            let enc = r.encrypted_key.as_str();
            let plain = decrypt_key(enc, master)
                .ok()
                .or_else(|| master_previous.and_then(|p| decrypt_key(enc, p).ok()))
                .ok_or_else(|| {
                    AppError::internal("decrypt api key failed (master key rotation?)".into())
                })?;
            Ok(Some(plain))
        }
        None => Ok(None),
    }
}

// ── User API Key (个人 BYOK) ────────────────────────────────────────────────

use crate::db::entities::user_api_key;

/// 设置用户个人 active key:验证 → 加密 → 旧的置 inactive → 插入新 active(事务)。
pub async fn set_user_api_key(
    db: &DatabaseConnection,
    user_id: &str,
    plain_key: &str,
    provider: &str,
    master: &str,
) -> Result<user_api_key::Model, AppError> {
    let settings = crate::services::settings::SettingsReader::new(db, None);
    let base_url = settings.get_string("general.modelProviderBaseUrl").await;
    if let Err(e) = validate_openai_key(plain_key, base_url.as_deref()).await {
        tracing::warn!(error = %e, "User API key validation failed (allowing anyway for local/custom providers)");
    }
    let enc = encrypt_key(plain_key, master)?;
    let hint = key_hint(plain_key);
    let now = now_ms();
    let id = new_id();
    let provider = if provider.trim().is_empty() {
        "openai"
    } else {
        provider.trim()
    };

    let k = db
        .transaction(|txn| {
            let id = id.clone();
            let enc = enc.clone();
            let hint = hint.clone();
            let user_id = user_id.to_string();
            let provider = provider.to_string();
            Box::pin(async move {
                // 先把该用户所有 active 行转 inactive。
                let active_rows = user_api_key::Entity::find()
                    .filter(user_api_key::Column::UserId.eq(user_id.clone()))
                    .filter(user_api_key::Column::IsActive.eq(true))
                    .all(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("query active user keys: {e}")))?;
                for row in active_rows {
                    let mut am: user_api_key::ActiveModel = row.into();
                    am.is_active = Set(false);
                    am.updated_at = Set(now);
                    ActiveModelTrait::update(am, txn)
                        .await
                        .map_err(|e| AppError::internal(format!("deactivate old user key: {e}")))?;
                }
                let am = user_api_key::ActiveModel {
                    id: Set(id),
                    user_id: Set(user_id),
                    provider: Set(provider),
                    encrypted_key: Set(enc),
                    key_hint: Set(hint),
                    is_active: Set(true),
                    created_at: Set(now),
                    updated_at: Set(now),
                };
                am.insert(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("insert user api key: {e}")))
            })
        })
        .await
        .map_err(|e| AppError::internal(format!("tx: {e}")))?;
    Ok(k)
}

/// 取用户当前 active key 并解密明文(供个人 workspace 注入 codex 用)。
pub async fn get_user_active_plain_key(
    db: &DatabaseConnection,
    user_id: &str,
    master: &str,
    master_previous: Option<&str>,
) -> Result<Option<String>, AppError> {
    let row = user_api_key::Entity::find()
        .filter(user_api_key::Column::UserId.eq(user_id.to_string()))
        .filter(user_api_key::Column::IsActive.eq(true))
        .order_by_desc(user_api_key::Column::CreatedAt)
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query active user key: {e}")))?;
    match row {
        Some(r) => {
            let enc = r.encrypted_key.as_str();
            let plain = decrypt_key(enc, master)
                .ok()
                .or_else(|| master_previous.and_then(|p| decrypt_key(enc, p).ok()))
                .ok_or_else(|| {
                    AppError::internal("decrypt user api key failed".into())
                })?;
            Ok(Some(plain))
        }
        None => Ok(None),
    }
}

/// 列出用户的全部 key(历史 + active)。
pub async fn list_user_api_keys(
    db: &DatabaseConnection,
    user_id: &str,
) -> Result<Vec<user_api_key::Model>, AppError> {
    user_api_key::Entity::find()
        .filter(user_api_key::Column::UserId.eq(user_id.to_string()))
        .order_by_desc(user_api_key::Column::CreatedAt)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list user api keys: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let master = "team-master-secret";
        let plain = "sk-test-1234567890abcdef";
        let enc = encrypt_key(plain, master).unwrap();
        assert_ne!(enc, plain, "ciphertext must differ from plaintext");
        assert_eq!(decrypt_key(&enc, master).unwrap(), plain);
    }

    #[test]
    fn decrypt_with_wrong_master_fails() {
        let enc = encrypt_key("sk-abc", "master1").unwrap();
        assert!(decrypt_key(&enc, "master2").is_err());
    }

    #[test]
    fn key_hint_tail() {
        assert_eq!(key_hint("sk-abcdefgh1234"), "…1234");
        assert_eq!(key_hint("ab"), "…");
    }
}
