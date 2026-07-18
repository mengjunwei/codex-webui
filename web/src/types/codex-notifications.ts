/**
 * Codex 通知相关类型别名。
 *
 * TODO: ThreadTokenUsageDto / TokenUsageBreakdownDto / ThreadStatus*Dto
 *       来自旧 OpenAPI SDK,已下线。这里用本地最小形状替代,
 *       待新 mt-client API 注解补全后再恢复强类型。
 */

/** 单 turn 的 token 使用明细。 */
export interface TokenUsageBreakdown {
  cachedInputTokens: number;
  inputTokens: number;
  outputTokens: number;
  reasoningOutputTokens: number;
  totalTokens: number;
}

/** 线程 token 使用量快照:最近一次 turn + 线程累计。 */
export interface ThreadTokenUsage {
  last: TokenUsageBreakdown;
  total: TokenUsageBreakdown;
  modelContextWindow?: number;
  turnId?: string;
}

/** 旧扁平字段别名,用于兼容 mt-client 返回的扁平结构。 */
export interface FlatThreadTokenUsage {
  turnId?: string;
  usage?: TokenUsageBreakdown;
  totalTokens?: number;
  inputTokens?: number;
  outputTokens?: number;
  reasoningOutputTokens?: number;
  cachedInputTokens?: number;
  modelContextWindow?: number;
}

// 线程状态用判别联合,字段松散以兼容运行时任何形状
export type ThreadStatusType =
  | { type: 'notLoaded' }
  | { type: 'idle' }
  | { type: 'systemError'; message?: string }
  | { type: 'active'; activeFlags: string[] };
