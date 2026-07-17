//! 计费/配额(M6 预留):per-team turn/token 配额校验与用量计数。
//!
//! `team_quotas` 表(team_id PK)。每次 turn 前校验、turn 完成后累加次数;
//! token 用量(event_persist)累加月度。配额为 0 表示不限(free 起步)。
//! find→update(非强原子,配额为软限制可接受)。

use crate::db::entities::team_quota::{ActiveModel, Entity, Model};
use crate::error::{AppError, ErrorCode};
use crate::services::multitenant::now_ms;
use axum::http::StatusCode;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

const HOUR_SECS: i64 = 3600;

/// 当前小时桶(Unix 毫秒,对齐到整点)。
pub fn hour_bucket_ms(now_ms: i64) -> i64 {
    let secs = now_ms / 1000;
    (secs / HOUR_SECS) * HOUR_SECS * 1000
}

/// 当前月桶字符串 `YYYY-MM`。
pub fn month_bucket(now_ms: i64) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(now_ms)
        .unwrap_or_else(Utc::now)
        .format("%Y-%m")
        .to_string()
}

/// 确保 team 有配额记录(创建 team 时调用,写入默认配额)。已存在则 no-op。
pub async fn ensure_quota_row(
    db: &DatabaseConnection,
    team_id: &str,
    default_turn_quota_hourly: i64,
) -> Result<Model, AppError> {
    if let Some(m) = Entity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query quota: {e}")))?
    {
        return Ok(m);
    }
    let now = now_ms();
    let am = ActiveModel {
        team_id: Set(team_id.to_string()),
        plan: Set("free".into()),
        turn_quota_hourly: Set(default_turn_quota_hourly),
        token_quota_monthly: Set(0),
        used_turns_hour: Set(0),
        hour_bucket: Set(hour_bucket_ms(now)),
        used_tokens_month: Set(0),
        month_bucket: Set(month_bucket(now)),
        updated_at: Set(now),
    };
    am.insert(db)
        .await
        .map_err(|e| AppError::internal(format!("insert quota: {e}")))
}

/// 校验 team 本小时 turn 配额;超限返回错误(429)。配额 0 = 不限。
/// 跨小时自动视为可用(计数会在 incr 时重置)。
pub async fn check_turn_quota(db: &DatabaseConnection, team_id: &str) -> Result<(), AppError> {
    let row = Entity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query quota: {e}")))?;
    let row = match row {
        Some(r) => r,
        None => return Ok(()), // 无配额记录 = 不限
    };
    if row.turn_quota_hourly == 0 {
        return Ok(());
    }
    let cur_bucket = hour_bucket_ms(now_ms());
    if row.hour_bucket != cur_bucket {
        return Ok(()); // 新小时,计数待重置,允许
    }
    if row.used_turns_hour >= row.turn_quota_hourly {
        return Err(AppError::business(
            ErrorCode::HttpRequestFailed,
            StatusCode::TOO_MANY_REQUESTS,
            "team hourly turn quota exceeded".into(),
            None,
        ));
    }
    Ok(())
}

/// 累加一次 turn 用量(小时桶变化时重置计数);tokens 为可选的月度 token 增量(留空则只计次数)。
pub async fn incr_turn_usage(
    db: &DatabaseConnection,
    team_id: &str,
    tokens_delta: Option<i64>,
) -> Result<(), AppError> {
    let row = Entity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query quota: {e}")))?
        .ok_or_else(|| AppError::internal("quota row missing".into()))?;
    let now = now_ms();
    let cur_hour = hour_bucket_ms(now);
    let cur_month = month_bucket(now);

    // 用 row 原始值预计算新值。
    let (new_turns, new_hour) = if row.hour_bucket != cur_hour {
        (1, cur_hour)
    } else {
        (row.used_turns_hour + 1, row.hour_bucket)
    };
    let new_tokens = if let Some(d) = tokens_delta {
        if row.month_bucket != cur_month {
            d
        } else {
            row.used_tokens_month + d
        }
    } else {
        row.used_tokens_month
    };
    let new_month = if row.month_bucket != cur_month {
        cur_month
    } else {
        row.month_bucket.clone()
    };

    let mut am: ActiveModel = row.into();
    am.used_turns_hour = Set(new_turns);
    am.hour_bucket = Set(new_hour);
    am.used_tokens_month = Set(new_tokens);
    am.month_bucket = Set(new_month);
    am.updated_at = Set(now);
    am.update(db)
        .await
        .map_err(|e| AppError::internal(format!("update quota: {e}")))?;
    Ok(())
}

/// 累加月度 token 用量(由 event_persist 在 token usage 更新时调用;跨月自动重置)。
pub async fn incr_tokens(
    db: &DatabaseConnection,
    team_id: &str,
    delta: i64,
) -> Result<(), AppError> {
    ensure_quota_row(db, team_id, 0).await?;
    let row = Entity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query quota: {e}")))?
        .ok_or_else(|| AppError::internal("quota row missing".into()))?;
    let now = now_ms();
    let cur_month = month_bucket(now);
    let (new_tokens, new_month) = if row.month_bucket != cur_month {
        (delta, cur_month)
    } else {
        (row.used_tokens_month + delta, row.month_bucket.clone())
    };
    let mut am: ActiveModel = row.into();
    am.used_tokens_month = Set(new_tokens);
    am.month_bucket = Set(new_month);
    am.updated_at = Set(now);
    am.update(db)
        .await
        .map_err(|e| AppError::internal(format!("update quota: {e}")))?;
    Ok(())
}

/// 设置 team 配额(owner / 管理接口用)。
pub async fn set_turn_quota(
    db: &DatabaseConnection,
    team_id: &str,
    turn_quota_hourly: i64,
) -> Result<(), AppError> {
    ensure_quota_row(db, team_id, turn_quota_hourly).await?;
    let row = Entity::find()
        .filter(crate::db::entities::team_quota::Column::TeamId.eq(team_id.to_string()))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query quota: {e}")))?
        .ok_or_else(|| AppError::internal("quota row missing".into()))?;
    let mut am: ActiveModel = row.into();
    am.turn_quota_hourly = Set(turn_quota_hourly);
    am.updated_at = Set(now_ms());
    am.update(db)
        .await
        .map_err(|e| AppError::internal(format!("update quota: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_bucket_aligns_to_hour() {
        let ms = 1_752_671_696_000_i64;
        let b = hour_bucket_ms(ms);
        assert_eq!(b % (HOUR_SECS * 1000), 0);
        assert!(b <= ms && ms - b < HOUR_SECS * 1000);
    }

    #[test]
    fn month_bucket_format() {
        let dt = chrono::NaiveDate::from_ymd_opt(2025, 7, 16)
            .unwrap()
            .and_hms_opt(12, 34, 56)
            .unwrap();
        let ms = dt.and_utc().timestamp_millis();
        assert_eq!(month_bucket(ms), "2025-07");
    }
}
