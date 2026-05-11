/**
 * Hook that connects socket.io events to Zustand stores.
 * Handles all Codex app-server notification routing.
 * Also triggers TanStack Query invalidation for relevant events.
 */
import { useEffect } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { getSocket } from '../socket';
import { useConnectionStore } from '../stores/connection-store';
import { useTimelineStore } from '../stores/timeline-store';
import { useFilesStore } from '../stores/files-store';
import { threadsListThreadsQueryKey } from '../generated/api/@tanstack/react-query.gen';

export function useCodexSocket(enabled = true) {
  const setConnected = useConnectionStore((s) => s.setConnected);
  const queryClient = useQueryClient();
  const rootDir = useFilesStore((s) => s.rootDir);
  const expandedDirs = useFilesStore((s) => s.expandedDirs);
  const {
    updateCurrentTurn,
    updateTurnItem,
    updateTurnDiff,
    setLoading,
    expandReasoning,
    collapseReasoning,
    addApproval,
    resolveApproval,
  } = useTimelineStore();

  useEffect(() => {
    if (!enabled) return;

    const socket = getSocket();

    socket.on('connect', () => setConnected(true));
    socket.on('disconnect', () => setConnected(false));

    socket.on(
      'codex.notification',
      (notification: {
        method: string;
        params: Record<string, unknown>;
      }) => {
        const { method, params } = notification;
        const turnId = params.turnId as string | undefined;
        const itemId = params.itemId as string | undefined;

        // Reasoning delta
        if (
          method === 'item/reasoning/summaryTextDelta' &&
          turnId &&
          itemId
        ) {
          const delta = params.delta as string;
          updateTurnItem(turnId, itemId, (existing) => ({
            type: 'reasoning',
            itemId,
            content: (existing?.content ?? '') + delta,
            completed: false,
          }));
          expandReasoning(itemId);
        }

        // Agent message delta
        if (method === 'item/agentMessage/delta' && turnId && itemId) {
          const delta = params.delta as string;
          updateTurnItem(turnId, itemId, (existing) => ({
            type: 'agentMessage',
            itemId,
            content: (existing?.content ?? '') + delta,
            completed: false,
          }));
        }

        // Command execution output delta
        if (
          method === 'item/commandExecution/outputDelta' &&
          turnId &&
          itemId
        ) {
          const delta = params.delta as string;
          updateTurnItem(turnId, itemId, (existing) => ({
            type: 'commandExecution',
            itemId,
            content: (existing?.content ?? '') + delta,
            completed: false,
          }));
        }

        // File change output delta
        if (
          method === 'item/fileChange/outputDelta' &&
          turnId &&
          itemId
        ) {
          const delta = params.delta as string;
          updateTurnItem(turnId, itemId, (existing) => ({
            type: 'fileChange',
            itemId,
            content: (existing?.content ?? '') + delta,
            completed: false,
            filePath: existing?.filePath,
          }));
        }

        // Turn-level unified diff updated
        if (method === 'turn/diff/updated' && turnId) {
          const diff = params.diff as string;
          updateTurnDiff(turnId, diff);
        }

        // Item started — create placeholder for tool calls
        if (method === 'item/started' && turnId) {
          const item = params.item as Record<string, unknown> | undefined;
          if (!item) return;
          const startedItemId = item.id as string;

          if (item.type === 'mcpToolCall') {
            updateTurnItem(turnId, startedItemId, () => ({
              type: 'mcpToolCall',
              itemId: startedItemId,
              content: '',
              completed: false,
              toolServer: (item.server as string) ?? '',
              toolName: (item.tool as string) ?? '',
              toolArgs: item.arguments
                ? JSON.stringify(item.arguments, null, 2)
                : '',
            }));
          }

          if (item.type === 'fileChange') {
            const changes = item.changes as
              | Array<{ file?: string }>
              | undefined;
            const filePath = changes?.[0]?.file ?? '';
            updateTurnItem(turnId, startedItemId, () => ({
              type: 'fileChange',
              itemId: startedItemId,
              content: '',
              completed: false,
              filePath,
            }));
          }

          if (item.type === 'commandExecution') {
            updateTurnItem(turnId, startedItemId, () => ({
              type: 'commandExecution',
              itemId: startedItemId,
              content: '',
              completed: false,
              command: (item.command as string) ?? '',
            }));
          }
        }

        // Item completed — calibrate and mark done
        if (method === 'item/completed' && turnId) {
          const item = params.item as Record<string, unknown> | undefined;
          if (!item) return;
          const completedItemId =
            (params.itemId as string) ?? (item.id as string);

          if (item.type === 'agentMessage') {
            const text = (item.text as string) ?? '';
            updateTurnItem(turnId, completedItemId, () => ({
              type: 'agentMessage',
              itemId: completedItemId,
              content: text,
              completed: true,
            }));
          }

          if (item.type === 'reasoning') {
            updateTurnItem(turnId, completedItemId, (existing) => ({
              ...(existing ?? {
                type: 'reasoning' as const,
                itemId: completedItemId,
                content: '',
              }),
              completed: true,
            }));
            collapseReasoning(completedItemId);
          }

          if (item.type === 'commandExecution') {
            const cmd = (item.command as string) ?? '';
            const output = (item.aggregatedOutput as string) ?? '';
            updateTurnItem(turnId, completedItemId, (existing) => ({
              ...(existing ?? {
                type: 'commandExecution' as const,
                itemId: completedItemId,
                content: '',
              }),
              content: output || existing?.content || '',
              command: cmd || existing?.command,
              exitCode: (item.exitCode as number) ?? existing?.exitCode,
              completed: true,
            }));
          }

          if (item.type === 'mcpToolCall') {
            const result = item.result as Record<string, unknown> | null;
            const resultText = result?.content
              ? JSON.stringify(result.content, null, 2).slice(0, 500)
              : ((item.error as string) ?? '');
            updateTurnItem(turnId, completedItemId, (existing) => ({
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
            const changes = item.changes as
              | Array<{ file?: string }>
              | undefined;
            const filePath = changes?.[0]?.file ?? '';
            updateTurnItem(turnId, completedItemId, (existing) => ({
              ...(existing ?? {
                type: 'fileChange' as const,
                itemId: completedItemId,
              }),
              content: existing?.content ?? '',
              completed: true,
              filePath: existing?.filePath ?? filePath,
            }));
          }
        }

        // Turn completed — invalidate thread list for updated preview
        if (method === 'turn/completed' && turnId) {
          updateCurrentTurn(turnId, (items) => ({
            items,
            completed: true,
          }));
          setLoading(false);
          void queryClient.invalidateQueries({
            queryKey: threadsListThreadsQueryKey(),
          });
        }
      },
    );

    // File watcher events → invalidate affected file queries
    socket.on('fs.changed', (event: { event: string; path: string }) => {
      void queryClient.invalidateQueries({
        predicate: ({ queryKey }) => {
          const key = queryKey[0] as
            | { _id?: string; query?: { path?: string; root?: string } }
            | undefined;
          if (!key?._id) return false;
          if (key._id === 'filesReadTree') return true;
          return (
            (key._id === 'filesReadFile' || key._id === 'filesGetMetadata') &&
            key.query?.path === event.path
          );
        },
      });
    });

    socket.on(
      'codex.serverRequest',
      (request: {
        id: number | string;
        method: string;
        params: Record<string, unknown>;
      }) => {
        const { id, method, params } = request;
        const threadId = params.threadId as string;
        const turnId = params.turnId as string;
        const itemId = params.itemId as string;

        if (method === 'item/commandExecution/requestApproval') {
          addApproval({
            requestId: id,
            kind: 'commandExecution',
            threadId,
            turnId,
            itemId,
            status: 'pending',
            command: (params.command as string) ?? null,
            cwd: (params.cwd as string) ?? null,
            reason: (params.reason as string) ?? null,
          });
        }

        if (method === 'item/fileChange/requestApproval') {
          addApproval({
            requestId: id,
            kind: 'fileChange',
            threadId,
            turnId,
            itemId,
            status: 'pending',
            reason: (params.reason as string) ?? null,
            grantRoot: (params.grantRoot as string) ?? null,
          });
        }
      },
    );

    return () => {
      socket.off('connect');
      socket.off('disconnect');
      socket.off('codex.notification');
      socket.off('codex.serverRequest');
      socket.off('fs.changed');
    };
  }, [
    enabled,
    setConnected,
    queryClient,
    updateCurrentTurn,
    updateTurnItem,
    updateTurnDiff,
    setLoading,
    expandReasoning,
    collapseReasoning,
    addApproval,
    resolveApproval,
  ]);

  // Subscribe/unsubscribe file watchers for visible directories
  useEffect(() => {
    if (!enabled) return;

    const socket = getSocket();
    const watchedDirs = new Set<string>();
    if (rootDir) watchedDirs.add(rootDir);
    for (const dir of expandedDirs) watchedDirs.add(dir);

    for (const path of watchedDirs) {
      socket.emit('fs.subscribe', { path });
    }

    return () => {
      for (const path of watchedDirs) {
        socket.emit('fs.unsubscribe', { path });
      }
    };
  }, [enabled, rootDir, expandedDirs]);
}
