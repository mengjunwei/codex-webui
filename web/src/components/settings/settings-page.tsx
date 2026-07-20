/**
 * Settings page shell with tab routing to category sub-components.
 */
import { useState } from 'react';
import { useNavigate } from '@tanstack/react-router';
import { ArrowLeft } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Separator } from '@/components/ui/separator';
import { useThemeStore } from '@/stores/theme-store';
import { useTimelineStore } from '@/stores/timeline-store';
import { useIsPlatformAdmin } from '@/hooks/use-permission';
import { clearApiToken } from '@/auth-token';
import { resetSocket } from '@/socket';
import { sectionLabel } from './setting-helpers';
import { GeneralSettings } from './general-settings';
import { AccountSettings } from './account/account-settings';
import { PlatformAdminPanel } from './platform-admin-panel';
import { TeamMembersDialog } from '../team/team-members';
import { TeamSettingsDialog } from '../team/team-settings';

const SECTIONS = ['general', 'account', 'team', 'platform'] as const;

type SettingsSection = (typeof SECTIONS)[number];

export function SettingsPage() {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const dark = useThemeStore((s) => s.dark);
  const toggleDark = useThemeStore((s) => s.toggleDark);
  const threadId = useTimelineStore((s) => s.threadId);
  const isPlatformAdmin = useIsPlatformAdmin();
  const [section, setSection] = useState<SettingsSection>('general');
  // general/platform 仅平台管理员;当前 section 对用户不可见时,落到首个可见 tab。
  const visibleSections = SECTIONS.filter(
    (s) => !((s === 'general' || s === 'platform') && !isPlatformAdmin),
  );
  const active = visibleSections.includes(section) ? section : visibleSections[0];
  const [membersOpen, setMembersOpen] = useState(false);
  const [teamSettingsOpen, setTeamSettingsOpen] = useState(false);

  const navigateBack = () => {
    if (threadId) {
      void navigate({ to: '/t/$threadId', params: { threadId } });
    } else {
      void navigate({ to: '/' });
    }
  };

  const handleLogout = () => {
    clearApiToken();
    resetSocket();
    void navigate({ to: '/login', search: { redirect: '/' } });
  };

  return (
    <div className="flex flex-1 flex-col overflow-auto">
      <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-4 sm:px-6 sm:py-8">
        {/* Header */}
        <div className="flex items-center gap-3">
          <Button
            size="icon"
            variant="ghost"
            className="h-8 w-8"
            onClick={navigateBack}
          >
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <h1 className="text-xl font-semibold">{t('Settings')}</h1>
        </div>

        <div className="flex flex-wrap gap-2">
          {SECTIONS.map((s) => {
            // general(含全局运行时配置)+ platform(管理员管理) 仅平台管理员可见。
            if ((s === 'general' || s === 'platform') && !isPlatformAdmin) return null;
            return (
              <Button
                key={s}
                variant={active === s ? 'default' : 'outline'}
                size="sm"
                onClick={() => setSection(s)}
              >
                {t(sectionLabel(s))}
              </Button>
            );
          })}
        </div>

        <Separator />

        {active === 'general' && (
          <GeneralSettings
            dark={dark}
            toggleDark={toggleDark}
            language={i18n.language}
            changeLanguage={(lang) => void i18n.changeLanguage(lang)}
            onLogout={handleLogout}
          />
        )}
        {active === 'account' && <AccountSettings />}
        {/* Codex / terminal / files / security tab 临时下线:多租户迁移后这些是全局配置,
            显示会误导用户以为可 per-team 配置;且 files/terminal 入口与团队 workspace 语义重叠。
            per-team 配置接口待后续实现后再恢复。 */}
        {active === 'team' && (
          <div className="space-y-4">
            <h2 className="text-lg font-semibold">{t('团队管理')}</h2>
            <p className="text-sm text-muted-foreground">
              {t('管理团队成员、邀请和团队设置。')}
            </p>
            <div className="flex gap-3">
              <Button size="sm" onClick={() => setMembersOpen(true)}>
                {t('成员管理')}
              </Button>
              <Button size="sm" variant="outline" onClick={() => setTeamSettingsOpen(true)}>
                {t('团队设置')}
              </Button>
            </div>
            <TeamMembersDialog open={membersOpen} onClose={() => setMembersOpen(false)} />
            <TeamSettingsDialog open={teamSettingsOpen} onClose={() => setTeamSettingsOpen(false)} />
          </div>
        )}
        {active === 'platform' && isPlatformAdmin && <PlatformAdminPanel />}
      </div>
    </div>
  );
}
