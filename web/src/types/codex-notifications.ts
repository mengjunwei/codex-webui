/**
 * Frontend-local types for notification dispatcher and token usage.
 * Minimal payload shapes for handled core events; the backend codex-schema
 * remains the source of truth but is not imported into the browser bundle.
 */

/** Token usage breakdown per model call. */
export interface TokenUsageBreakdown {
  totalTokens: number;
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  reasoningOutputTokens: number;
}

/** Thread-level token usage snapshot sent with `thread/tokenUsage/updated`. */
export interface ThreadTokenUsage {
  total: TokenUsageBreakdown;
  last: TokenUsageBreakdown;
  modelContextWindow: number | null;
}

/** Thread status from `thread/status/changed`. */
export type ThreadStatusType =
  | { type: 'notLoaded' }
  | { type: 'idle' }
  | { type: 'systemError' }
  | { type: 'active'; activeFlags: string[] };
