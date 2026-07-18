/**
 * Notification dispatcher for Codex app-server events.
 * Maps every ServerNotification method to a typed handler.
 * Unknown methods fall through to a dev-only debug log.
 *
 * TODO: accountReadAccountQueryKey / accountReadRateLimitsQueryKey /
 *       codexStatusGetStatusQueryKey 旧 SDK 函数已下线,这些端点对应的账号/MCP/
 *       Codex 状态通知暂时只本地更新 store,不再主动 invalidate 查询。
 */
import type { QueryClient } from '@tanstack/react-query';
// TODO: FileUpdateChangeDto / RateLimitSnapshotDto 来自旧 OpenAPI SDK,已下线。
//       当前用本地 any 别名,待新 mt-client API 注解补全后再恢复强类型。
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type FileUpdateChangeDto = any;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type RateLimitSnapshotDto = any;
import { useAccountStore } from '@/stores/account-store';
import { useMcpStore } from '@/stores/mcp-store';
import { showSnackbar } from '@/stores/snackbar-store';
import type { AuthMode, PlanType } from '@/types/account';
import type { ThreadTokenUsage, ThreadStatusType } from '@/types/codex-notifications';
import type { McpServerStartupState } from '@/types/mcp';
import type { TurnItem, TurnPlanState, TurnPlanStepStatus } from '@/types/timeline';
import type { ApprovalRequest } from '@/types/approval';
import i18n from '@/i18n';

// ---------------------------------------------------------------------------
// Context injected by the hook — all store actions + queryClient
// ---------------------------------------------------------------------------

export interface NotificationContext {
  threadId: string | null;
  queryClient: QueryClient;
  updateCurrentTurn: (
    turnId: string,
    updater: (
      items: TurnItem[],
      completed: boolean,
    ) => { items: TurnItem[]; completed: boolean },
  ) => void;
  updateTurnItem: (
    turnId: string,
    itemId: string,
    updater: (existing: TurnItem | undefined) => TurnItem,
  ) => void;
  updateTurnDiff: (turnId: string, diff: string) => void;
  updateTurnPlan: (
    turnId: string,
    plan: TurnPlanState,
  ) => void;
  appendPlanDelta: (turnId: string, itemId: string, delta: string) => void;
  setLoading: (loading: boolean) => void;
  expandReasoning: (itemId: string) => void;
  collapseReasoning: (itemId: string) => void;
  addApproval: (approval: ApprovalRequest) => void;
  addSystemMessage: (message: string, severity?: 'info' | 'warning' | 'error', turnId?: string) => void;
  addSystemError: (message: string) => void;
  setTokenUsage: (turnId: string, usage: ThreadTokenUsage) => void;
  setThreadStatus: (status: ThreadStatusType | null) => void;
  setActiveTurnId: (turnId: string | null) => void;
  clearActiveTurn: () => void;
  setThreadTitle: (title: string | null) => void;
  resolveApprovalByRequestId: (requestId: string | number) => void;
}

type Params = Record<string, unknown>;
type Handler = (params: Params, ctx: NotificationContext) => void;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Checks if the notification carries a thread scope matching the routed context. */
function hasThreadScope(params: Params, ctx: NotificationContext): boolean {
  const eventThreadId = params.threadId as string | undefined;
  return Boolean(eventThreadId && ctx.threadId === eventThreadId);
}

// ---------------------------------------------------------------------------
// Error deduplication — suppress repeated retry toasts within a short window
// ---------------------------------------------------------------------------

const recentErrors = new Map<string, number>();
const DEDUP_WINDOW_MS = 5_000;
/** Tracks final error system entries to avoid duplicates from error + turn/completed. */
const finalErrorEntries = new Set<string>();
const MAX_FINAL_ERROR_ENTRIES = 500;

function isDuplicateRetryError(key: string): boolean {
  const now = Date.now();
  const last = recentErrors.get(key);
  if (last && now - last < DEDUP_WINDOW_MS) return true;
  recentErrors.set(key, now);
  for (const [k, ts] of recentErrors) {
    if (now - ts > DEDUP_WINDOW_MS) recentErrors.delete(k);
  }
  return false;
}

/** Returns true only on first call per unique error — deduplicates error + turn/completed. */
function shouldRecordFinalError(threadId: string | undefined, turnId: string | undefined, message: string): boolean {
  const key = `${threadId ?? ''}:${turnId ?? ''}:${message}`;
  if (finalErrorEntries.has(key)) return false;
  if (finalErrorEntries.size >= MAX_FINAL_ERROR_ENTRIES) {
    const first = finalErrorEntries.values().next().value;
    if (first !== undefined) finalErrorEntries.delete(first);
  }
  finalErrorEntries.add(key);
  return true;
}

// ---------------------------------------------------------------------------
// Thread-list invalidation with debounce to avoid storms
// ---------------------------------------------------------------------------

let invalidateTimer: ReturnType<typeof setTimeout> | null = null;

/** 多租户 threads 列表查询键前缀（供 useQuery / invalidateQueries 共用） */
export const MT_THREADS_QUERY_KEY_PREFIX = 'mt-threads';

function debouncedInvalidateThreadList(queryClient: QueryClient): void {
  if (invalidateTimer) clearTimeout(invalidateTimer);
  invalidateTimer = setTimeout(() => {
    void queryClient.invalidateQueries({
      predicate: (query) => {
        const key = query.queryKey;
        return Array.isArray(key) && key[0] === MT_THREADS_QUERY_KEY_PREFIX;
      },
    });
    invalidateTimer = null;
  }, 300);
}

function invalidateAccountQueries(_queryClient: QueryClient): void {
  // TODO: 旧 SDK 查询键已下线。当前只刷新本地 store,不再 invalidate 旧查询。
}

function isPlanStepStatus(value: unknown): value is TurnPlanStepStatus {
  return value === 'pending' || value === 'inProgress' || value === 'completed';
}

// ---------------------------------------------------------------------------
// Tier 0 — Already handled (migrated from if-chain)
// ---------------------------------------------------------------------------

const handleReasoningSummaryTextDelta: Handler = (params, ctx) => {
  const { turnId, itemId, delta } = params as { turnId?: string; itemId?: string; delta?: string };
  if (!turnId || !itemId || !hasThreadScope(params, ctx)) return;
  ctx.updateTurnItem(turnId, itemId, (existing) => ({
    type: 'reasoning',
    itemId,
    content: (existing?.content ?? '') + (delta ?? ''),
    completed: false,
  }));
  ctx.expandReasoning(itemId);
};

const handleAgentMessageDelta: Handler = (params, ctx) => {
  const { turnId, itemId, delta } = params as { turnId?: string; itemId?: string; delta?: string };
  if (!turnId || !itemId || !hasThreadScope(params, ctx)) return;
  ctx.updateTurnItem(turnId, itemId, (existing) => ({
    type: 'agentMessage',
    itemId,
    content: (existing?.content ?? '') + (delta ?? ''),
    completed: false,
  }));
};

const handleCommandExecutionOutputDelta: Handler = (params, ctx) => {
  const { turnId, itemId, delta } = params as { turnId?: string; itemId?: string; delta?: string };
  if (!turnId || !itemId || !hasThreadScope(params, ctx)) return;
  ctx.updateTurnItem(turnId, itemId, (existing) => ({
    ...(existing ?? { type: 'commandExecution' as const, itemId }),
    content: (existing?.content ?? '') + (delta ?? ''),
    completed: false,
  }));
};

const handleFileChangeOutputDelta: Handler = (params, ctx) => {
  const { turnId, itemId, delta } = params as { turnId?: string; itemId?: string; delta?: string };
  if (!turnId || !itemId || !hasThreadScope(params, ctx)) return;
  ctx.updateTurnItem(turnId, itemId, (existing) => ({
    ...(existing ?? { type: 'fileChange' as const, itemId }),
    content: (existing?.content ?? '') + (delta ?? ''),
    completed: false,
  }));
};

const handleTurnDiffUpdated: Handler = (params, ctx) => {
  const { turnId } = params as { turnId?: string };
  const diff = params.diff as string | undefined;
  if (!turnId || typeof diff !== 'string' || !hasThreadScope(params, ctx)) return;
  ctx.updateTurnDiff(turnId, diff);
};

const handleItemStarted: Handler = (params, ctx) => {
  const { turnId } = params as { turnId?: string };
  if (!turnId || !hasThreadScope(params, ctx)) return;
  const item = params.item as Record<string, unknown> | undefined;
  if (!item) return;
  const id = item.id as string;

  if (item.type === 'mcpToolCall') {
    ctx.updateTurnItem(turnId, id, () => ({
      type: 'mcpToolCall',
      itemId: id,
      content: '',
      completed: false,
      toolServer: (item.server as string) ?? '',
      toolName: (item.tool as string) ?? '',
      toolArgs: item.arguments ? JSON.stringify(item.arguments, null, 2) : '',
    }));
  }
  if (item.type === 'fileChange') {
    const changes = item.changes as FileUpdateChangeDto[] | undefined;
    ctx.updateTurnItem(turnId, id, () => ({
      type: 'fileChange',
      itemId: id,
      content: '',
      completed: false,
      filePath: changes?.[0]?.path ?? '',
      fileDiff: changes?.[0]?.diff ?? '',
    }));
  }
  if (item.type === 'commandExecution') {
    ctx.updateTurnItem(turnId, id, () => ({
      type: 'commandExecution',
      itemId: id,
      content: '',
      completed: false,
      command: (item.command as string) ?? '',
    }));
  }
};

const handleItemCompleted: Handler = (params, ctx) => {
  const { turnId } = params as { turnId?: string };
  if (!turnId || !hasThreadScope(params, ctx)) return;
  const item = params.item as Record<string, unknown> | undefined;
  if (!item) return;
  const completedItemId = (params.itemId as string) ?? (item.id as string);

  if (item.type === 'agentMessage') {
    ctx.updateTurnItem(turnId, completedItemId, () => ({
      type: 'agentMessage',
      itemId: completedItemId,
      content: (item.text as string) ?? '',
      completed: true,
    }));
  }
  if (item.type === 'reasoning') {
    ctx.updateTurnItem(turnId, completedItemId, (existing) => ({
      ...(existing ?? { type: 'reasoning' as const, itemId: completedItemId, content: '' }),
      completed: true,
    }));
    ctx.collapseReasoning(completedItemId);
  }
  if (item.type === 'commandExecution') {
    ctx.updateTurnItem(turnId, completedItemId, (existing) => ({
      ...(existing ?? { type: 'commandExecution' as const, itemId: completedItemId, content: '' }),
      content: (item.aggregatedOutput as string) || existing?.content || '',
      command: (item.command as string) || existing?.command,
      exitCode: (item.exitCode as number) ?? existing?.exitCode,
      completed: true,
    }));
  }
  if (item.type === 'mcpToolCall') {
    const result = item.result as Record<string, unknown> | null;
    const resultText = result?.content
      ? JSON.stringify(result.content, null, 2).slice(0, 500)
      : ((item.error as string) ?? '');
    ctx.updateTurnItem(turnId, completedItemId, (existing) => ({
      ...(existing ?? {
        type: 'mcpToolCall' as const,
        itemId: completedItemId,
        toolServer: (item.server as string) ?? '',
        toolName: (item.tool as string) ?? '',
        toolArgs: '',
      }),
      content: resultText,
      completed: true,
    }));
  }
  if (item.type === 'fileChange') {
    const changes = item.changes as FileUpdateChangeDto[] | undefined;
    const firstChange = changes?.[0];
    ctx.updateTurnItem(turnId, completedItemId, (existing) => ({
      ...(existing ?? { type: 'fileChange' as const, itemId: completedItemId }),
      content: existing?.content ?? '',
      completed: true,
      filePath: existing?.filePath ?? firstChange?.path ?? '',
      fileDiff: firstChange?.diff || existing?.fileDiff || '',
    }));
  }
};

/** turn/completed payload is { threadId, turn: { id, status, error } }. */
const handleTurnCompleted: Handler = (params, ctx) => {
  const turn = params.turn as
    | { id?: string; status?: string; error?: { message?: string } | null }
    | undefined;
  const turnId = turn?.id;
  if (!turnId) return;

  if (!hasThreadScope(params, ctx)) {
    // Still invalidate thread list for non-active threads
    debouncedInvalidateThreadList(ctx.queryClient);
    return;
  }

  ctx.updateCurrentTurn(turnId, (items) => ({ items, completed: true }));
  ctx.setLoading(false);
  ctx.clearActiveTurn();

  if (
    turn?.status === 'failed' &&
    turn.error?.message &&
    shouldRecordFinalError(params.threadId as string | undefined, turnId, turn.error.message)
  ) {
    ctx.addSystemMessage(`Error: ${turn.error.message}`, 'error', turnId);
  }

  void ctx.queryClient.invalidateQueries({
    predicate: (query) => {
      const key = query.queryKey;
      return Array.isArray(key) && key[0] === MT_THREADS_QUERY_KEY_PREFIX;
    },
  });
};

// ---------------------------------------------------------------------------
// Tier 1 — High value
// ---------------------------------------------------------------------------

const handleError: Handler = (params, ctx) => {
  const error = params.error as { message?: string; additionalDetails?: string } | undefined;
  const willRetry = params.willRetry as boolean;
  const turnId = params.turnId as string | undefined;
  const threadId = params.threadId as string | undefined;
  const message = error?.message ?? 'Unknown error';

  if (willRetry) {
    const dedupKey = `${threadId}:${turnId}:${message}`;
    if (ctx.threadId === threadId && !isDuplicateRetryError(dedupKey)) {
      showSnackbar(message, 'warning');
    }
  } else {
    if (ctx.threadId === threadId) {
      showSnackbar(message, 'error', 5000);
      if (shouldRecordFinalError(threadId, turnId, message)) {
        ctx.addSystemMessage(`Error: ${message}`, 'error', turnId);
      }
      if (turnId) {
        ctx.updateCurrentTurn(turnId, (items) => ({ items, completed: true }));
      }
      ctx.setLoading(false);
      ctx.clearActiveTurn();
    }
  }
};

const handleTokenUsageUpdated: Handler = (params, ctx) => {
  const turnId = params.turnId as string | undefined;
  const tokenUsage = params.tokenUsage as ThreadTokenUsage | undefined;
  if (!turnId || !tokenUsage || !hasThreadScope(params, ctx)) return;
  ctx.setTokenUsage(turnId, tokenUsage);
};

const handleServerRequestResolved: Handler = (params, ctx) => {
  const requestId = params.requestId as string | number | undefined;
  if (requestId == null || !hasThreadScope(params, ctx)) return;
  ctx.resolveApprovalByRequestId(requestId);
};

const handleConfigWarning: Handler = (params) => {
  const summary = params.summary as string;
  const details = params.details as string | null;
  showSnackbar(details ? `${summary}: ${details}` : summary, 'warning', 5000);
};

const handleDeprecationNotice: Handler = (params) => {
  const summary = params.summary as string;
  showSnackbar(summary, 'warning', 5000);
};

const handleTurnPlanUpdated: Handler = (params, ctx) => {
  const turnId = params.turnId as string | undefined;
  if (!turnId || !hasThreadScope(params, ctx)) return;
  const rawPlan = Array.isArray(params.plan) ? params.plan : [];
  const steps = rawPlan
    .map((step) => step as { step?: unknown; status?: unknown })
    .filter(
      (step): step is { step: string; status: TurnPlanStepStatus } =>
        typeof step.step === 'string' && isPlanStepStatus(step.status),
    )
    .map((step) => ({ step: step.step, status: step.status }));
  ctx.updateTurnPlan(turnId, {
    explanation: typeof params.explanation === 'string' ? params.explanation : null,
    steps,
  });
};

const handlePlanDelta: Handler = (params, ctx) => {
  const { turnId, itemId, delta } = params as {
    turnId?: string;
    itemId?: string;
    delta?: string;
  };
  if (!turnId || !itemId || !delta || !hasThreadScope(params, ctx)) return;
  ctx.appendPlanDelta(turnId, itemId, delta);
};

const handleMcpToolCallProgress: Handler = (params, ctx) => {
  const { turnId, itemId, message } = params as {
    turnId?: string;
    itemId?: string;
    message?: string;
  };
  if (!turnId || !itemId || !hasThreadScope(params, ctx)) return;
  ctx.updateTurnItem(turnId, itemId, (existing) => ({
    ...(existing ?? {
      type: 'mcpToolCall' as const,
      itemId,
      content: '',
      completed: false,
      toolServer: '',
      toolName: '',
      toolArgs: '',
    }),
    toolProgress: message ?? '',
  }));
};

// eslint-disable-next-line @typescript-eslint/no-unused-vars
const handleMcpStartupStatusUpdated: Handler = (params, _ctx) => {
  const name = params.name as string | undefined;
  const status = params.status as string | undefined;
  if (!name || !isMcpStartupStatus(status)) return;
  useMcpStore.getState().setServerStatus({
    name,
    status,
    error: typeof params.error === 'string' ? params.error : null,
  });
  if (status === 'ready' || status === 'failed') {
    // MCP 服务端列表查询已迁移至 mt-client，无需旧查询键失效
  }
};

// ---------------------------------------------------------------------------
// Tier 2 — Thread/Turn lifecycle
// ---------------------------------------------------------------------------

const handleThreadStarted: Handler = (_params, ctx) => {
  debouncedInvalidateThreadList(ctx.queryClient);
};

const handleThreadStatusChanged: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  const status = params.status as ThreadStatusType | undefined;
  if (!status) return;

  if (ctx.threadId === threadId) {
    ctx.setThreadStatus(status);
    if (status.type === 'systemError') {
      ctx.addSystemMessage(i18n.t('Thread encountered a system error'), 'error');
    }
  }
  debouncedInvalidateThreadList(ctx.queryClient);
};

const handleThreadNameUpdated: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  const name = params.threadName as string | undefined;
  if (threadId && ctx.threadId === threadId) {
    ctx.setThreadTitle(name?.trim() || null);
  }
  debouncedInvalidateThreadList(ctx.queryClient);
};

const handleThreadClosed: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  if (ctx.threadId === threadId) {
    ctx.addSystemMessage(i18n.t('Thread closed'), 'info');
  }
  debouncedInvalidateThreadList(ctx.queryClient);
};

const handleThreadArchived: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  if (ctx.threadId === threadId) {
    ctx.addSystemMessage(i18n.t('Thread archived'), 'warning');
  }
  debouncedInvalidateThreadList(ctx.queryClient);
};

const handleThreadUnarchived: Handler = (_params, ctx) => {
  debouncedInvalidateThreadList(ctx.queryClient);
};

const handleTurnStarted: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  const turn = params.turn as { id?: string } | undefined;
  const turnId = turn?.id;
  if (!turnId || ctx.threadId !== threadId) return;
  ctx.updateCurrentTurn(turnId, () => ({ items: [], completed: false }));
  ctx.setLoading(true);
  ctx.setActiveTurnId(turnId);
};

const handleThreadCompacted: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  if (ctx.threadId === threadId) {
    ctx.addSystemMessage(i18n.t('Context compacted'), 'info');
  }
};

const handleModelRerouted: Handler = (params, ctx) => {
  const threadId = params.threadId as string | undefined;
  const fromModel = params.fromModel as string;
  const toModel = params.toModel as string;
  const message = i18n.t('Model rerouted: {{from}} → {{to}}', {
    from: fromModel,
    to: toModel,
  });
  if (ctx.threadId === threadId) {
    ctx.addSystemMessage(message, 'warning');
    showSnackbar(message, 'info');
  }
};

const handleAccountUpdated: Handler = (params, ctx) => {
  const authMode = params.authMode as AuthMode | null;
  const planType = params.planType as PlanType | null;
  useAccountStore.getState().setAccountUpdated({ authMode, planType });
  invalidateAccountQueries(ctx.queryClient);
};

const handleAccountLoginCompleted: Handler = (params, ctx) => {
  const payload = {
    loginId: typeof params.loginId === 'string' ? params.loginId : null,
    success: Boolean(params.success),
    error: typeof params.error === 'string' ? params.error : null,
  };
  useAccountStore.getState().setLoginCompleted(payload);
  invalidateAccountQueries(ctx.queryClient);
  if (payload.success) {
    showSnackbar(i18n.t('ChatGPT login completed'), 'success');
  } else if (payload.error) {
    showSnackbar(payload.error, 'error', 5000);
  }
};

const handleAccountRateLimitsUpdated: Handler = (params, _ctx) => {
  const rateLimits = params.rateLimits as RateLimitSnapshotDto | undefined;
  if (!rateLimits) return;
  useAccountStore.getState().setRateLimitSnapshot(rateLimits);
  // TODO: 旧 accountReadRateLimitsQueryKey 已下线,新 mt-client 待接入
};

const handleSkillsChanged: Handler = (_params, _ctx) => {
  // skills 查询已迁移至 mt-client，无需旧查询键失效
};

/** Invalidate apps query when app list changes (e.g. after plugin install). */
const handleAppListUpdated: Handler = (_params, _ctx) => {
  // apps 查询已迁移至 mt-client，无需旧查询键失效
};

/** Refresh MCP status and show toast after OAuth login completes. */
const handleMcpOauthLoginCompleted: Handler = (params, _ctx) => {
  // MCP 服务端列表查询已迁移至 mt-client，无需旧查询键失效
  const name = typeof params.name === 'string' ? params.name : 'MCP server';
  const success = params.success === true;
  if (success) {
    showSnackbar(i18n.t('{{name}} login completed', { name }), 'success');
  } else {
    const error = typeof params.error === 'string' ? params.error : '';
    showSnackbar(i18n.t('{{name}} login failed: {{error}}', { name, error }), 'error');
  }
};

function isMcpStartupStatus(value: unknown): value is McpServerStartupState {
  return value === 'starting' || value === 'ready' || value === 'failed' || value === 'cancelled';
}

// ---------------------------------------------------------------------------
// Tier 3 — Known low-priority methods (debug-only logging)
// ---------------------------------------------------------------------------

const TIER3_METHODS = new Set([
  'hook/started',
  'hook/completed',
  'item/autoApprovalReview/started',
  'item/autoApprovalReview/completed',
  'rawResponseItem/completed',
  'command/exec/outputDelta',
  'item/commandExecution/terminalInteraction',
  'fs/changed',
  'item/reasoning/summaryPartAdded',
  'item/reasoning/textDelta',
  'fuzzyFileSearch/sessionUpdated',
  'fuzzyFileSearch/sessionCompleted',
  'thread/realtime/started',
  'thread/realtime/itemAdded',
  'thread/realtime/transcriptUpdated',
  'thread/realtime/outputAudio/delta',
  'thread/realtime/sdp',
  'thread/realtime/error',
  'thread/realtime/closed',
  'windows/worldWritableWarning',
  'windowsSandbox/setupCompleted',
]);

// ---------------------------------------------------------------------------
// Master handler map
// ---------------------------------------------------------------------------

const HANDLERS: Record<string, Handler> = {
  // Tier 0 — existing
  'item/reasoning/summaryTextDelta': handleReasoningSummaryTextDelta,
  'item/agentMessage/delta': handleAgentMessageDelta,
  'item/commandExecution/outputDelta': handleCommandExecutionOutputDelta,
  'item/fileChange/outputDelta': handleFileChangeOutputDelta,
  'turn/diff/updated': handleTurnDiffUpdated,
  'item/started': handleItemStarted,
  'item/completed': handleItemCompleted,
  'turn/completed': handleTurnCompleted,

  // Tier 1 — high value
  'error': handleError,
  'thread/tokenUsage/updated': handleTokenUsageUpdated,
  'serverRequest/resolved': handleServerRequestResolved,
  'configWarning': handleConfigWarning,
  'deprecationNotice': handleDeprecationNotice,
  'turn/plan/updated': handleTurnPlanUpdated,
  'item/plan/delta': handlePlanDelta,
  'item/mcpToolCall/progress': handleMcpToolCallProgress,
  'mcpServer/startupStatus/updated': handleMcpStartupStatusUpdated,
  'account/updated': handleAccountUpdated,
  'account/rateLimits/updated': handleAccountRateLimitsUpdated,
  'account/login/completed': handleAccountLoginCompleted,

  // Tier 2 — thread/turn lifecycle
  'thread/started': handleThreadStarted,
  'thread/status/changed': handleThreadStatusChanged,
  'thread/name/updated': handleThreadNameUpdated,
  'thread/closed': handleThreadClosed,
  'thread/archived': handleThreadArchived,
  'thread/unarchived': handleThreadUnarchived,
  'turn/started': handleTurnStarted,
  'thread/compacted': handleThreadCompacted,
  'model/rerouted': handleModelRerouted,
  'skills/changed': handleSkillsChanged,
  'app/list/updated': handleAppListUpdated,
  'mcpServer/oauthLogin/completed': handleMcpOauthLoginCompleted,
};

// ---------------------------------------------------------------------------
// Public dispatcher
// ---------------------------------------------------------------------------

/**
 * Dispatches a Codex app-server notification to the appropriate handler.
 *
 * @param method - Notification method name (e.g. 'item/agentMessage/delta')
 * @param params - Notification params payload
 * @param ctx - Injected dependencies (store actions, queryClient)
 */
export function handleNotification(
  method: string,
  params: Record<string, unknown>,
  ctx: NotificationContext,
): void {
  const handler = HANDLERS[method];
  const eventThreadId = params.threadId as string | undefined;
  const previousThreadId = ctx.threadId;

  // Route thread-scoped notifications to their owning thread runtime.
  if (eventThreadId) ctx.threadId = eventThreadId;

  try {
    if (handler) {
      handler(params, ctx);
      return;
    }

    if (TIER3_METHODS.has(method)) {
      if (import.meta.env.DEV) {
        console.debug(`[codex] tier3 notification: ${method}`);
      }
      return;
    }

    if (import.meta.env.DEV) {
      console.debug(`[codex] unknown notification: ${method}`);
    }
  } finally {
    ctx.threadId = previousThreadId;
  }
}
