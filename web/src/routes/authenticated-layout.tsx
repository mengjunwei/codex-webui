/**
 * Authenticated layout: sidebar + header + main content outlet.
 * Replaces the old App.tsx conditional rendering.
 */
import { useCallback, useEffect, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { Outlet, useNavigate, useRouterState } from '@tanstack/react-router';
import { useTranslation } from 'react-i18next';
import { TooltipProvider } from '@/components/ui/tooltip';
import {
  Sheet,
  SheetContent,
  SheetTitle,
} from '@/components/ui/sheet';
import { ChatHeader } from '@/components/chat/chat-header';
import { ThreadSidebar } from '@/components/chat/thread-sidebar';
import { SnackbarContainer } from '@/components/snackbar/snackbar-container';
import { CodexStatusBanner } from '@/components/codex-status-banner';
import { useBreakpoint } from '@/hooks/use-breakpoint';
import { useCodexSocket } from '@/hooks/use-codex-socket';
import { useFilesStore } from '@/stores/files-store';
import { useLayoutStore } from '@/stores/layout-store';
import { useTimelineStore } from '@/stores/timeline-store';
import { useThemeStore } from '@/stores/theme-store';
import { cn } from '@/lib/utils';
import { clearApiToken } from '@/auth-token';
import { getSocket, resetSocket } from '@/socket';
import { getRoots } from '@/generated/api';
import {
  list as settingsListSettings,
} from '@/generated/api/sdk.gen';
import { listQueryKey as settingsListSettingsQueryKey } from '@/generated/api/@tanstack/react-query.gen';
import { useTeamStore } from '@/stores/team-store';
import { useUserStore } from '@/stores/user-store';
import { approvalsApi, threadsApi, type PendingApproval } from '@/lib/mt-client';
import type { ApprovalRequest, NetworkPolicyAmendment, RawCommandDecision } from '@/types/approval';

const MAX_IDLE_SUBSCRIPTIONS_KEY = 'general.maxIdleSubscriptions';
const DEFAULT_MAX_IDLE_SUBSCRIPTIONS = 30;
const IDLE_SUBSCRIPTION_CLEANUP_INTERVAL_MS = 5 * 60 * 1000;

/**
 * ResumeThreadResponse: threadsApi.invoke(tid, {method:'thread/resume'}) 的响应形状。
 * 兼容两种情况：直接返回或 { data: ... } 包装。
 */
interface ResumeThreadResponse {
  cwd?: string;
  thread: {
    turns?: Array<{ id?: string; status?: string }>;
    cwd?: string;
    name?: string;
    preview?: string;
    status?: string;
  };
}

/**
 * pendingApprovalToRequest: 新多租户 PendingApproval → ApprovalRequest。
 *
 * PendingApproval.params_json 是 JSON 字符串，字段名是 camelCase。
 * 旧 approvalFromPending 接受 PendingServerRequestDto（已下线）。
 */
interface PendingApprovalDto extends PendingApproval {
  /** 服务端可选返回，用于兼容旧逻辑 */
  params_json: string;
}

function pendingApprovalToRequest(pa: PendingApprovalDto): ApprovalRequest | null {
  let params: Record<string, unknown> = {};
  try {
    params = JSON.parse(pa.params_json) as Record<string, unknown>;
  } catch {
    /* invalid JSON — skip */
  }

  const turnId = typeof params.turnId === 'string' ? params.turnId : pa.turn_id ?? null;
  const itemId = typeof params.itemId === 'string' ? params.itemId : pa.item_id ?? null;
  if (!turnId || !itemId || pa.status !== 'pending') return null;

  if (pa.method === 'item/commandExecution/requestApproval') {
    return {
      requestId: pa.request_id,
      kind: 'commandExecution',
      threadId: pa.thread_id,
      turnId,
      itemId,
      status: 'pending',
      command: (params.command as string) ?? null,
      cwd: (params.cwd as string) ?? null,
      reason: (params.reason as string) ?? null,
      availableDecisions: Array.isArray(params.availableDecisions)
        ? (params.availableDecisions as RawCommandDecision[])
        : [],
      proposedExecpolicyAmendment: Array.isArray(params.proposedExecpolicyAmendment)
        ? (params.proposedExecpolicyAmendment as string[])
        : [],
      proposedNetworkPolicyAmendments: Array.isArray(params.proposedNetworkPolicyAmendments)
        ? (params.proposedNetworkPolicyAmendments as NetworkPolicyAmendment[])
        : [],
    };
  }

  if (pa.method === 'item/fileChange/requestApproval') {
    return {
      requestId: pa.request_id,
      kind: 'fileChange',
      threadId: pa.thread_id,
      turnId,
      itemId,
      status: 'pending',
      reason: (params.reason as string) ?? null,
      grantRoot: (params.grantRoot as string) ?? null,
    };
  }

  return null;
}

function readMaxIdleSubscriptions(
  settings: Array<{ key: string; value: unknown }> | undefined,
): number {
  const value = settings?.find(
    (setting) => setting.key === MAX_IDLE_SUBSCRIPTIONS_KEY,
  )?.value;
  return typeof value === 'number' && Number.isFinite(value)
    ? value
    : DEFAULT_MAX_IDLE_SUBSCRIPTIONS;
}

export function AuthenticatedLayout() {
  const navigate = useNavigate();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const [homeDir, setHomeDir] = useState<string | null>(null);

  const threadCwd = useTimelineStore((s) => s.threadCwd);
  const addApprovalForThread = useTimelineStore((s) => s.addApprovalForThread);
  const ensureThreadState = useTimelineStore((s) => s.ensureThreadState);
  const hydrateTimelineForThread = useTimelineStore((s) => s.hydrateTimelineForThread);
  const setLoadingForThread = useTimelineStore((s) => s.setLoadingForThread);
  const setThreadStatusForThread = useTimelineStore((s) => s.setThreadStatusForThread);
  const setActiveTurnIdForThread = useTimelineStore((s) => s.setActiveTurnIdForThread);
  const setThreadTitleForThread = useTimelineStore((s) => s.setThreadTitleForThread);
  const setActiveThread = useTimelineStore((s) => s.setActiveThread);
  const setMaxIdleSubscriptions = useTimelineStore((s) => s.setMaxIdleSubscriptions);
  const cleanupIdleThreadSubscriptions = useTimelineStore((s) => s.cleanupIdleThreadSubscriptions);
  const setRootDir = useFilesStore((s) => s.setRootDir);
  const dark = useThemeStore((s) => s.dark);
  const toggleDark = useThemeStore((s) => s.toggleDark);
  const generalSettingsQuery = useQuery({
    queryKey: settingsListSettingsQueryKey({ query: { category: 'general' } }),
    queryFn: async () => {
      const { data } = await settingsListSettings({
        query: { category: 'general' },
        throwOnError: true,
      });
      return data;
    },
  });
  const maxIdleSubscriptions = readMaxIdleSubscriptions(
    generalSettingsQuery.data?.settings,
  );

  useCodexSocket(true);

  // 挂载时若尚未拉取当前用户身份(/me),则补拉一次。
  useEffect(() => {
    if (!useUserStore.getState().me) {
      void useUserStore.getState().loadMe();
    }
  }, []);

  useEffect(() => {
    setMaxIdleSubscriptions(maxIdleSubscriptions);
  }, [maxIdleSubscriptions, setMaxIdleSubscriptions]);

  useEffect(() => {
    const timer = window.setInterval(
      () => cleanupIdleThreadSubscriptions(maxIdleSubscriptions),
      IDLE_SUBSCRIPTION_CLEANUP_INTERVAL_MS,
    );
    return () => window.clearInterval(timer);
  }, [cleanupIdleThreadSubscriptions, maxIdleSubscriptions]);

  // Fetch home dir on mount
  useEffect(() => {
    getRoots({ throwOnError: true })
      .then(({ data }) => setHomeDir(data.homeDir))
      .catch(() => undefined);
  }, []);

  // Discover loaded threads and hydrate pending approvals on mount.
  useEffect(() => {
    let cancelled = false;
    const socket = getSocket();

    // 1. Discover loaded threads from multitenant API and subscribe them.
    const discoverLoadedThreads = async () => {
      const { currentTeamId, loadTeams } = useTeamStore.getState();
      let teamId = currentTeamId;
      if (!teamId) {
        await loadTeams();
        teamId = useTeamStore.getState().currentTeamId;
      }
      if (!teamId) return; // 无 team，跳过

      // threadsApi.list(teamId) 返回该 team 下所有 thread（数组）
      const rawThreads: unknown[] = await threadsApi.list(teamId);

      const threadIds: string[] = rawThreads.map((t) =>
        typeof t === 'string' ? t : (t as { id: string }).id,
      );

      for (const tid of threadIds) {
        ensureThreadState({ threadId: tid });
        setLoadingForThread(tid, true);
        socket.emit('thread.subscribe', { threadId: tid });
        useTimelineStore.setState((s) => ({
          subscribedThreadIds: new Set(s.subscribedThreadIds).add(tid),
        }));

        // Resume to get full thread state via new invoke endpoint.
        void threadsApi
          .invoke(tid, { method: 'thread/resume' })
          .then((rawResume) => {
            if (cancelled) return;
            // 兼容：响应可能是 { data: ThreadState } 或直接 ThreadState
            const resumeData =
              rawResume && typeof rawResume === 'object' && 'data' in rawResume
                ? (rawResume as { data: ResumeThreadResponse }).data
                : (rawResume as ResumeThreadResponse);
            if (!resumeData?.thread) return;
            const th = resumeData.thread;
            hydrateTimelineForThread(tid, th.turns ?? [], resumeData.cwd ?? th.cwd);
            setThreadTitleForThread(tid, th.name ?? th.preview ?? null);
            setThreadStatusForThread(tid, (th.status ?? null) as import('@/types/codex-notifications').ThreadStatusType | null);
            const activeTurn = th.turns?.find(
              (turn: { status?: string }) => turn.status === 'inProgress',
            );
            setActiveTurnIdForThread(tid, activeTurn?.id ?? null);
            setLoadingForThread(tid, Boolean(activeTurn));

            // 2. Hydrate pending approvals for this thread via multitenant API.
            void approvalsApi
              .list(tid)
              .then((approvals) => {
                if (cancelled) return;
                for (const pa of approvals) {
                  const approval = pendingApprovalToRequest(pa as PendingApprovalDto);
                  if (approval) addApprovalForThread(tid, approval);
                }
              })
              .catch(() => undefined);
          })
          .catch(() => {
            if (!cancelled) setLoadingForThread(tid, false);
          });
      }
    };
    void discoverLoadedThreads().catch(() => undefined);

    return () => { cancelled = true; };
  }, [
    addApprovalForThread,
    ensureThreadState,
    hydrateTimelineForThread,
    setActiveTurnIdForThread,
    setLoadingForThread,
    setThreadStatusForThread,
    setThreadTitleForThread,
  ]);

  // Handle snackbar jump-to-thread actions.
  useEffect(() => {
    const handleJump = (event: Event) => {
      const threadId = (event as CustomEvent<{ threadId?: string }>).detail?.threadId;
      if (!threadId) return;
      setActiveThread(threadId);
      void navigate({ to: '/t/$threadId', params: { threadId } });
    };
    window.addEventListener('codex-webui:jump-thread', handleJump);
    return () => window.removeEventListener('codex-webui:jump-thread', handleJump);
  }, [navigate, setActiveThread]);

  // Handle auth expiry → redirect to /login
  useEffect(() => {
    const handleAuthExpired = () => {
      clearApiToken();
      resetSocket();
      useUserStore.getState().clearMe();
      void navigate({ to: '/login', search: { redirect: '/' } });
    };
    window.addEventListener('codex-webui:auth-expired', handleAuthExpired);
    return () => window.removeEventListener('codex-webui:auth-expired', handleAuthExpired);
  }, [navigate]);

  // Sync file tree root based on current route context
  useEffect(() => {
    const dir = pathname.startsWith('/files')
      ? homeDir
      : pathname.startsWith('/t/')
        ? threadCwd
        : null;
    setRootDir(dir);
  }, [pathname, threadCwd, homeDir, setRootDir]);

  const { t } = useTranslation();
  const handleToggleDiagnostics = useCallback(() => {
    void navigate({ to: '/diagnostics' });
  }, [navigate]);

  // ── Responsive layout ────────────────────────────────────────────────
  const breakpoint = useBreakpoint();
  const isDesktop = breakpoint === 'desktop';
  const sidebarOpen = useLayoutStore((s) => s.sidebarOpen);
  const setSidebarOpen = useLayoutStore((s) => s.setSidebarOpen);
  const desktopSidebarCollapsed = useLayoutStore((s) => s.desktopSidebarCollapsed);

  // Auto-close sidebar sheet on route change
  useEffect(() => {
    setSidebarOpen(false);
  }, [pathname, setSidebarOpen]);

  // Auto-close sidebar sheet when entering desktop breakpoint
  useEffect(() => {
    if (isDesktop) setSidebarOpen(false);
  }, [isDesktop, setSidebarOpen]);

  return (
    <TooltipProvider>
      <div className="flex h-full overflow-hidden bg-background">
        {/* Desktop: inline sidebar with collapse animation */}
        {isDesktop && (
          <aside
            className={cn(
              'relative z-10 shrink-0 overflow-hidden border-r border-[var(--glass-border-subtle)] transition-[width] duration-200 ease-in-out',
              desktopSidebarCollapsed ? 'w-0 border-r-0' : 'w-64',
            )}
          >
            <div className="flex h-full w-64 flex-col">
              <ThreadSidebar />
            </div>
          </aside>
        )}

        {/* Mobile/Tablet: sidebar as Sheet overlay */}
        {!isDesktop && (
          <Sheet open={sidebarOpen} onOpenChange={setSidebarOpen}>
            <SheetContent side="left" className="!w-[280px] p-0 sm:!max-w-[320px]" showCloseButton={false}>
              <SheetTitle className="sr-only">{t('Navigation')}</SheetTitle>
              <ThreadSidebar />
            </SheetContent>
          </Sheet>
        )}

        <div className="flex min-h-0 min-w-0 flex-1 flex-col isolate">
          <ChatHeader
            dark={dark}
            onToggleDark={toggleDark}
            onToggleDiagnostics={handleToggleDiagnostics}
          />
          <CodexStatusBanner />
          <Outlet />
        </div>
      </div>
      <SnackbarContainer />
    </TooltipProvider>
  );
}
