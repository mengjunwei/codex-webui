/**
 * 平台管理员面板:展示当前管理员(is_platform_admin=true 的用户)+ 配置方式提示。
 *
 * 范围说明:
 * - 后端平台管理员的增删 API(POST/DELETE 管理员)在批次3a 未实现,
 *   当前仅支持通过 config.toml [security] admin_emails 在启动时 bootstrap。
 * - 因此本 panel 做只读展示 + 引导,不提供增删 UI;后续后端补 API 后可在此扩展。
 */
import { useTranslation } from 'react-i18next';
import { ShieldCheck } from 'lucide-react';
import { useUserStore } from '@/stores/user-store';

export function PlatformAdminPanel() {
  const { t } = useTranslation();
  const me = useUserStore((s) => s.me);

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <h2 className="flex items-center gap-2 text-lg font-semibold">
          <ShieldCheck className="h-5 w-5" />
          {t('平台管理')}
        </h2>
        <p className="text-sm text-muted-foreground">
          {t(
            '平台管理员可修改全局配置、读取全局日志。当前管理员通过 config.toml 的 [security] admin_emails 在启动时 bootstrap。',
          )}
        </p>
      </div>

      <div className="rounded-lg border border-border bg-card/50 px-4 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium">
            {me?.user.email ?? t('未知')}
          </span>
          <span className="text-xs text-muted-foreground">
            {t('(你,平台管理员)')}
          </span>
        </div>
        {me?.user.display_name && (
          <p className="mt-1 text-xs text-muted-foreground">
            {me.user.display_name}
          </p>
        )}
      </div>

      <div className="rounded-lg border border-border bg-card/50 px-4 py-3">
        <h3 className="text-sm font-medium">{t('管理员管理')}</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          {t(
            '提示:增删管理员的 API 尚未实现,当前通过配置文件管理。后续后端补齐 API 后,这里会提供增删 UI。',
          )}
        </p>
      </div>

      <div className="rounded-lg border border-border bg-card/50 px-4 py-3">
        <h3 className="text-sm font-medium">{t('全局能力')}</h3>
        <ul className="mt-2 space-y-1 text-xs text-muted-foreground">
          <li>- {t('修改全局运行时配置(general/security/files/terminal 各 tab 的 Runtime Settings)')}</li>
          <li>- {t('读取全局日志与审计')}</li>
          <li>- {t('公共工作区文件读写')}</li>
        </ul>
      </div>
    </div>
  );
}
