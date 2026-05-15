/**
 * Hook that connects socket.io events to Zustand stores.
 * Delegates all Codex notification routing to the dispatcher.
 * Also triggers TanStack Query invalidation for relevant events.
 */
import { useEffect } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { getSocket } from '../socket';
import { useConnectionStore } from '../stores/connection-store';
import { useTimelineStore } from '../stores/timeline-store';
import { handleNotification, type NotificationContext } from './notification-handlers';
import { tokenUsageReadThreadTokenUsage, turnDiffReadThreadTurnDiffs } from '@/generated/api/sdk.gen';
import type { NetworkPolicyAmendment, RawCommandDecision } from '@/types/approval';
import i18n from '@/i18n';

/** Parses availableDecisions from raw socket params with runtime validation. */
const rawSimpleDecisions = new Set(['accept', 'acceptForSession', 'decline', 'cancel']);

function parseAvailableDecisions(value: unknown): RawCommandDecision[] | null {
  if (!Array.isArray(value)) return null;
  return value.filter((d): d is RawCommandDecision => {
    if (typeof d === 'string') return rawSimpleDecisions.has(d);
    return d !== null && typeof d === 'object' &&
      ('acceptWithExecpolicyAmendment' in d || 'applyNetworkPolicyAmendment' in d);
  });
}

function parseStringArray(value: unknown): string[] | null {
  return Array.isArray(value) ? value.filter((s): s is string => typeof s === 'string') : null;
}

function parseNetworkAmendments(value: unknown): NetworkPolicyAmendment[] | null {
  if (!Array.isArray(value)) return null;
  return value.filter((item): item is NetworkPolicyAmendment => {
    if (item === null || typeof item !== 'object') return false;
    const r = item as Record<string, unknown>;
    return typeof r.host === 'string' && (r.action === 'allow' || r.action === 'deny');
  });
}

type CodexLifecycleEvent =
  | { type: 'appServerRestarting'; generation: number; delayMs: number }
  | { type: 'appServerUnavailable'; generation: number; message: string }
  | { type: 'appServerReady'; generation: number; restarted: boolean }
  | { type: 'autoResumeCompleted'; generation: number; resumedThreadIds: string[]; failedThreadIds: string[] };

export function useCodexSocket(enabled = true) {
  const setConnected = useConnectionStore((s) => s.setConnected);
  const queryClient = useQueryClient();
  const {
    threadId,
    updateCurrentTurn,
    updateTurnItem,
    updateTurnDiff,
    updateTurnPlan,
    appendPlanDelta,
    setLoading,
    expandReasoning,
    collapseReasoning,
    addApproval,
    addSystemMessage,
    addSystemError,
    setTokenUsage,
    setThreadStatus,
    setActiveTurnId,
    clearActiveTurn,
    hydrateTokenUsage,
    hydrateTurnDiffs,
    setThreadTitle,
    resolveApprovalByRequestId,
  } = useTimelineStore();

  useEffect(() => {
    if (!enabled) return;

    const socket = getSocket();

    const handleConnect = () => {
      setConnected(true);
      const { threadId: activeId, threadMode: mode } = useTimelineStore.getState();
      if (activeId && mode === 'live') {
        socket.emit('thread.subscribe', { threadId: activeId });
      }
    };
    const handleDisconnect = () => setConnected(false);

    socket.on('connect', handleConnect);
    socket.on('disconnect', handleDisconnect);

    // Build context object for the notification dispatcher
    const ctx: NotificationContext = {
      threadId,
      queryClient,
      updateCurrentTurn,
      updateTurnItem,
      updateTurnDiff,
      updateTurnPlan,
      appendPlanDelta,
      setLoading,
      expandReasoning,
      collapseReasoning,
      addApproval,
      addSystemMessage,
      addSystemError,
      setTokenUsage,
      setThreadStatus,
      setActiveTurnId,
      clearActiveTurn,
      setThreadTitle,
      resolveApprovalByRequestId,
    };

    const handleCodexNotification = (notification: {
      method: string;
      params: Record<string, unknown>;
    }) => {
      handleNotification(notification.method, notification.params, ctx);
    };

    socket.on('codex.notification', handleCodexNotification);

    const handleCodexLifecycle = (event: CodexLifecycleEvent) => {
      const activeThreadId = useTimelineStore.getState().threadId;

      if (event.type === 'appServerUnavailable') {
        clearActiveTurn();
        setThreadStatus({ type: 'systemError' });
      }

      if (event.type === 'appServerRestarting') {
        clearActiveTurn();
        setThreadStatus({ type: 'systemError' });
        if (activeThreadId) {
          addSystemMessage(
            i18n.t('Codex app-server is restarting. Waiting to resume this thread.'),
            'warning',
          );
        }
      }

      if (event.type === 'appServerReady') {
        void queryClient.invalidateQueries();
      }

      if (event.type !== 'autoResumeCompleted' || !activeThreadId) return;

      if (event.failedThreadIds.includes(activeThreadId)) {
        addSystemMessage(i18n.t('Auto-resume failed. Reopen this thread to retry.'), 'error');
        return;
      }

      if (event.resumedThreadIds.includes(activeThreadId)) {
        addSystemMessage(i18n.t('Thread resumed after app-server restart.'), 'info');
        void tokenUsageReadThreadTokenUsage({ path: { threadId: activeThreadId } })
          .then(({ data }) => data && hydrateTokenUsage(data.turns))
          .catch(() =>
            addSystemMessage(i18n.t('Token usage recovery failed after resume.'), 'warning'),
          );
        void turnDiffReadThreadTurnDiffs({ path: { threadId: activeThreadId } })
          .then(({ data }) => data && hydrateTurnDiffs(data.turns))
          .catch(() => undefined);
      }
    };

    socket.on('codex.lifecycle', handleCodexLifecycle);

    const handleCodexServerRequest = (request: {
      id: number | string;
      method: string;
      params: Record<string, unknown>;
    }) => {
        const { id, method, params } = request;
        const reqThreadId = params.threadId as string;
        const turnId = params.turnId as string;
        const itemId = params.itemId as string;

        if (method === 'item/commandExecution/requestApproval') {
          addApproval({
            requestId: id,
            kind: 'commandExecution',
            threadId: reqThreadId,
            turnId,
            itemId,
            status: 'pending',
            command: (params.command as string) ?? null,
            cwd: (params.cwd as string) ?? null,
            reason: (params.reason as string) ?? null,
            availableDecisions: parseAvailableDecisions(params.availableDecisions),
            proposedExecpolicyAmendment: parseStringArray(params.proposedExecpolicyAmendment),
            proposedNetworkPolicyAmendments: parseNetworkAmendments(params.proposedNetworkPolicyAmendments),
          });
        }

        if (method === 'item/fileChange/requestApproval') {
          addApproval({
            requestId: id,
            kind: 'fileChange',
            threadId: reqThreadId,
            turnId,
            itemId,
            status: 'pending',
            reason: (params.reason as string) ?? null,
            grantRoot: (params.grantRoot as string) ?? null,
          });
        }
    };

    socket.on('codex.serverRequest', handleCodexServerRequest);

    return () => {
      socket.off('connect', handleConnect);
      socket.off('disconnect', handleDisconnect);
      socket.off('codex.notification', handleCodexNotification);
      socket.off('codex.lifecycle', handleCodexLifecycle);
      socket.off('codex.serverRequest', handleCodexServerRequest);
    };
  }, [
    enabled,
    threadId,
    setConnected,
    queryClient,
    updateCurrentTurn,
    updateTurnItem,
    updateTurnDiff,
    updateTurnPlan,
    appendPlanDelta,
    setLoading,
    expandReasoning,
    collapseReasoning,
    addApproval,
    addSystemMessage,
    addSystemError,
    setTokenUsage,
    setThreadStatus,
    setActiveTurnId,
    clearActiveTurn,
    hydrateTokenUsage,
    hydrateTurnDiffs,
    setThreadTitle,
    resolveApprovalByRequestId,
  ]);

}
