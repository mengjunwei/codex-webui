//! hook 审计批量入库(per-user workspace 实施步骤 5)。
//!
//! 后台 task:批大小 50,刷新间隔 1s。失败 tracing::error,不入 caller 错误路径。

use sea_orm::{ConnectionTrait, DatabaseConnection};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;

/// audit 写入器:clone 轻量,内部仅持 Sender。
#[derive(Clone)]
pub struct AuditWriter {
    tx: mpsc::Sender<AuditEvent>,
}

pub struct AuditEvent {
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub thread_id: Option<String>,
    pub event_type: String,
    pub tool_name: Option<String>,
    pub payload: Value,
    pub decision: Option<String>,
}

impl AuditWriter {
    /// 入队;队列满则丢弃(tracing::warn),不阻塞 caller。
    pub fn submit(&self, ev: AuditEvent) {
        if let Err(e) = self.tx.try_send(ev) {
            tracing::warn!(error = %e, "audit queue full; dropping event");
        }
    }
}

/// 启动后台 task,返回 AuditWriter(handle)。
pub fn spawn(db: DatabaseConnection) -> AuditWriter {
    let (tx, mut rx) = mpsc::channel::<AuditEvent>(1024);
    tokio::spawn(async move {
        let mut buf: Vec<AuditEvent> = Vec::with_capacity(64);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                maybe_ev = rx.recv() => {
                    match maybe_ev {
                        Some(ev) => {
                            buf.push(ev);
                            if buf.len() >= 50 {
                                flush(&db, &mut buf).await;
                            }
                        }
                        None => {
                            // sender drop → 刷盘后退出
                            if !buf.is_empty() {
                                flush(&db, &mut buf).await;
                            }
                            break;
                        }
                    }
                }
                _ = tick.tick() => {
                    if !buf.is_empty() {
                        flush(&db, &mut buf).await;
                    }
                }
            }
        }
        tracing::info!("audit writer exited");
    });
    AuditWriter { tx }
}

async fn flush(db: &DatabaseConnection, buf: &mut Vec<AuditEvent>) {
    if buf.is_empty() {
        return;
    }
    let drained: Vec<AuditEvent> = buf.drain(..).collect();
    let now = crate::services::multitenant::now_ms();
    let backend = db.get_database_backend();
    let sql = "INSERT INTO workspace_audit \
               (team_id, user_id, thread_id, event_type, tool_name, payload_json, decision, created_at) \
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)";
    for ev in drained {
        let payload_str = match serde_json::to_string(&ev.payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "audit payload serialize failed");
                continue;
            }
        };
        let stmt = sea_orm::Statement::from_sql_and_values(
            backend,
            sql,
            vec![
                ev.team_id.into(),
                ev.user_id.into(),
                ev.thread_id.into(),
                ev.event_type.into(),
                ev.tool_name.into(),
                payload_str.into(),
                ev.decision.into(),
                now.into(),
            ],
        );
        if let Err(e) = db.execute(stmt).await {
            tracing::error!(error = %e, "audit insert failed (dropped)");
        }
    }
}