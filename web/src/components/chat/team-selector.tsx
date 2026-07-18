/**
 * Team selector — shows current team + switch/create/join.
 * Uses popover + dialog (dropdown-menu not available).
 */
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Check, ChevronDown, Plus, UserPlus } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useTeamStore } from '@/stores/team-store';
import { showSnackbar } from '@/stores/snackbar-store';

export function TeamSelector() {
  const { t } = useTranslation();
  const { teams, currentTeam, currentTeamId, setCurrentTeam, createTeam, joinTeam } = useTeamStore();
  const [open, setOpen] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const [joinOpen, setJoinOpen] = useState(false);
  const [teamName, setTeamName] = useState('');
  const [inviteCode, setInviteCode] = useState('');

  if (!currentTeamId || teams.length === 0) return null;

  return (
    <>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button variant="ghost" className="w-full justify-between px-3 py-1.5 text-sm font-medium">
            <span className="truncate">{currentTeam?.name ?? t('Select team')}</span>
            <ChevronDown className="h-4 w-4 shrink-0 opacity-50" />
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" className="w-56 p-1">
          {teams.map((team) => (
            <button
              key={team.id}
              onClick={() => { setCurrentTeam(team.id); setOpen(false); }}
              className="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm hover:bg-accent"
            >
              {team.id === currentTeamId && <Check className="h-4 w-4" />}
              <span className={team.id === currentTeamId ? 'font-semibold' : ''}>{team.name}</span>
            </button>
          ))}
          <hr className="my-1 border-border" />
          <button
            onClick={() => { setOpen(false); setCreateOpen(true); }}
            className="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm hover:bg-accent"
          >
            <Plus className="h-4 w-4" />
            {t('Create team')}
          </button>
          <button
            onClick={() => { setOpen(false); setJoinOpen(true); }}
            className="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm hover:bg-accent"
          >
            <UserPlus className="h-4 w-4" />
            {t('Join with code')}
          </button>
        </PopoverContent>
      </Popover>

      <Dialog open={createOpen} onOpenChange={setCreateOpen}>
        <DialogContent>
          <DialogHeader><DialogTitle>{t('Create new team')}</DialogTitle></DialogHeader>
          <Input placeholder={t('Team name')} value={teamName} onChange={(e) => setTeamName(e.target.value)} />
          <Button
            disabled={!teamName.trim()}
            onClick={async () => {
              try {
                await createTeam(teamName.trim());
                setTeamName('');
                setCreateOpen(false);
                showSnackbar(t('Team created'), 'success');
              } catch (e: unknown) {
                showSnackbar(String(e), 'error');
              }
            }}
          >{t('Create')}</Button>
        </DialogContent>
      </Dialog>

      <Dialog open={joinOpen} onOpenChange={setJoinOpen}>
        <DialogContent>
          <DialogHeader><DialogTitle>{t('Join team')}</DialogTitle></DialogHeader>
          <Input placeholder={t('Invitation code')} value={inviteCode} onChange={(e) => setInviteCode(e.target.value)} />
          <Button
            disabled={!inviteCode.trim()}
            onClick={async () => {
              try {
                await joinTeam(inviteCode.trim());
                setInviteCode('');
                setJoinOpen(false);
                showSnackbar(t('Joined team'), 'success');
              } catch (e: unknown) {
                showSnackbar(String(e), 'error');
              }
            }}
          >{t('Join')}</Button>
        </DialogContent>
      </Dialog>
    </>
  );
}
