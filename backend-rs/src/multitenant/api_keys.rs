//! team OpenAI API key 管理(BYOK):AES-256-GCM 加密存储 + 有效性验证 + 设置/查询。
//!
//! 注入 codex(写入 per-team CODEX_HOME/auth.json)在 M3 与进程池一起实现;
//! 本模块只负责 key 的安全存储与校验。主密钥由 `MASTER_KEY`(回退 webui_api_key)
//! 经 SHA-256 派生 32 字节,密文格式 hex(nonce || ciphertext)。

use crate::error::{AppError, ErrorCode};
use crate::multitenant::models::TeamApiKey;
use crate::multitenant::{new_id, now_ms};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use axum::http::StatusCode;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const API_KEY_COLUMNS: &str =
    "id, team_id, provider, encrypted_key, key_hint, set_by, is_active, created_at, updated_at";

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
pub async fn validate_openai_key(key: &str) -> Result<(), AppError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::internal(format!("http client: {e}")))?;
    let resp = client
        .get("https://api.openai.com/v1/models")
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
            format!("OpenAI rejected the key (HTTP {})", resp.status().as_u16()),
            None,
        ))
    }
}

// ── 持久化 ───────────────────────────────────────────────────────────────

/// 设置 team 的 active key:验证 → 加密 → 旧的置 inactive → 插入新 active(事务)。
pub async fn set_team_api_key(
    pool: &PgPool,
    team_id: &str,
    set_by: &str,
    plain_key: &str,
    provider: &str,
    master: &str,
) -> Result<TeamApiKey, AppError> {
    validate_openai_key(plain_key).await?;
    let enc = encrypt_key(plain_key, master)?;
    let hint = key_hint(plain_key);
    let now = now_ms();
    let id = new_id();
    let provider = if provider.trim().is_empty() {
        "openai"
    } else {
        provider.trim()
    };

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::internal(format!("begin tx: {e}")))?;
    sqlx::query(
        "UPDATE team_api_keys SET is_active = FALSE, updated_at = $1 \
         WHERE team_id = $2 AND is_active = TRUE",
    )
    .bind(now)
    .bind(team_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| AppError::internal(format!("deactivate old keys: {e}")))?;
    let k: TeamApiKey = sqlx::query_as(&format!(
        "INSERT INTO team_api_keys (id, team_id, provider, encrypted_key, key_hint, set_by, is_active, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, TRUE, $7, $7) RETURNING {API_KEY_COLUMNS}"
    ))
    .bind(&id)
    .bind(team_id)
    .bind(provider)
    .bind(&enc)
    .bind(&hint)
    .bind(set_by)
    .bind(now)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| AppError::internal(format!("insert api key: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| AppError::internal(format!("commit tx: {e}")))?;
    Ok(k)
}

/// 列出 team 的全部 key(历史 + active,按 created_at 倒序)。
pub async fn list_team_api_keys(pool: &PgPool, team_id: &str) -> Result<Vec<TeamApiKey>, AppError> {
    let sql = format!(
        "SELECT {API_KEY_COLUMNS} FROM team_api_keys WHERE team_id = $1 ORDER BY created_at DESC"
    );
    sqlx::query_as::<_, TeamApiKey>(&sql)
        .bind(team_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::internal(format!("list api keys: {e}")))
}

/// 取 team 当前 active key 并解密明文(供 M3 注入 codex 用)。
pub async fn get_active_plain_key(
    pool: &PgPool,
    team_id: &str,
    master: &str,
) -> Result<Option<String>, AppError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT encrypted_key FROM team_api_keys WHERE team_id = $1 AND is_active = TRUE \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(team_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::internal(format!("query active key: {e}")))?;
    match row {
        Some((enc,)) => Ok(Some(decrypt_key(&enc, master)?)),
        None => Ok(None),
    }
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
