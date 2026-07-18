/**
 * Login dialog for Codex account: API Key or ChatGPT device code.
 *
 * TODO: accountLogin / accountCancelLogin 端点已下线,
 *       待迁移到新 mt-client API。当前渲染只读占位 UI。
 */
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onChanged: () => void;
}

export function AccountLoginDialog({
  open,
  onOpenChange,
  onChanged,
}: Props) {
  const { t } = useTranslation();
  // 占位:旧 accountLogin/accountCancelLogin 已下线,暂不接入实际登录流程
  void onChanged;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('Login to Codex')}</DialogTitle>
          <DialogDescription>
            {t('Codex account login is temporarily unavailable.')}
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t('Close')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
