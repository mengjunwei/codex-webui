//! 进程池调度策略(M3/M4):纯逻辑决策,便于单测。
//!
//! 实际的状态管理(进程表、semaphore、空闲回收 task)在 `codex_pool.rs` 中组合使用这些策略。

use std::collections::HashMap;

/// 从"team → 最后活跃时间"表中选出最久未活跃的 team(LRU 回收候选)。
pub fn lru_victim(last_active: &HashMap<String, i64>) -> Option<String> {
    last_active
        .iter()
        .min_by_key(|(_, t)| *t)
        .map(|(k, _)| k.clone())
}

/// 是否应"按 team 扩进程":当前并发已达阈值,且该 team 进程数未达上限。
pub fn should_scale(
    in_flight: usize,
    threshold: usize,
    per_team_count: usize,
    per_team_max: usize,
) -> bool {
    in_flight >= threshold && per_team_count < per_team_max
}

/// 全局进程是否已满(拒绝/背压)。
pub fn global_full(count: usize, global_max: usize) -> bool {
    count >= global_max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_picks_oldest() {
        let mut m = HashMap::new();
        m.insert("t1".to_string(), 100);
        m.insert("t2".to_string(), 50);
        m.insert("t3".to_string(), 300);
        assert_eq!(lru_victim(&m).as_deref(), Some("t2"));
    }

    #[test]
    fn lru_empty_is_none() {
        let m = HashMap::<String, i64>::new();
        assert!(lru_victim(&m).is_none());
    }

    #[test]
    fn scale_decision() {
        // 并发未达阈值 → 不扩。
        assert!(!should_scale(3, 8, 1, 4));
        // 达阈值且未满 → 扩。
        assert!(should_scale(8, 8, 1, 4));
        // 达阈值但已满 → 不扩。
        assert!(!should_scale(20, 8, 4, 4));
    }

    #[test]
    fn global_full_check() {
        assert!(!global_full(24, 25));
        assert!(global_full(25, 25));
        assert!(global_full(26, 25));
    }
}
