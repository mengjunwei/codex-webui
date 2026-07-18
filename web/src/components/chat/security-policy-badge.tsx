/** Interactive security policy selector for the chat input area.
 *  TODO: codexStatusGetStatusOptions / codexStatusUpdateApprovalPolicyMutation / codexStatusUpdateSandboxModeMutation 已下线,
 *        待迁移到新 mt-client API。当前组件返回只读占位。
 */
import { ShieldCheck } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';

/** Displays and allows switching approval policy and sandbox mode. */
export function SecurityPolicyBadge() {
  const { t } = useTranslation();

  // TODO: 迁移到新 mt-client API — 当前返回只读占位
  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 gap-1 rounded-lg px-2 text-xs"
          title={t('Security policy')}
          disabled
        >
          <ShieldCheck className="h-3.5 w-3.5" />
          <span className="hidden sm:inline">
            {t('workspace-write')}
            <span className="mx-1 text-muted-foreground">·</span>
            {t('on-failure')}
          </span>
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        side="top"
        className="w-60 space-y-3 p-3 text-sm"
      >
        <p className="text-xs text-muted-foreground">
          {t('Security policy configuration is temporarily unavailable.')}
        </p>
      </PopoverContent>
    </Popover>
  );
}
