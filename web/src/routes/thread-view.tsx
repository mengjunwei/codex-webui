/**
 * Thread route component — resumes/reads a thread by URL param.
 * Selecting a thread no longer clears other live thread state.
 */
import { useEffect, useRef, useState, useCallback } from 'react';
import { useParams, useNavigate } from '@tanstack/react-router';
import { useMutation } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ChatTimeline } from '@/components/chat/chat-timeline';
import { ChatInput, type ChatInputHandle } from '@/components/chat/chat-input';
import { SessionPanel } from '@/components/chat/session-panel';
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from '@/components/ui/resizable';
import {
  Sheet,
  SheetContent,
  SheetTitle,
} from '@/components/ui/sheet';
import { useBreakpoint } from '@/hooks/use-breakpoint';
import { useTimelineStore } from '@/stores/timeline-store';
import { showSnackbar } from '@/stores/snackbar-store';
import { threadsApi, tokenUsageApi, turnDiffApi, turnErrorApi } from '@/lib/mt-client';

/** Extracts a display label from a thread DTO. */
function threadLabel(thread: { name?: string | null; preview?: string | null }): string {
  return thread.name ?? thread.preview ?? '';
}

export function ThreadView() {
  const { threadId } = useParams({ strict: false }) as { threadId: string };
  const { t } = useTranslation();
  const navigate = useNavigate();
  const chatInputRef = useRef<ChatInputHandle>(null);
  const [sessionPanelOpen, setSessionPanelOpen] = useState(false);

  const threadCwd = useTimelineStore((s) => s.threadCwd);
  const setActiveThread = useTimelineStore((s) => s.setActiveThread);
  const setReadOnlyThread = useTimelineStore((s) => s.setReadOnlyThread);
  const hydrateTimelineForThread = useTimelineStore((s) => s.hydrateTimelineForThread);
  const hydrateTokenUsageForThread = useTimelineStore((s) => s.hydrateTokenUsageForThread);
  const hydrateTurnDiffsForThread = useTimelineStore((s) => s.hydrateTurnDiffsForThread);
  const hydrateTurnErrorsForThread = useTimelineStore((s) => s.hydrateTurnErrorsForThread);
  const setThreadTitleForThread = useTimelineStore((s) => s.setThreadTitleForThread);
  const setThreadStatusForThread = useTimelineStore((s) => s.setThreadStatusForThread);
  const setActiveTurnIdForThread = useTimelineStore((s) => s.setActiveTurnIdForThread);
  const setLoadingForThread = useTimelineStore((s) => s.setLoadingForThread);

  // Pending file open request from @mention click or image badge click.
  // Uses { path, seq } so re-clicking the same file still triggers a new open.
  const openSeqRef = useRef(0);
  const [pendingOpenFile, setPendingOpenFile] = useState<{ path: string; seq: number } | null>(null);

  // Listen for codex-webui:open-file events from chat message badges
  useEffect(() => {
    const handler = (e: Event) => {
      const path = (e as CustomEvent<{ path: string }>).detail?.path;
      if (!path) return;
      setSessionPanelOpen(true);
      setPendingOpenFile({ path, seq: ++openSeqRef.current });
    };
    window.addEventListener('codex-webui:open-file', handler);
    return () => window.removeEventListener('codex-webui:open-file', handler);
  }, []);

  const handleFileOpened = useCallback(() => {
    setPendingOpenFile(null);
  }, []);

  /** 加载 token-usage / turn-diffs / turn-errors 补充数据 */
  const loadSupplementaryData = useCallback((tid: string) => {
    void tokenUsageApi
      .get(tid)
      .then((rows) => {
        if (rows.length > 0) {
          hydrateTokenUsageForThread(
            tid,
            rows.map((r) => ({
              turnId: r.turn_id,
              usage: {
                total: {
                  totalTokens: r.total_tokens,
                  inputTokens: r.input_tokens,
                  outputTokens: r.output_tokens,
                  cachedInputTokens: r.cached_input_tokens,
                  reasoningOutputTokens: r.reasoning_output_tokens,
                },
                last: {
                  totalTokens: r.total_tokens,
                  inputTokens: r.input_tokens,
                  outputTokens: r.output_tokens,
                  cachedInputTokens: r.cached_input_tokens,
                  reasoningOutputTokens: r.reasoning_output_tokens,
                },
                modelContextWindow: r.model_context_window ?? 0,
              } as any,
            })),
          );
        }
      })
      .catch(() => undefined);
    void turnDiffApi
      .list(tid)
      .then((rows) => {
        if (rows.length > 0) {
          hydrateTurnDiffsForThread(tid, rows.map((r) => ({ turnId: r.turn_id, diff: r.diff })));
        }
      })
      .catch(() => undefined);
    void turnErrorApi
      .list(tid)
      .then((rows) => {
        if (rows.length > 0) {
          hydrateTurnErrorsForThread(tid, rows.map((r) => ({ turnId: r.turn_id, message: r.message })));
        }
      })
      .catch(() => undefined);
  }, [hydrateTokenUsageForThread, hydrateTurnDiffsForThread, hydrateTurnErrorsForThread]);

  const resumeThread = useMutation({
    mutationFn: (threadId: string) =>
      threadsApi.invoke(threadId, { method: 'thread/resume' }),
    onSuccess: (res: any) => {
      const tid = res.thread.id;
      const title = threadLabel(res.thread);
      setThreadTitleForThread(tid, title);
      hydrateTimelineForThread(tid, res.thread.turns, res.cwd);
      // Restore active turn state so sidebar shows loading and input stays in steer mode.
      setThreadStatusForThread(tid, res.thread.status);
      const activeTurn = res.thread.turns.find((t: any) => t.status === 'inProgress');
      if (activeTurn) {
        setActiveTurnIdForThread(tid, activeTurn.id);
        setLoadingForThread(tid, true);
      } else {
        setLoadingForThread(tid, false);
      }
      loadSupplementaryData(tid);
    },
    onError: (_err, threadId) => {
      setLoadingForThread(threadId, false);
      // Only attempt archived read if this thread is still selected.
      if (useTimelineStore.getState().threadId === threadId) {
        void tryReadArchived(threadId);
      }
    },
  });

  /** Fallback: try to read the thread as an archived snapshot. */
  const tryReadArchived = async (targetId: string) => {
    try {
      const res: any = await threadsApi.invoke(targetId, { method: 'thread/read', params: { includeTurns: true } });
      // Guard: user may have navigated away during the fetch.
      if (useTimelineStore.getState().threadId !== targetId) return;
      setReadOnlyThread(res.thread);
      loadSupplementaryData(targetId);
    } catch {
      if (useTimelineStore.getState().threadId !== targetId) return;
      showSnackbar(t('Thread not found or cannot be opened.'), 'error');
      void navigate({ to: '/' });
    }
  };

  // Load or select thread when URL param changes. Backend ensures resume is deduped.
  // 关闭终端面板:切会话后旧 contextKey 的终端内容残留,且新 thread 的 cwd 可能
  // 还没从 resume 加载完(terminal cwd 沙箱校验失败)。用户重新点开即用正确 context。
  useEffect(() => {
    setSessionPanelOpen(false);
    setActiveThread(threadId);
    setLoadingForThread(threadId, true);
    resumeThread.mutate(threadId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [threadId]);

  const breakpoint = useBreakpoint();
  const isDesktop = breakpoint === 'desktop';
  const showPanel = sessionPanelOpen && !!threadCwd;

  const sessionPanelContent = showPanel ? (
    <SessionPanel
      threadId={threadId}
      cwd={threadCwd!}
      onClose={() => setSessionPanelOpen(false)}
      openFile={pendingOpenFile?.path ?? null}
      openFileSeq={pendingOpenFile?.seq ?? -1}
      onFileOpened={handleFileOpened}
    />
  ) : null;

  return (
    <>
      {showPanel && isDesktop ? (
        /* Desktop: resizable vertical split */
        <ResizablePanelGroup orientation="vertical" className="min-h-0 flex-1">
          <ResizablePanel defaultSize="65%" minSize="20%">
            <div className="flex h-full flex-col">
              <ChatTimeline onEditMessage={(v) => chatInputRef.current?.setInput(v)} />
            </div>
          </ResizablePanel>
          <ResizableHandle withHandle />
          <ResizablePanel defaultSize="35%" minSize="15%">
            <div className="flex h-full flex-col">
              {sessionPanelContent}
            </div>
          </ResizablePanel>
        </ResizablePanelGroup>
      ) : (
        <ChatTimeline onEditMessage={(v) => chatInputRef.current?.setInput(v)} />
      )}

      {/* Mobile/Tablet: session panel as bottom Sheet */}
      {!isDesktop && (
        <Sheet open={showPanel} onOpenChange={(open) => { if (!open) setSessionPanelOpen(false); }}>
          <SheetContent side="bottom" className="!h-[70dvh] p-0" showCloseButton={false}>
            <SheetTitle className="sr-only">{t('Session panel')}</SheetTitle>
            <div className="flex h-full flex-col">
              {sessionPanelContent}
            </div>
          </SheetContent>
        </Sheet>
      )}

      <ChatInput
        ref={chatInputRef}
        panelOpen={sessionPanelOpen}
        onTogglePanel={() => setSessionPanelOpen((o) => !o)}
      />
    </>
  );
}
