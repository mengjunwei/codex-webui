/**
 * Left sidebar: global actions (top) + workspace-grouped thread navigation.
 * Rendering is split into sidebar/ sub-components; this file orchestrates
 * state, queries, mutations, and view routing.
 */
import { useMemo, useState } from 'react';
import { FolderOpen, PanelLeftClose, Plus, Settings, Terminal } from 'lucide-react';
import { useNavigate, useRouterState } from '@tanstack/react-router';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Separator } from '@/components/ui/separator';
import { threadsApi, tokenUsageApi, turnDiffApi, turnErrorApi } from '@/lib/mt-client';
import { useTeamStore } from '@/stores/team-store';
import { TeamSelector } from './team-selector';
import type { ThreadDto } from '@/lib/mt-client';
import { useTimelineStore } from '@/stores/timeline-store';
import { useLayoutStore } from '@/stores/layout-store';
import { cn } from '@/lib/utils';
import { getApiErrorMessage } from '@/lib/api-error';
import type { ConfirmAction } from './sidebar/sidebar-types';
import { threadLabel, groupByWorkspace } from './sidebar/sidebar-types';
import { ThreadRow } from './sidebar/thread-row';
import { WorkspaceOverview } from './sidebar/workspace-overview';
import { RenameDialog, ConfirmDialog } from './sidebar/sidebar-dialogs';
import { WorkspaceSelectorDialog } from './sidebar/workspace-selector-dialog';

/** Derives the active "view" from the current route path. */
function useActiveView(): 'chat' | 'files' | 'terminal' | 'diagnostics' | 'settings' | 'integrations' | 'other' {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  if (pathname.startsWith('/files')) return 'files';
  if (pathname.startsWith('/terminal')) return 'terminal';
  if (pathname.startsWith('/diagnostics')) return 'diagnostics';
  if (pathname.startsWith('/integrations')) return 'integrations';
  if (pathname.startsWith('/settings')) return 'settings';
  if (pathname === '/' || pathname.startsWith('/t/')) return 'chat';
  return 'other';
}

export function ThreadSidebar() {
  const navigate = useNavigate();
  const activeView = useActiveView();
  const { t } = useTranslation();
  const threadId = useTimelineStore((s) => s.threadId);
  const threadMode = useTimelineStore((s) => s.threadMode);
  const loading = useTimelineStore((s) => s.loading);
  const approvals = useTimelineStore((s) => s.approvals);
  const threadStatus = useTimelineStore((s) => s.threadStatus);
  const threadsById = useTimelineStore((s) => s.threadsById);
  const setActiveThread = useTimelineStore((s) => s.setActiveThread);
  const clearThread = useTimelineStore((s) => s.clearThread);
  const setThreadTitle = useTimelineStore((s) => s.setThreadTitle);
  const hydrateTimelineForThread = useTimelineStore((s) => s.hydrateTimelineForThread);
  const hydrateTokenUsageForThread = useTimelineStore((s) => s.hydrateTokenUsageForThread);
  const hydrateTurnDiffsForThread = useTimelineStore((s) => s.hydrateTurnDiffsForThread);
  const hydrateTurnErrorsForThread = useTimelineStore((s) => s.hydrateTurnErrorsForThread);
  const setThreadTitleForThread = useTimelineStore((s) => s.setThreadTitleForThread);
  const setLoadingForThread = useTimelineStore((s) => s.setLoadingForThread);
  const setThreadStatusForThread = useTimelineStore((s) => s.setThreadStatusForThread);
  const setActiveTurnIdForThread = useTimelineStore((s) => s.setActiveTurnIdForThread);
  const addSystemError = useTimelineStore((s) => s.addSystemError);
  const queryClient = useQueryClient();
  const teams = useTeamStore((s) => s.teams);

  // ── Layout store (sidebar view + collapsed groups + collapse) ────────
  const collapsedGroupKeys = useLayoutStore((s) => s.collapsedGroupKeys);
  const toggleCollapsedGroup = useLayoutStore((s) => s.toggleCollapsedGroup);
  const toggleDesktopSidebarCollapsed = useLayoutStore((s) => s.toggleDesktopSidebarCollapsed);
  // Derive Set<string> for child components that expect it
  const collapsedGroups = useMemo(() => new Set(collapsedGroupKeys), [collapsedGroupKeys]);

  // ── Local UI state (ephemeral) ─────────────────────────────────────
  const [renameThread, setRenameThread] = useState<ThreadDto | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [confirmAction, setConfirmAction] = useState<ConfirmAction>(null);
  const [wsSelectorOpen, setWsSelectorOpen] = useState(false);
  // ── Queries ─────────────────────────────────────────────────────────
  // 聚合列表:当前用户所有团队 workspace + 个人 workspace 的全部会话(一次拉取)。
  const myThreadsQuery = useQuery({
    queryKey: ['threads', 'mine'],
    queryFn: () => threadsApi.listMine(),
  });

  const allThreads = useMemo(() => myThreadsQuery.data ?? [], [myThreadsQuery.data]);
  // 按 workspace 分组(个人/各 team),组内再分 active/archived。
  const workspaceGroups = useMemo(() => groupByWorkspace(allThreads, teams), [allThreads, teams]);

  const invalidateThreads = () => {
    void queryClient.invalidateQueries({ queryKey: ['threads', 'mine'] });
  };

  // ── Thread open helpers ─────────────────────────────────────────────
  const resumeThread = useMutation({
    mutationFn: (vars: { path: { threadId: string } }) =>
      threadsApi.invoke(vars.path.threadId, { method: 'thread/resume' }),
    onSuccess: (res) => {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const rawResume = res as any;
      const thread = rawResume.thread ?? {};
      const tid: string = thread.id;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const turns: any[] = (thread.turns ?? []) as any[];
      setThreadTitleForThread(tid, threadLabel(thread));
      hydrateTimelineForThread(tid, turns, rawResume.cwd ?? thread.cwd);
      setThreadStatusForThread(tid, thread.status);
      const activeTurn = turns.find((turn) => (turn as { status?: string }).status === 'inProgress');
      setActiveTurnIdForThread(tid, (activeTurn as { id?: string } | undefined)?.id ?? null);
      setLoadingForThread(tid, Boolean(activeTurn));
      void tokenUsageApi.get(tid)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .then((data: unknown) => data && hydrateTokenUsageForThread(tid, data as any))
        .catch(() => undefined);
      void turnDiffApi.list(tid)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .then((data: unknown) => data && hydrateTurnDiffsForThread(tid, data as any))
        .catch(() => undefined);
      void turnErrorApi.list(tid)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .then((data: unknown) => data && hydrateTurnErrorsForThread(tid, data as any))
        .catch(() => undefined);
    },
    onError: (_err, vars) => setLoadingForThread(vars.path.threadId, false),
  });

  /** Navigate to archived thread — ThreadView handles loading (resume → fail → read). */
  const openArchivedThread = (thread: ThreadDto) => {
    if (thread.id === threadId && threadMode === 'readOnly') return;
    void navigate({ to: '/t/$threadId', params: { threadId: thread.id } });
  };

  const openLiveThread = (thread: ThreadDto) => {
    if (thread.id === threadId && threadMode === 'live') return;
    setActiveThread(thread.id, thread.cwd, threadLabel(thread));
    setLoadingForThread(thread.id, true);
    resumeThread.mutate({ path: { threadId: thread.id } });
    void navigate({ to: '/t/$threadId', params: { threadId: thread.id } });
  };

  const switchAfterArchive = (archivedId: string) => {
    const current = useTimelineStore.getState();
    if (current.threadId !== archivedId || current.threadMode !== 'live') return;
    const live = allThreads.filter((th) => th.status !== 'archived' && th.id !== archivedId);
    const next = live[0];
    if (next) openLiveThread(next);
    else { clearThread(); void navigate({ to: '/' }); }
  };

  // ── Mutations ───────────────────────────────────────────────────────
  // 创建会话:teamId 缺省 → 个人 workspace(后端 is_personal=true);否则团队 workspace。
  const createThread = useMutation({
    mutationFn: (vars: { body: { teamId?: string; cwd?: string } }) =>
      threadsApi.create(vars.body),
    onSuccess: (res: any) => {
      const tid = res.thread?.thread?.id || res.id || res.thread?.id;
      const cwd = res.cwd || res.thread?.cwd || '';
      setActiveThread(tid, cwd, threadLabel(res.thread?.thread || res.thread || res));
      invalidateThreads();
      void navigate({ to: '/t/$threadId', params: { threadId: tid } });
    },
    onError: (err) => addSystemError(getApiErrorMessage(err)),
  });

  // 删除会话(含归档):后端校验权限(个人自删/团队仅创建者) + 清 PG + 删 rollout。
  const deleteThread = useMutation({
    mutationFn: (vars: { path: { threadId: string } }) =>
      threadsApi.remove(vars.path.threadId),
    onSuccess: (_res, vars) => {
      useTimelineStore.getState().unsubscribeThread(vars.path.threadId);
      // 删除的是当前会话 → 切回首页。
      if (useTimelineStore.getState().threadId === vars.path.threadId) {
        clearThread();
        void navigate({ to: '/' });
      }
      invalidateThreads();
    },
    onError: (err) => addSystemError(getApiErrorMessage(err)),
  });

  const archiveThread = useMutation({
    mutationFn: (vars: { path: { threadId: string } }) =>
      threadsApi.archive(vars.path.threadId),
    onSuccess: (_res, vars) => {
      useTimelineStore.getState().unsubscribeThread(vars.path.threadId);
      invalidateThreads();
      switchAfterArchive(vars.path.threadId);
    },
  });

  const unarchiveThread = useMutation({
    mutationFn: (vars: { path: { threadId: string } }) =>
      threadsApi.invoke(vars.path.threadId, { method: 'thread/unarchive' }),
    onSuccess: (res) => {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const thread = (res as any).thread;
      invalidateThreads();
      if (thread && threadId === thread.id && threadMode === 'readOnly') openLiveThread(thread);
    },
  });

  const compactThread = useMutation({
    mutationFn: (vars: { path: { threadId: string } }) =>
      threadsApi.invoke(vars.path.threadId, { method: 'thread/compact' }),
    onSuccess: () => invalidateThreads(),
  });

  const forkThread = useMutation({
    mutationFn: (vars: { path: { threadId: string } }) =>
      threadsApi.invoke(vars.path.threadId, { method: 'thread/fork' }),
    onSuccess: (res) => {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const rawFork = res as any;
      const thread = rawFork.thread ?? {};
      const tid: string = thread.id;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const turns: any[] = (thread.turns ?? []) as any[];
      setActiveThread(tid, rawFork.cwd ?? thread.cwd, threadLabel(thread));
      hydrateTimelineForThread(tid, turns, rawFork.cwd ?? thread.cwd);
      void tokenUsageApi.get(tid)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .then((data: unknown) => data && hydrateTokenUsageForThread(tid, data as any))
        .catch(() => undefined);
      void turnDiffApi.list(tid)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .then((data: unknown) => data && hydrateTurnDiffsForThread(tid, data as any))
        .catch(() => undefined);
      void turnErrorApi.list(tid)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .then((data: unknown) => data && hydrateTurnErrorsForThread(tid, data as any))
        .catch(() => undefined);
      invalidateThreads();
      void navigate({ to: '/t/$threadId', params: { threadId: tid } });
    },
  });

  const updateThreadName = useMutation({
    mutationFn: (vars: { path: { threadId: string }; body: { name: string } }) =>
      threadsApi.rename(vars.path.threadId, { name: vars.body.name }),
    onSuccess: (_res, vars) => {
      if (vars.path.threadId === threadId) setThreadTitle(vars.body.name.trim());
      setRenameThread(null);
      setRenameValue('');
      invalidateThreads();
    },
  });

  // ── View navigation helpers ─────────────────────────────────────────

  // ── Rename / Confirm ────────────────────────────────────────────────
  const startRename = (thread: ThreadDto) => { setRenameThread(thread); setRenameValue(threadLabel(thread)); };
  const saveRename = () => {
    if (!renameThread) return;
    const name = renameValue.trim();
    if (!name) return;
    updateThreadName.mutate({ path: { threadId: renameThread.id }, body: { name } });
  };
  const confirmCurrentAction = () => {
    if (!confirmAction) return;
    if (confirmAction.type === 'archive') archiveThread.mutate({ path: { threadId: confirmAction.thread.id } });
    if (confirmAction.type === 'compact') compactThread.mutate({ path: { threadId: confirmAction.thread.id } });
    if (confirmAction.type === 'delete') deleteThread.mutate({ path: { threadId: confirmAction.thread.id } });
    setConfirmAction(null);
  };

  // ── Shared thread-row renderer (passed to overview/detail) ──────────
  const renderThreadRow = (thread: ThreadDto, archived: boolean) => {
    const runtime = thread.id === threadId
      ? { loading, approvals, threadStatus }
      : threadsById[thread.id];
    const isRunning = Boolean(runtime?.loading);
    const activeFlags = runtime?.threadStatus?.type === 'active'
      ? runtime.threadStatus.activeFlags
      : [];
    // Count hydrated pending approvals (source of truth for badge).
    const pendingApprovalCount = Object.values(runtime?.approvals ?? {}).filter(
      (a) => a.status === 'pending',
    ).length;
    const waitingOnApproval =
      activeFlags.includes('waitingOnApproval') || pendingApprovalCount > 0;
    const waitingOnUserInput = activeFlags.includes('waitingOnUserInput');
    // "Generating" = thread active but not blocked on any user-facing request.
    const generating =
      runtime?.threadStatus?.type === 'active' &&
      !waitingOnApproval &&
      !waitingOnUserInput;

    return (
      <ThreadRow
        key={thread.id}
        thread={thread}
        archived={archived}
        isActive={thread.id === threadId && activeView === 'chat'}
        destructiveDisabled={isRunning}
        actionPending={forkThread.isPending || unarchiveThread.isPending || deleteThread.isPending}
        running={generating || isRunning}
        pendingApproval={waitingOnApproval}
        pendingApprovalCount={pendingApprovalCount}
        waitingOnUserInput={waitingOnUserInput}
        onOpen={() => { if (archived) void openArchivedThread(thread); else openLiveThread(thread); }}
        onRename={() => startRename(thread)}
        onArchive={() => setConfirmAction({ type: 'archive', thread })}
        onUnarchive={() => unarchiveThread.mutate({ path: { threadId: thread.id } })}
        onCompact={() => setConfirmAction({ type: 'compact', thread })}
        onFork={() => forkThread.mutate({ path: { threadId: thread.id } })}
        onDelete={() => setConfirmAction({ type: 'delete', thread })}
      />
    );
  };

  // ── Render ──────────────────────────────────────────────────────────
  return (
    <div className="flex h-full flex-col bg-card/80">
      {/* Team selector + management */}
      <div className="px-2 py-2 space-y-1">
        <TeamSelector />
      </div>
      <Separator />
      {/* Global actions */}
      <div className="space-y-0.5 px-2 py-2">
        <button
          type="button"
          onClick={() => void navigate({ to: '/files' })}
          className={cn(
            'flex w-full cursor-pointer items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm transition-colors',
            activeView === 'files'
              ? 'bg-accent text-accent-foreground'
              : 'text-muted-foreground hover:bg-accent/50 hover:text-foreground',
          )}
        >
          <FolderOpen className="h-4 w-4 shrink-0" />
          {t('Files')}
        </button>
        <button
          type="button"
          onClick={() => void navigate({ to: '/terminal' })}
          className={cn(
            'flex w-full cursor-pointer items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm transition-colors',
            activeView === 'terminal'
              ? 'bg-accent text-accent-foreground'
              : 'text-muted-foreground hover:bg-accent/50 hover:text-foreground',
          )}
        >
          <Terminal className="h-4 w-4 shrink-0" />
          {t('Terminal')}
        </button>
        {/* 集成菜单临时下线:多租户迁移后集成配置是全局的,显示会误导用户。
              per-team 集成配置接口待后续实现后再恢复。 */}
        <button
          type="button"
          onClick={() => void navigate({ to: '/settings' })}
          className={cn(
            'flex w-full cursor-pointer items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm transition-colors',
            activeView === 'settings'
              ? 'bg-accent text-accent-foreground'
              : 'text-muted-foreground hover:bg-accent/50 hover:text-foreground',
          )}
        >
          <Settings className="h-4 w-4 shrink-0" />
          {t('Settings')}
        </button>
      </div>

      <Separator />

      {/* Thread list header */}
      <div className="flex items-center justify-between px-3 py-2">
        <span className="text-xs font-medium text-muted-foreground">{t('Threads')}</span>
        <Button
          size="icon"
          variant="ghost"
          className="h-6 w-6"
          aria-label={t('New workspace thread')}
          title={t('New workspace thread')}
          onClick={() => setWsSelectorOpen(true)}
        >
          <Plus className="h-3.5 w-3.5" />
        </Button>
      </div>

      <ScrollArea className="min-h-0 flex-1 px-2 [&_[data-slot=scroll-area-viewport]>div]:block!">
        <WorkspaceOverview
          workspaceGroups={workspaceGroups}
          collapsedGroups={collapsedGroups}
          isLoading={myThreadsQuery.isLoading}
          onToggleCollapse={toggleCollapsedGroup}
          onCreateInWorkspace={(group) =>
            createThread.mutate({
              body: { teamId: group.workspace_type === 'team' ? group.key : undefined },
            })
          }
          renderThreadRow={renderThreadRow}
        />
      </ScrollArea>

      {/* Desktop collapse toggle (hidden in mobile Sheet) */}
      <div className="hidden shrink-0 border-t border-border px-2 py-1.5 lg:block">
        <button
          type="button"
          onClick={toggleDesktopSidebarCollapsed}
          className="flex w-full cursor-pointer items-center gap-2 rounded-lg px-2.5 py-2 text-sm text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
        >
          <PanelLeftClose className="h-4 w-4 shrink-0" />
          {t('Collapse sidebar')}
        </button>
      </div>

      <RenameDialog
        open={renameThread !== null}
        pending={updateThreadName.isPending}
        value={renameValue}
        onChange={setRenameValue}
        onSave={saveRename}
        onClose={() => setRenameThread(null)}
      />
      <ConfirmDialog
        action={confirmAction}
        pending={archiveThread.isPending || compactThread.isPending || deleteThread.isPending}
        onConfirm={confirmCurrentAction}
        onClose={() => setConfirmAction(null)}
      />
      <WorkspaceSelectorDialog
        open={wsSelectorOpen}
        onClose={() => setWsSelectorOpen(false)}
        onSelect={(ws) => {
          // 个人 workspace:不传 teamId(后端 is_personal=true);团队:传 teamId。
          createThread.mutate({
            body: { teamId: ws.type === 'team' ? ws.teamId : undefined },
          });
          setWsSelectorOpen(false);
        }}
      />
    </div>
  );
}
