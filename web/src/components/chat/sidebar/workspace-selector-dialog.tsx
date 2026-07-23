/**
 * Workspace selector dialog — 选择创建会话的工作区类型。
 *
 * 交互逻辑:
 * - 有多个团队 → 选择个人/团队(shared) workspace
 * - 只有一个团队 → 直接显示"个人 workspace"和"团队 workspace"两个选项
 * - 无团队 → 提示创建团队
 *
 * 后端 codex 工作区路径:
 * - 个人: codex_home/users/{user_id}/personal/
 * - 团队: codex_home/teams/{team_id}/shared/
 */
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Users, User } from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import { useTeamStore } from '@/stores/team-store';

interface Props {
  open: boolean;
  onClose: () => void;
  onSelect: (workspace: { type: 'personal' | 'team'; teamId?: string; cwd?: string }) => void;
}

export function WorkspaceSelectorDialog({ open, onClose, onSelect }: Props) {
  const { t } = useTranslation();
  const { teams, currentTeamId } = useTeamStore();
  const [selected, setSelected] = useState<'personal' | 'team' | null>(null);
  const [selectedTeamId, setSelectedTeamId] = useState<string | null>(null);

  const handleConfirm = () => {
    if (selected === 'personal') {
      onSelect({ type: 'personal' });
      onClose();
    } else if (selected === 'team') {
      const teamId = selectedTeamId ?? currentTeamId ?? teams[0]?.id;
      if (teamId) {
        onSelect({ type: 'team', teamId });
        onClose();
      }
    }
  };

  // 选中个人 workspace 时立即确认
  const handleSelectPersonal = () => {
    onSelect({ type: 'personal' });
    onClose();
  };

  return (
    <Dialog open={open} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="max-w-sm">
        <DialogHeader>
          <DialogTitle>{t('Select workspace')}</DialogTitle>
        </DialogHeader>

        <div className="space-y-3 py-2">
          {/* Personal workspace - 点击直接创建 */}
          <button
            type="button"
            onClick={handleSelectPersonal}
            className={cn(
              'flex w-full items-center gap-3 rounded-xl border p-4 text-left transition-colors',
              'border-border hover:border-primary/50 hover:bg-accent/30',
            )}
          >
            <User className="h-5 w-5 shrink-0 text-muted-foreground" />
            <div className="flex-1">
              <div className="text-sm font-medium">{t('Personal workspace')}</div>
              <div className="text-xs text-muted-foreground">{t('Private notes and drafts')}</div>
            </div>
          </button>

          {/* Team workspace */}
          <button
            type="button"
            onClick={() => {
              setSelected('team');
              if (!selectedTeamId && currentTeamId) setSelectedTeamId(currentTeamId);
            }}
            className={cn(
              'flex w-full items-center gap-3 rounded-xl border p-4 text-left transition-colors',
              selected === 'team'
                ? 'border-primary bg-primary/5'
                : 'border-border hover:border-primary/50 hover:bg-accent/30',
            )}
          >
            <Users className="h-5 w-5 shrink-0 text-muted-foreground" />
            <div className="flex-1">
              <div className="text-sm font-medium">{t('Team workspace')}</div>
              <div className="text-xs text-muted-foreground">{t('Shared with team members')}</div>
            </div>
          </button>

          {/* Team selector (only visible when team is selected) */}
          {selected === 'team' && teams.length > 0 && (
            <div className="ml-8 flex flex-wrap gap-1.5">
              {teams.map((team) => (
                <Badge
                  key={team.id}
                  variant={team.id === (selectedTeamId ?? currentTeamId) ? 'default' : 'outline'}
                  className="cursor-pointer"
                  onClick={() => setSelectedTeamId(team.id)}
                >
                  {team.name}
                </Badge>
              ))}
            </div>
          )}

          {selected === 'team' && teams.length === 0 && (
            <p className="text-xs text-muted-foreground text-center py-2">
              {t('No teams available. Create a team first.')}
            </p>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" size="sm" onClick={onClose}>
            {t('Cancel')}
          </Button>
          {selected === 'team' && (
            <Button
              size="sm"
              disabled={!selectedTeamId && !currentTeamId && teams.length === 0}
              onClick={handleConfirm}
            >
              {t('Confirm')}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
