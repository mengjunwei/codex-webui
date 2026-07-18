//! CODEX_HOME 快照备份/恢复(M4 故障恢复)。
//!
//! 周期(RPO)备份 per-team CODEX_HOME;worker 故障迁移到新机后,从快照恢复 CODEX_HOME,
//! 再懒启动 codex 进程(thread/resume 续接)。
//!
//! 存储后端抽象(`StorageBackend`):
//! - `LocalBackend`:本地目录递归拷贝(默认;`SNAPSHOTS_ROOT`)。
//! - `S3Backend`:打包 tar.gz,经 reqwest + AWS Signature V4 上传/下载到 S3/MinIO
//!   (配置 `S3_*` 启用;不引入额外 crate,签名自实现)。
//!
//! 注:codex SQLite(WAL)在线打包可能不一致;严格在线 backup 需 sqlite 库,起步不引入,
//! 失败可接受(rollout 主线已在)。auth.json 不含(当前用 env 注入,恢复时重注入)。

use crate::error::AppError;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;

type HmacSha256 = Hmac<Sha256>;

/// 快照存储后端抽象。
#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn backup_team(&self, team_id: &str, local_codex_home: &Path) -> Result<(), AppError>;
    async fn restore_team(&self, team_id: &str, local_codex_home: &Path) -> Result<(), AppError>;
}

// ── 本地后端 ────────────────────────────────────────────────────────────

/// 本地目录后端:`{root}/{team_id}/.codex` 递归拷贝。
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl StorageBackend for LocalBackend {
    async fn backup_team(&self, team_id: &str, local: &Path) -> Result<(), AppError> {
        if !local.exists() {
            return Ok(());
        }
        let dst = self.root.join(team_id).join(".codex");
        copy_dir_all(local, &dst)
            .await
            .map_err(|e| AppError::internal(format!("local backup {team_id}: {e}")))?;
        Ok(())
    }

    async fn restore_team(&self, team_id: &str, local: &Path) -> Result<(), AppError> {
        let src = self.root.join(team_id).join(".codex");
        if !src.exists() {
            return Ok(());
        }
        copy_dir_all(&src, local)
            .await
            .map_err(|e| AppError::internal(format!("local restore {team_id}: {e}")))?;
        Ok(())
    }
}

// ── S3 / MinIO 后端(reqwest + AWS Sig V4)──────────────────────────────────

/// 最小 S3 客户端(支持 PUT/GET object,AWS Signature V4 签名;不依赖外部 crate)。
struct S3Client {
    endpoint: String, // 如 https://s3.minio.local:9000(无尾斜杠)
    host: String,     // endpoint 的 host[:port](签名用)
    region: String,
    access_key: String,
    secret_key: String,
    http: reqwest::Client,
}

impl S3Client {
    fn new(endpoint: String, region: String, access_key: String, secret_key: String) -> Self {
        let host = endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        Self {
            endpoint,
            host,
            region,
            access_key,
            secret_key,
            http: reqwest::Client::new(),
        }
    }

    async fn put_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<u16, AppError> {
        let url = format!("{}/{}/{}", self.endpoint, bucket, key);
        let ts = chrono::Utc::now();
        let date_long = ts.format("%Y%m%dT%H%M%SZ").to_string();
        let date_short = ts.format("%Y%m%d").to_string();
        let payload_hash = hex::encode(Sha256::digest(&body));
        let auth = aws_sig_v4(
            "PUT",
            &format!("/{bucket}/{key}"),
            &self.host,
            &self.region,
            &self.access_key,
            &self.secret_key,
            &payload_hash,
            &date_long,
            &date_short,
        );
        let resp = self
            .http
            .put(&url)
            .header("x-amz-date", &date_long)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .body(body)
            .send()
            .await
            .map_err(|e| AppError::internal(format!("s3 put send: {e}")))?;
        Ok(resp.status().as_u16())
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>, AppError> {
        let url = format!("{}/{}/{}", self.endpoint, bucket, key);
        let ts = chrono::Utc::now();
        let date_long = ts.format("%Y%m%dT%H%M%SZ").to_string();
        let date_short = ts.format("%Y%m%d").to_string();
        let payload_hash = hex::encode(Sha256::digest(b""));
        let auth = aws_sig_v4(
            "GET",
            &format!("/{bucket}/{key}"),
            &self.host,
            &self.region,
            &self.access_key,
            &self.secret_key,
            &payload_hash,
            &date_long,
            &date_short,
        );
        let resp = self
            .http
            .get(&url)
            .header("x-amz-date", &date_long)
            .header("x-amz-content-sha256", &payload_hash)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| AppError::internal(format!("s3 get send: {e}")))?;
        let status = resp.status().as_u16();
        if status == 404 || status >= 300 {
            return Ok(None);
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::internal(format!("s3 get body: {e}")))?
            .to_vec();
        Ok(Some(bytes))
    }
}

/// S3/MinIO 后端。配置 `S3_ENDPOINT` / `S3_BUCKET` / `S3_REGION` / `S3_ACCESS_KEY` /
/// `S3_SECRET_KEY` / `S3_PREFIX`。缺必需项则构造失败 → 调用方回退本地。
pub struct S3Backend {
    client: S3Client,
    bucket: String,
    prefix: String,
}

impl S3Backend {
    pub fn from_env() -> Result<Self, AppError> {
        let endpoint = env_required("S3_ENDPOINT")?;
        let bucket = env_required("S3_BUCKET")?;
        let region = std::env::var("S3_REGION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());
        let access = env_required("S3_ACCESS_KEY")?;
        let secret = env_required("S3_SECRET_KEY")?;
        let prefix = std::env::var("S3_PREFIX")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "codex-teams".to_string());
        let endpoint = if endpoint.ends_with('/') {
            endpoint.trim_end_matches('/').to_string()
        } else {
            endpoint
        };
        let client = S3Client::new(endpoint, region, access, secret);
        Ok(Self {
            client,
            bucket,
            prefix,
        })
    }

    fn key(&self, team_id: &str) -> String {
        format!("{}/{team_id}.tar.gz", self.prefix)
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    async fn backup_team(&self, team_id: &str, local: &Path) -> Result<(), AppError> {
        if !local.exists() {
            return Ok(());
        }
        let src = local.to_path_buf();
        let bytes = tokio::task::spawn_blocking(move || tar_gz_dir_sync(&src))
            .await
            .map_err(|e| AppError::internal(format!("tar join: {e}")))?
            .map_err(|e| AppError::internal(format!("tar pack: {e}")))?;
        let code = self
            .client
            .put_object(&self.bucket, &self.key(team_id), bytes)
            .await?;
        if code >= 300 {
            return Err(AppError::internal(format!("s3 put status {code}")));
        }
        Ok(())
    }

    async fn restore_team(&self, team_id: &str, local: &Path) -> Result<(), AppError> {
        let bytes = match self.client.get_object(&self.bucket, &self.key(team_id)).await? {
            Some(b) => b,
            None => return Ok(()), // 无快照 → 跳过(新 team 从空开始)。
        };
        let dst = local.to_path_buf();
        tokio::task::spawn_blocking(move || untar_gz_bytes_sync(&bytes, &dst))
            .await
            .map_err(|e| AppError::internal(format!("untar join: {e}")))?
            .map_err(|e| AppError::internal(format!("untar: {e}")))?;
        Ok(())
    }
}

fn env_required(name: &str) -> Result<String, AppError> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::internal(format!("{name} not set")))
}

// ── AWS Signature V4(自实现,仅 S3 PUT/GET 所需)───────────────────────────

#[allow(clippy::too_many_arguments)]
fn aws_sig_v4(
    method: &str,
    path: &str,
    host: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    payload_hash: &str,
    date_long: &str,
    date_short: &str,
) -> String {
    let canonical_headers = format!(
        "host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{date_long}\n"
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{date_short}/{region}/s3/aws4_request");
    let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{date_long}\n{scope}\n{canonical_hash}");
    let signing_key = derive_signing_key(secret_key, date_short, region);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

fn derive_signing_key(secret_key: &str, date_short: &str, region: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_short.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

// ── 打包 / 解包(同步,放 spawn_blocking)──────────────────────────────────

fn tar_gz_dir_sync(src: &Path) -> std::io::Result<Vec<u8>> {
    let buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(".", src)?;
    let enc = tar.into_inner()?;
    enc.finish()
}

fn untar_gz_bytes_sync(bytes: &[u8], dst: &Path) -> std::io::Result<()> {
    let dec = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dst)
}

// ── Snapshotter(委托后端)───────────────────────────────────────────────

pub struct Snapshotter {
    teams_root: PathBuf,
    backend: Box<dyn StorageBackend>,
}

impl Snapshotter {
    /// 按环境构造:S3 必需变量齐全 → S3Backend;否则 LocalBackend。
    pub fn from_env(teams_root: PathBuf, snapshots_root: PathBuf) -> Self {
        let backend: Box<dyn StorageBackend> = match S3Backend::from_env() {
            Ok(s3) => {
                tracing::info!("snapshot backend: S3/MinIO");
                Box::new(s3)
            }
            Err(_) => {
                tracing::info!("snapshot backend: local dir");
                Box::new(LocalBackend::new(snapshots_root))
            }
        };
        Self { teams_root, backend }
    }

    pub async fn backup_team(&self, team_id: &str) -> Result<(), AppError> {
        let local = self.teams_root.join(team_id).join(".codex");
        self.backend.backup_team(team_id, &local).await
    }

    pub async fn restore_team(&self, team_id: &str) -> Result<(), AppError> {
        let local = self.teams_root.join(team_id).join(".codex");
        self.backend.restore_team(team_id, &local).await
    }

    /// 备份 teams_root 下所有 team 目录,返回已备份数量(单 team 失败不阻塞)。
    pub async fn backup_all(&self) -> Result<usize, AppError> {
        let mut count = 0usize;
        let mut entries = match fs::read_dir(&self.teams_root).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(AppError::internal(format!("read teams_root: {e}"))),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| AppError::internal(format!("read entry: {e}")))?
        {
            if entry
                .file_type()
                .await
                .map_err(|e| AppError::internal(format!("file type: {e}")))?
                .is_dir()
            {
                if let Some(name) = entry.file_name().to_str() {
                    if let Err(e) = self.backup_team(name).await {
                        tracing::warn!(team_id = name, error = %e, "snapshot failed (non-fatal)");
                    } else {
                        count += 1;
                    }
                }
            }
        }
        Ok(count)
    }
}

/// 递归拷贝目录(async;递归调用需 Box::pin)。
async fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst).await?;
    let mut entries = fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            Box::pin(copy_dir_all(&from, &to)).await?;
        } else if ft.is_symlink() {
            // 不跟随 symlink:fs::copy / fs::metadata 默认跟随,会把指向 codex_home 外的
            // 链接目标实体化进快照 → 越权读取外部文件 + 快照膨胀(与 files copy_dir_recursive
            // Bug#21 同类)。Unix 重建相对链接(跳过绝对链接),Windows 跳过。
            #[cfg(unix)]
            {
                match fs::read_link(&from).await {
                    Ok(target) if target.is_absolute() => {
                        tracing::warn!(
                            from = %from.display(),
                            target = %target.display(),
                            "skipping absolute symlink in snapshot copy"
                        );
                    }
                    Ok(target) => {
                        let _ = fs::symlink(&target, &to).await;
                    }
                    Err(_) => {}
                }
            }
            #[cfg(not(unix))]
            {
                tracing::warn!(
                    from = %from.display(),
                    "skipping symlink in snapshot copy (windows)"
                );
            }
        } else {
            let _ = fs::copy(&from, &to).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_backup_then_restore_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("snap-test-{}", uuid::Uuid::new_v4()));
        let teams = tmp.join("teams");
        let snaps = tmp.join("snaps");
        let team_home = teams.join("t1").join(".codex");
        fs::create_dir_all(team_home.join("sessions")).await.unwrap();
        fs::write(
            team_home.join("sessions").join("rollout.jsonl"),
            "line1\nline2\n",
        )
        .await
        .unwrap();

        let s = Snapshotter {
            teams_root: teams.clone(),
            backend: Box::new(LocalBackend::new(snaps.clone())),
        };
        s.backup_team("t1").await.unwrap();
        assert!(snaps.join("t1").join(".codex").join("sessions").join("rollout.jsonl").exists());

        fs::remove_dir_all(teams.join("t1")).await.unwrap();
        s.restore_team("t1").await.unwrap();
        let restored = fs::read_to_string(team_home.join("sessions").join("rollout.jsonl"))
            .await
            .unwrap();
        assert_eq!(restored, "line1\nline2\n");

        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn backup_missing_team_is_noop() {
        let tmp = std::env::temp_dir().join(format!("snap-test2-{}", uuid::Uuid::new_v4()));
        let s = Snapshotter {
            teams_root: tmp.join("teams"),
            backend: Box::new(LocalBackend::new(tmp.join("snaps"))),
        };
        assert!(s.backup_team("nope").await.is_ok());
        assert_eq!(s.backup_all().await.unwrap(), 0);
        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn tar_roundtrip_in_memory() {
        let tmp = std::env::temp_dir().join(format!("snap-tar-{}", uuid::Uuid::new_v4()));
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::write(src.join("d").join("f.txt"), b"hello").unwrap();
        let bytes = tar_gz_dir_sync(&src).unwrap();
        let dst = tmp.join("dst");
        untar_gz_bytes_sync(&bytes, &dst).unwrap();
        let got = std::fs::read(dst.join("d").join("f.txt")).unwrap();
        assert_eq!(got, b"hello");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn signing_key_is_deterministic() {
        let a = derive_signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCY", "20260717", "us-east-1");
        let b = derive_signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCY", "20260717", "us-east-1");
        assert_eq!(a, b);
        let c = derive_signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCY", "20260717", "us-west-2");
        assert_ne!(a, c, "different region → different key");
    }
}
