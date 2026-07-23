/** Security policy selector for the current thread — per-session, not global.
 *
 *  Codex approval policy and sandbox mode are per-thread (turn/start 参数透传)。
 *  用户选中后存 timeline-store per-thread state，发消息时带入 turn/start body。
 *  null = codex 默认（approvalPolicy=on-request, sandboxMode=workspace-write）。
 */
import { ShieldCheck } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useTimelineStore } from '@/stores/timeline-store';
import { cn } from '@/lib/utils';

/** codex 原生 approval policy 枚举 */
const APPROVAL_OPTIONS = ['on-failure', 'on-request', 'never', 'untrusted'] as const;
/** codex 原生 sandbox mode 枚举 */
const SANDBOX_OPTIONS = ['read-only', 'workspace-write', 'danger-full-access'] as const;

export function SecurityPolicyBadge() {
  const { t } = useTranslation();
  const approvalPolicy = useTimelineStore((s) => s.approvalPolicy);
  const sandboxMode = useTimelineStore((s) => s.sandboxMode);
  const setApprovalPolicy = useTimelineStore((s) => s.setApprovalPolicy);
  const setSandboxMode = useTimelineStore((s) => s.setSandboxMode);

  // codex 默认值（null = 用 codex 内置默认）
  const currentApproval = approvalPolicy ?? 'on-request';
  const currentSandbox = sandboxMode ?? 'workspace-write';

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 gap-1 rounded-lg px-2 text-xs"
          title={t('Security policy')}
        >
          <ShieldCheck className="h-3.5 w-3.5" />
          <span className="hidden sm:inline">
            {t(currentSandbox)}
            <span className="mx-1 text-muted-foreground">·</span>
            {t(currentApproval)}
          </span>
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" side="top" className="w-64 space-y-3 p-3 text-sm">
        <div className="space-y-1">
          <p className="text-xs font-medium text-foreground">{t('Approval Policy')}</p>
          {APPROVAL_OPTIONS.map((opt) => (
            <button
              key={opt}
              type="button"
              className={cn(
                'w-full rounded px-2 py-1 text-left text-xs',
                opt === currentApproval
                  ? 'bg-accent text-accent-foreground'
                  : 'text-muted-foreground hover:bg-accent/50',
                opt === 'never' && 'text-destructive',
              )}
              onClick={() => setApprovalPolicy(opt === 'on-request' ? null : opt)}
            >
              {t(opt)}
              {opt === 'never' && <span className="ml-1 text-[10px]">({t('auto-approve')})</span>}
            </button>
          ))}
        </div>
        <div className="space-y-1">
          <p className="text-xs font-medium text-foreground">{t('Sandbox Mode')}</p>
          {SANDBOX_OPTIONS.map((opt) => (
            <button
              key={opt}
              type="button"
              className={cn(
                'w-full rounded px-2 py-1 text-left text-xs',
                opt === currentSandbox
                  ? 'bg-accent text-accent-foreground'
                  : 'text-muted-foreground hover:bg-accent/50',
                opt === 'danger-full-access' && 'text-destructive',
              )}
              onClick={() => setSandboxMode(opt === 'workspace-write' ? null : opt)}
            >
              {t(opt)}
              {opt === 'danger-full-access' && <span className="ml-1 text-[10px]">({t('risky')})</span>}
            </button>
          ))}
        </div>
      </PopoverContent>
    </Popover>
  );
}
