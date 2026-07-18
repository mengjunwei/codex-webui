/** Settings tab for Codex account/provider auth state and login flows.
 *  TODO: accountReadAccount / accountLogout / accountReadRateLimits 等端点已下线,
 *        当前仅渲染占位 UI,待迁移到新 mt-client API 后重新接入。
 */
import { useState } from 'react';
import { KeyRound } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { AccountLoginDialog } from './account-login-dialog';

export function AccountSettings() {
  const { t } = useTranslation();
  const [loginOpen, setLoginOpen] = useState(false);

  return (
    <section className="space-y-4">
      <div className="space-y-1">
        <h2 className="text-sm font-medium text-muted-foreground">
          {t('Codex Account')}
        </h2>
        <p className="text-xs text-muted-foreground">
          {t('Codex account integration is temporarily disabled.')}
        </p>
      </div>

      <div className="flex flex-wrap gap-2">
        <Button size="sm" className="h-8" disabled>
          <KeyRound className="h-3.5 w-3.5" />
          {t('Login with ChatGPT or API Key')}
        </Button>
      </div>

      <AccountLoginDialog open={loginOpen} onOpenChange={setLoginOpen} onChanged={() => undefined} />
    </section>
  );
}
