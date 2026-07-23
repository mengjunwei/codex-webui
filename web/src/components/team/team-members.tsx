/**
 * Team members management panel — 成员列表 + 邀请 + 角色变更 + 转让 + 解散。
 * 所有写操作按 team 级权限守卫(usePermission),平台管理员不绕过。
 */
import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from '@tanstack/react-router';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { UserPlus, Copy, Check, ChevronDown, Crown, TriangleAlert } from 'lucide-react';
import { teamsApi, type MemberDto, type InvitationDto, type Role } from '@/lib/mt-client';
import { useTeamStore } from '@/stores/team-store';
import { useUserStore } from '@/stores/user-store';
import { showSnackbar } from '@/stores/snackbar-store';
import { usePermission } from '@/hooks/use-permission';

interface Props {
  open: boolean;
  onClose: () => void;
}

/** 角色 → badge 变体三态映射:owner=default / admin=secondary / member=outline。 */
function roleBadgeVariant(role: string): 'default' | 'secondary' | 'outline' {
  if (role === 'owner') return 'default';
  if (role === 'admin') return 'secondary';
  return 'outline';
}

export function TeamMembersDialog({ open, onClose }: Props) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { currentTeamId, currentTeam, loadTeams } = useTeamStore();
  const loadMe = useUserStore((s) => s.loadMe);
  const [members, setMembers] = useState<MemberDto[]>([]);
  const [invitation, setInvitation] = useState<InvitationDto | null>(null);
  const [loading, setLoading] = useState(false);
  const [copied, setCopied] = useState(false);
  const [expiresHours, setExpiresHours] = useState('24');
  const [maxUses, setMaxUses] = useState('10');

  // 危险操作弹窗状态
  const [transferOpen, setTransferOpen] = useState(false);
  const [newOwnerId, setNewOwnerId] = useState<string | null>(null);
  const [dissolveOpen, setDissolveOpen] = useState(false);
  const [dissolveConfirm, setDissolveConfirm] = useState('');
  const [mutating, setMutating] = useState(false);

  // 权限守卫:平台管理员不绕过 team 级权限
  const canRoleWrite = usePermission('team:member:role:write');
  const canRemove = usePermission('team:member:remove');
  const canTransfer = usePermission('team:owner:transfer');
  const canDissolve = usePermission('team:dissolve');

  const loadMembers = useCallback(async () => {
    if (!currentTeamId) return;
    try {
      const data = await teamsApi.getMembers(currentTeamId);
      setMembers(data as MemberDto[]);
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    }
  }, [currentTeamId]);

  useEffect(() => {
    if (open) void loadMembers();
  }, [open, loadMembers]);

  const handleInvite = async () => {
    if (!currentTeamId) return;
    setLoading(true);
    try {
      const data = await teamsApi.createInvitation(currentTeamId, {
        expiresAt: Date.now() + parseInt(expiresHours) * 3600 * 1000,
        maxUses: parseInt(maxUses),
      });
      setInvitation(data as InvitationDto);
      showSnackbar(t('Invitation created'), 'success');
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setLoading(false);
    }
  };

  const handleCopy = () => {
    if (invitation?.code) {
      void navigator.clipboard.writeText(invitation.code);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  };

  const handleRemove = async (userId: string) => {
    if (!currentTeamId) return;
    try {
      await teamsApi.removeMember(currentTeamId, userId);
      showSnackbar(t('Member removed'), 'success');
      void loadMembers();
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    }
  };

  // 改成员角色(admin ↔ member;owner 不可改)
  const handleRoleChange = async (userId: string, role: Role) => {
    if (!currentTeamId) return;
    try {
      await teamsApi.setMemberRole(currentTeamId, userId, role);
      showSnackbar(t('Role updated'), 'success');
      void loadMembers();
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    }
  };

  // 转让队长:成功后当前用户失去 owner 权限,需刷新 me
  const handleTransfer = async () => {
    if (!currentTeamId || !newOwnerId) return;
    setMutating(true);
    try {
      await teamsApi.transferOwner(currentTeamId, newOwnerId);
      showSnackbar(t('Ownership transferred'), 'success');
      setTransferOpen(false);
      void loadMe();
      void loadMembers();
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setMutating(false);
    }
  };

  // 解散团队:loadTeams 会检测 currentTeamId 已失效并自动清空 / 重选,再刷 me + 关闭 + 跳走
  const handleDissolve = async () => {
    if (!currentTeamId) return;
    setMutating(true);
    try {
      await teamsApi.dissolve(currentTeamId);
      showSnackbar(t('Team dissolved'), 'success');
      setDissolveOpen(false);
      setDissolveConfirm('');
      await Promise.all([loadTeams(), loadMe()]);
      onClose();
      void navigate({ to: '/' });
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setMutating(false);
    }
  };

  const nonOwnerMembers = members.filter((m) => m.role !== 'owner');
  const dissolveMatch = !!currentTeam && dissolveConfirm.trim() === currentTeam.name;
  const showDangerZone = canTransfer || canDissolve;

  return (
    <>
      <Dialog open={open} onOpenChange={onClose}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('Team members')}</DialogTitle>
          </DialogHeader>

          {/* 危险操作区:转让队长 + 解散团队 */}
          {showDangerZone && (
            <div className="space-y-2 rounded-lg border border-destructive/30 bg-destructive/5 p-3">
              <span className="text-xs font-medium text-destructive">{t('Danger zone')}</span>
              <div className="flex flex-wrap gap-2">
                {canTransfer && (
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={nonOwnerMembers.length === 0}
                    onClick={() => {
                      // 打开时默认预选第一个非 owner 成员(避免 useEffect 内 setState)
                      setNewOwnerId(nonOwnerMembers[0]?.user_id ?? null);
                      setTransferOpen(true);
                    }}
                  >
                    <Crown className="h-4 w-4" />
                    {t('Transfer ownership')}
                  </Button>
                )}
                {canDissolve && (
                  <Button size="sm" variant="destructive" onClick={() => setDissolveOpen(true)}>
                    <TriangleAlert className="h-4 w-4" />
                    {t('Dissolve team')}
                  </Button>
                )}
              </div>
            </div>
          )}

          {/* Members list */}
          <div className="max-h-60 space-y-2 overflow-y-auto">
            {members.length === 0 ? (
              <p className="text-sm text-muted-foreground">{t('No members')}</p>
            ) : (
              members.map((m) => {
                const isOwner = m.role === 'owner';
                return (
                  <div
                    key={m.user_id}
                    className="flex items-center justify-between rounded-lg border p-3"
                  >
                    <div className="flex flex-col">
                      <span className="text-sm font-medium">
                        {m.display_name || m.email || m.user_id.slice(0, 8)}
                      </span>
                      {m.email && <span className="text-xs text-muted-foreground">{m.email}</span>}
                    </div>
                    <div className="flex items-center gap-1.5">
                      <Badge variant={roleBadgeVariant(m.role)}>{m.role}</Badge>
                      {/* 角色变更:仅非 owner 成员,需 team:member:role:write */}
                      {canRoleWrite && !isOwner && (
                        <RolePopover
                          currentRole={m.role}
                          onChange={(role) => void handleRoleChange(m.user_id, role)}
                        />
                      )}
                      {/* 移除成员:owner 不可移除;需 team:member:remove */}
                      {canRemove && !isOwner && (
                        <Button
                          size="sm"
                          variant="destructive"
                          onClick={() => void handleRemove(m.user_id)}
                        >
                          {t('Remove')}
                        </Button>
                      )}
                    </div>
                  </div>
                );
              })
            )}
          </div>

          {/* Invite section */}
          <div className="space-y-3 border-t pt-4">
            <div className="flex items-center gap-2">
              <UserPlus className="h-4 w-4" />
              <span className="text-sm font-medium">{t('Invite members')}</span>
            </div>
            <div className="flex gap-2">
              <Input
                placeholder={t('Expires in hours')}
                value={expiresHours}
                onChange={(e) => setExpiresHours(e.target.value)}
                className="w-32"
                type="number"
              />
              <Input
                placeholder={t('Max uses')}
                value={maxUses}
                onChange={(e) => setMaxUses(e.target.value)}
                className="w-24"
                type="number"
              />
              <Button onClick={() => void handleInvite()} disabled={loading}>
                {t('Generate code')}
              </Button>
            </div>
            {invitation && (
              <div className="flex items-center gap-2 rounded-lg bg-muted p-3">
                <code className="flex-1 font-mono text-sm">{invitation.code}</code>
                <Button size="sm" variant="ghost" onClick={handleCopy}>
                  {copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
                </Button>
              </div>
            )}
          </div>
        </DialogContent>
      </Dialog>

      {/* 转让队长二次确认 */}
      <AlertDialog
        open={transferOpen}
        onOpenChange={(o) => !o && !mutating && setTransferOpen(false)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('Transfer ownership')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t('Pick a new owner. You will lose owner privileges.')}
            </AlertDialogDescription>
          </AlertDialogHeader>
          {nonOwnerMembers.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              {t('No other members to transfer to.')}
            </p>
          ) : (
            <div className="max-h-48 space-y-1 overflow-y-auto">
              {nonOwnerMembers.map((m) => (
                <button
                  key={m.user_id}
                  type="button"
                  onClick={() => setNewOwnerId(m.user_id)}
                  className={`flex w-full items-center justify-between rounded-md border px-3 py-2 text-sm hover:bg-accent ${
                    newOwnerId === m.user_id ? 'border-primary bg-accent' : 'border-border'
                  }`}
                >
                  <span>{m.display_name || m.email || m.user_id.slice(0, 8)}</span>
                  {newOwnerId === m.user_id && <Check className="h-4 w-4" />}
                </button>
              ))}
            </div>
          )}
          <AlertDialogFooter>
            <AlertDialogCancel disabled={mutating}>{t('Cancel')}</AlertDialogCancel>
            <AlertDialogAction
              disabled={!newOwnerId || mutating}
              onClick={(e) => {
                e.preventDefault();
                void handleTransfer();
              }}
            >
              {t('Confirm transfer')}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      {/* 解散团队二次确认:必须输入团队名才能提交 */}
      <AlertDialog
        open={dissolveOpen}
        onOpenChange={(o) => !o && !mutating && setDissolveOpen(false)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('Dissolve team')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t('This cannot be undone. Type the team name "{{name}}" to confirm.', {
                name: currentTeam?.name ?? '',
              })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <Input
            placeholder={currentTeam?.name ?? ''}
            value={dissolveConfirm}
            onChange={(e) => setDissolveConfirm(e.target.value)}
            disabled={mutating}
          />
          <AlertDialogFooter>
            <AlertDialogCancel disabled={mutating}>{t('Cancel')}</AlertDialogCancel>
            <AlertDialogAction
              disabled={!dissolveMatch || mutating}
              onClick={(e) => {
                e.preventDefault();
                void handleDissolve();
              }}
            >
              {t('Dissolve team')}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

/** 角色切换小弹窗:admin ↔ member 二选一(owner 不展示)。 */
function RolePopover({
  currentRole,
  onChange,
}: {
  currentRole: string;
  onChange: (role: Role) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const options: Role[] = ['admin', 'member'];
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="icon-xs"
          className="text-muted-foreground"
          title={t('Change role')}
        >
          <ChevronDown className="h-3 w-3" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-32 p-1">
        {options.map((r) => (
          <button
            key={r}
            type="button"
            onClick={() => {
              if (r !== currentRole) onChange(r);
              setOpen(false);
            }}
            className="flex w-full items-center justify-between rounded-md px-3 py-1.5 text-sm hover:bg-accent"
          >
            <span>{r}</span>
            {r === currentRole && <Check className="h-3 w-3" />}
          </button>
        ))}
      </PopoverContent>
    </Popover>
  );
}
