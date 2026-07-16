//! 线程 resume 注册表（service 层）：缓存最近一次 resume/start/fork 的响应，
//! 按 generation 去重 + 并发 resume 串行化（对齐 TS ThreadResumeRegistryService）。
//!
//! 从 api/threads.rs 拆出，供 AppState 持有（main.rs 构造、realtime.rs 与
//! threads handler 共享）。

use crate::error::AppError;

/// H6：线程 resume 注册表（按 generation 去重，对齐 TS ThreadResumeRegistryService）。
/// 线程 resume 注册表：缓存最近一次 resume/start/fork 的响应，按 generation 去重。
/// codex 重启（generation 变化）时通过 `advance_generation()` 清空缓存（对齐 TS
/// resumeRegistry 在 appServerReady 时按 generation 重建）。
///
/// ## 三个并发难题与对应解决方案
///
/// 1. **H7**：旧实现中条目无 generation，advance 与 auto-resume 跨任务调度顺序
///    无保证 → 旧 generation 的陈旧缓存可能命中新 generation 的 resume。
///    **修复**：条目绑定写入时的 generation；读侧按当前 generation 过滤。
///
/// 2. **TS 对齐**：并发 resume 同线程会触发非幂等的 `thread/resume` RPC 多次。
///    **修复**：per-key 锁槽（std Mutex 取槽 + tokio Mutex 跨 await），保证
///    同线程串行；不同线程并发安全。
///
/// 3. **T7**：锁槽无限增长。**修复**：`reap_inflight_slot` 仅在 `strong_count == 1`
///    时移除 —— 调用方先 drop 自己的 Arc clone，再检查 strong_count。
///
/// ## 数据布局
///
/// - `generation`：当前 generation（启动时为 0）。
/// - `entries`：HashMap<thread_id, (generation, response)>。
/// - `inflight`：HashMap<thread_id, Arc<tokio::Mutex<()>>>。
#[derive(Debug, Default)]
pub struct ThreadResumeRegistry {
    generation: std::sync::Mutex<u64>,
    /// 条目携带写入时的 generation；读侧按当前 generation 过滤，根除
    /// advance_generation 与 auto-resume 跨任务调度的时序竞态（H7）。
    entries: std::sync::Mutex<std::collections::HashMap<String, (u64, serde_json::Value)>>,
    /// per-key in-flight 锁槽：并发 resume 串行化（对齐 TS resumeRegistry.inFlight）。
    /// HashMap 用 std Mutex（取槽短暂），每个槽是 tokio Mutex（跨 RPC await 持有）。
    inflight: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl ThreadResumeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次 resume/start/fork 的响应（缓存供后续重复调用复用）。
    /// 条目绑定当前 generation，跨 generation 读侧自动失效。
    pub fn mark_resumed(&self, thread_id: &str, response: serde_json::Value) {
        let g = *self.generation.lock().unwrap();
        self.entries
            .lock()
            .unwrap()
            .insert(thread_id.to_string(), (g, response));
    }

    /// 返回缓存响应（仅当条目 generation 与当前 generation 一致时命中）。
    pub fn get_cached(&self, thread_id: &str) -> Option<serde_json::Value> {
        let g = *self.generation.lock().unwrap();
        self.entries
            .lock()
            .unwrap()
            .get(thread_id)
            .filter(|(gen, _)| *gen == g)
            .map(|(_, v)| v.clone())
    }

    pub fn forget(&self, thread_id: &str) {
        self.entries.lock().unwrap().remove(thread_id);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// generation 推进：generation 变化时清空缓存响应。
    /// 注意：不再清空 inflight —— clear 会打断进行中的 resume，使 per-key 互斥出现破洞；
    /// 孤立锁槽改由 ensure_resumed 释放 guard 后按 strong_count 回收。
    /// （即使本调用未被及时调度，get_cached 的 generation 过滤也能保证不命中陈旧缓存。）
    pub fn advance_generation(&self, new_generation: u64) {
        let mut g = self.generation.lock().unwrap();
        if *g != new_generation {
            *g = new_generation;
            self.entries.lock().unwrap().clear();
        }
    }

    /// ensure_resumed：缓存命中直接返回；否则在 per-key 锁内串行化并发 resume，
    /// 锁内重检缓存（前一个 in-flight 可能已完成），仍未命中才执行 RPC 并写缓存。
    /// 返回 `(响应, 是否来自缓存)`。对齐 TS `resumeRegistry` 的 inFlight + resumed 去重，
    /// 避免对非幂等的 `thread/resume` 并发重复调用。
    ///
    /// ## 完整流程
    ///
    /// 1. **快路径**：`get_cached` 命中（generation 过滤）→ 直接返回 `(v, true)`。
    /// 2. **取锁槽**：std Mutex 短持有获取或创建 per-key tokio Mutex。
    /// 3. **异步加锁**：`lock.await` —— 此处跨 await，tokio Mutex 不会被 worker 线程独占。
    /// 4. **锁内重检**：可能前一个 in-flight 已完成 → 命中后释放锁槽再返回。
    /// 5. **执行 RPC**：失败路径也必须释放锁槽 + reap（否则泄漏）。
    /// 6. **写缓存**：成功后 `mark_resumed`。
    /// 7. **释放 + reap**：先 drop 自己的 guard 与 Arc clone，再 `reap_inflight_slot`
    ///    —— 检查 `strong_count == 1` 时移除锁槽。
    ///
    /// ## 为什么 `reap_inflight_slot` 在 drop 之后
    ///
    /// 若 drop 之前检查 strong_count，本地 Arc + HashMap 里的 Arc 至少 2，
    /// 永远不会被回收。drop 之后本表成为唯一持有者才能正确判定孤立。
    pub async fn ensure_resumed<F, Fut>(
        &self,
        thread_id: &str,
        f: F,
    ) -> Result<(serde_json::Value, bool), AppError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<serde_json::Value, AppError>>,
    {
        // 快路径：缓存命中（未获取锁槽，无回收义务）。
        if let Some(v) = self.get_cached(thread_id) {
            return Ok((v, true));
        }
        // per-key 锁槽（std Mutex 短暂持有取槽，tokio Mutex 跨 RPC await）。
        let key = thread_id.to_string();
        let lock = {
            let mut guards = self.inflight.lock().unwrap();
            guards
                .entry(key.clone())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        // T7：锁内重检命中 / RPC 失败 / 成功 三条路径都要回收锁槽，否则并发命中（最常见）
        // 与失败路径泄漏。提取 reap_inflight_slot，先 drop 自己的 guard + Arc clone 再检查 strong_count。
        if let Some(v) = self.get_cached(thread_id) {
            drop(_guard);
            drop(lock);
            self.reap_inflight_slot(&key);
            return Ok((v, true));
        }
        let result = match f(key.clone()).await {
            Ok(r) => r,
            Err(e) => {
                drop(_guard);
                drop(lock);
                self.reap_inflight_slot(&key);
                return Err(e);
            }
        };
        self.mark_resumed(&key, result.clone());
        drop(_guard);
        drop(lock);
        self.reap_inflight_slot(&key);
        Ok((result, false))
    }
}

impl ThreadResumeRegistry {
    /// 回收孤立的 in-flight 锁槽：仅当本表是唯一持有者（strong_count==1）时移除。
    /// 调用前必须已 drop 调用方自己的 Arc clone，否则计数恒 ≥2。
    fn reap_inflight_slot(&self, key: &str) {
        let mut guards = self.inflight.lock().unwrap();
        if let Some(arc) = guards.get(key) {
            if std::sync::Arc::strong_count(arc) == 1 {
                guards.remove(key);
            }
        }
    }
}
