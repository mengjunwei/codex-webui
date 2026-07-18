/**
 * Team members management panel — list members + invite via code.
 */
import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { UserPlus, Copy, Check } from 'lucide-react';
import { teamsApi, type MemberDto, type InvitationDto } from '@/lib/mt-client';
import { useTeamStore } from '@/stores/team-store';
import { showSnackbar } from '@/stores/snackbar-store';

interface Props {
  open: boolean;
  onClose: () => void;
}

export function TeamMembersDialog({ open, onClose }: Props) {
  const { t } = useTranslation();
  const { currentTeamId } = useTeamStore();
  const [members, setMembers] = useState<MemberDto[]>([]);
  const [invitation, setInvitation] = useState<InvitationDto | null>(null);
  const [loading, setLoading] = useState(false);
  const [copied, setCopied] = useState(false);
  const [expiresHours, setExpiresHours] = useState('24');
  const [maxUses, setMaxUses] = useState('10');

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

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('Team members')}</DialogTitle>
        </DialogHeader>

        {/* Members list */}
        <div className="space-y-2 max-h-60 overflow-y-auto">
          {members.length === 0 ? (
            <p className="text-sm text-muted-foreground">{t('No members')}</p>
          ) : (
            members.map((m) => (
              <div key={m.user_id} className="flex items-center justify-between rounded-lg border p-3">
                <div className="flex flex-col">
                  <span className="text-sm font-medium">{m.display_name || m.email || m.user_id.slice(0, 8)}</span>
                  {m.email && <span className="text-xs text-muted-foreground">{m.email}</span>}
                </div>
                <div className="flex items-center gap-2">
                  <Badge variant={m.role === 'owner' ? 'default' : 'secondary'}>{m.role}</Badge>
                  {m.role !== 'owner' && (
                    <Button size="sm" variant="destructive" onClick={() => void handleRemove(m.user_id)}>
                      {t('Remove')}
                    </Button>
                  )}
                </div>
              </div>
            ))
          )}
        </div>

        {/* Invite section */}
        <div className="border-t pt-4 space-y-3">
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
              <code className="flex-1 text-sm font-mono">{invitation.code}</code>
              <Button size="sm" variant="ghost" onClick={handleCopy}>
                {copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
              </Button>
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
