/**
 * Team settings panel — view team info + API keys.
 */
import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Copy, Key } from 'lucide-react';
import { teamsApi, type TeamDto, type ApiKeyResp } from '@/lib/mt-client';
import { useTeamStore } from '@/stores/team-store';
import { showSnackbar } from '@/stores/snackbar-store';

interface Props {
  open: boolean;
  onClose: () => void;
}

export function TeamSettingsDialog({ open, onClose }: Props) {
  const { t } = useTranslation();
  const { currentTeamId } = useTeamStore();
  const [team, setTeam] = useState<TeamDto | null>(null);
  const [apiKeys, setApiKeys] = useState<ApiKeyResp[]>([]);
  const [newKey, setNewKey] = useState('');
  const [provider, setProvider] = useState('openai');
  const [loading, setLoading] = useState(false);

  const loadData = useCallback(async () => {
    if (!currentTeamId) return;
    try {
      const keys = await teamsApi.listApiKeys(currentTeamId);
      setApiKeys(keys as ApiKeyResp[]);
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    }
  }, [currentTeamId]);

  useEffect(() => {
    if (open) void loadData();
  }, [open, loadData]);

  const handleSetKey = async () => {
    if (!currentTeamId || !newKey.trim()) return;
    setLoading(true);
    try {
      await teamsApi.setApiKey(currentTeamId, { key: newKey.trim(), provider });
      setNewKey('');
      showSnackbar(t('API key saved'), 'success');
      void loadData();
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setLoading(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('Team settings')}</DialogTitle>
        </DialogHeader>

        {/* Team info */}
        {team && (
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-sm text-muted-foreground">{t('Team name')}</span>
              <span className="text-sm font-medium">{team.name}</span>
            </div>
            <div className="flex items-center justify-between">
              <span className="text-sm text-muted-foreground">{t('Team ID')}</span>
              <span className="text-xs font-mono">{team.id}</span>
            </div>
          </div>
        )}

        {/* API keys */}
        <div className="border-t pt-4 space-y-3">
          <div className="flex items-center gap-2">
            <Key className="h-4 w-4" />
            <span className="text-sm font-medium">{t('API keys')}</span>
          </div>
          {apiKeys.length === 0 ? (
            <p className="text-sm text-muted-foreground">{t('No API keys set')}</p>
          ) : (
            apiKeys.map((k) => (
              <div key={k.id} className="flex items-center justify-between rounded-lg border p-3">
                <div className="flex flex-col">
                  <span className="text-sm font-medium">{k.key_hint}</span>
                  <span className="text-xs text-muted-foreground">{k.provider} · {k.is_active ? t('Active') : t('Inactive')}</span>
                </div>
                <Badge variant={k.is_active ? 'default' : 'secondary'}>{k.is_active ? t('Active') : t('Inactive')}</Badge>
              </div>
            ))
          )}
          <div className="flex gap-2">
            <Input
              placeholder={t('Enter API key')}
              value={newKey}
              onChange={(e) => setNewKey(e.target.value)}
              type="password"
              className="flex-1"
            />
            <Button onClick={() => void handleSetKey()} disabled={loading || !newKey.trim()}>
              {t('Save')}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
