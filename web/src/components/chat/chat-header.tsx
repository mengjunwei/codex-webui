import { useState } from 'react';
import { Activity, Check, Edit3, Globe, Moon, Settings, Sun, X } from 'lucide-react';
import { useNavigate, useRouterState } from '@tanstack/react-router';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Separator } from '@/components/ui/separator';
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import {
  threadsListThreadsQueryKey,
  threadsSetThreadNameMutation,
} from '@/generated/api/@tanstack/react-query.gen';
import { useConnectionStore } from '@/stores/connection-store';
import { useTimelineStore } from '@/stores/timeline-store';
import { AccountRateLimitBadge } from './account-rate-limit-badge';
import { McpStatusBadge } from './mcp-status-badge';

interface Props {
  dark: boolean;
  onToggleDark: () => void;
  onToggleDiagnostics: () => void;
}

export function ChatHeader({ dark, onToggleDark, onToggleDiagnostics }: Props) {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const connected = useConnectionStore((s) => s.connected);
  const threadId = useTimelineStore((s) => s.threadId);
  const threadTitle = useTimelineStore((s) => s.threadTitle);
  const threadMode = useTimelineStore((s) => s.threadMode);
  const setThreadTitle = useTimelineStore((s) => s.setThreadTitle);
  const [editing, setEditing] = useState(false);
  const [draftName, setDraftName] = useState('');
  const queryClient = useQueryClient();

  const isDiagnostics = useRouterState({
    select: (s) => s.location.pathname.startsWith('/diagnostics'),
  });

  const renameThread = useMutation({
    ...threadsSetThreadNameMutation(),
    onSuccess: (_res, variables) => {
      setThreadTitle(variables.body.name.trim());
      setEditing(false);
      void queryClient.invalidateQueries({ queryKey: threadsListThreadsQueryKey() });
    },
  });

  const toggleLanguage = () => {
    const next = i18n.language.startsWith('zh') ? 'en' : 'zh-CN';
    void i18n.changeLanguage(next);
  };

  const startEditing = () => {
    setDraftName(threadTitle ?? '');
    setEditing(true);
  };

  const saveName = () => {
    if (!threadId) return;
    const name = draftName.trim();
    if (!name) return;
    renameThread.mutate({ path: { threadId }, body: { name } });
  };

  const handleDiagnosticsToggle = () => {
    if (isDiagnostics) {
      if (threadId) {
        void navigate({ to: '/t/$threadId', params: { threadId } });
      } else {
        void navigate({ to: '/' });
      }
    } else {
      onToggleDiagnostics();
    }
  };

  return (
    <>
      <header className="glass-4 sticky top-0 z-10 flex items-center gap-3 px-4 py-3 md:px-6">
        <div className="min-w-0 flex-1">
          {threadId ? (
            <div className="flex min-w-0 items-center gap-2">
              {editing ? (
                <>
                  <Input
                    value={draftName}
                    onChange={(event) => setDraftName(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') saveName();
                      if (event.key === 'Escape') setEditing(false);
                    }}
                    className="h-8 max-w-sm"
                    autoFocus
                  />
                  <Button size="icon" variant="ghost" className="h-8 w-8" onClick={saveName} disabled={!draftName.trim()}>
                    <Check className="h-4 w-4" />
                  </Button>
                  <Button size="icon" variant="ghost" className="h-8 w-8" onClick={() => setEditing(false)}>
                    <X className="h-4 w-4" />
                  </Button>
                </>
              ) : (
                <>
                  <button
                    type="button"
                    className="truncate text-left text-lg font-semibold tracking-tight hover:underline"
                    onClick={startEditing}
                    title={threadTitle ?? threadId}
                  >
                    {threadTitle || threadId.slice(0, 8)}
                  </button>
                  <Button size="icon" variant="ghost" className="h-7 w-7" onClick={startEditing} title={t('Rename')}>
                    <Edit3 className="h-3.5 w-3.5" />
                  </Button>
                  {threadMode === 'readOnly' && (
                    <Badge variant="secondary" className="text-xs">
                      {t('Archived read-only')}
                    </Badge>
                  )}
                </>
              )}
            </div>
          ) : (
            <h1 className="text-lg font-semibold tracking-tight">Codex WebUI</h1>
          )}
        </div>
        <McpStatusBadge mode="header" />
        <AccountRateLimitBadge />
        <Badge
          variant={connected ? 'default' : 'secondary'}
          className="text-xs transition-colors duration-300"
        >
          <span
            className={`mr-1.5 inline-block h-1.5 w-1.5 rounded-full ${
              connected
                ? 'animate-pulse bg-green-400'
                : 'bg-muted-foreground'
            }`}
          />
          {connected ? t('Connected') : t('Disconnected')}
        </Badge>

        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={handleDiagnosticsToggle}
            >
              <Activity className="h-4 w-4" />
            </Button>
          </TooltipTrigger>
          <TooltipContent>{t('Diagnostics')}</TooltipContent>
        </Tooltip>

        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={() => void navigate({ to: '/settings' })}
            >
              <Settings className="h-4 w-4" />
            </Button>
          </TooltipTrigger>
          <TooltipContent>{t('Settings')}</TooltipContent>
        </Tooltip>

        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={toggleLanguage}
            >
              <Globe className="h-4 w-4" />
            </Button>
          </TooltipTrigger>
          <TooltipContent>
            {i18n.language.startsWith('zh') ? 'English' : '简体中文'}
          </TooltipContent>
        </Tooltip>

        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={onToggleDark}
            >
              {dark ? (
                <Sun className="h-4 w-4" />
              ) : (
                <Moon className="h-4 w-4" />
              )}
            </Button>
          </TooltipTrigger>
          <TooltipContent>
            {dark ? t('Light mode') : t('Dark mode')}
          </TooltipContent>
        </Tooltip>
      </header>
      <Separator />
    </>
  );
}
