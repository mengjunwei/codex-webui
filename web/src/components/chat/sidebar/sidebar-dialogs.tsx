/** Rename and confirmation dialogs for the thread sidebar. */
import { Loader2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/input';
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
import type { ConfirmAction } from './sidebar-types';

interface RenameDialogProps {
  open: boolean;
  pending: boolean;
  value: string;
  onChange: (value: string) => void;
  onSave: () => void;
  onClose: () => void;
}

/** Dialog for renaming a thread. */
export function RenameDialog({ open, pending, value, onChange, onSave, onClose }: RenameDialogProps) {
  const { t } = useTranslation();
  return (
    <AlertDialog open={open} onOpenChange={(o) => !o && !pending && onClose()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t('Rename thread')}</AlertDialogTitle>
          <AlertDialogDescription>{t('Enter a non-empty thread name.')}</AlertDialogDescription>
        </AlertDialogHeader>
        <Input
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter' && value.trim() && !pending) onSave(); }}
          disabled={pending}
          autoFocus
        />
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>{t('Cancel')}</AlertDialogCancel>
          <AlertDialogAction disabled={!value.trim() || pending} onClick={onSave}>
            {pending ? <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" /> : null}
            {t('Save')}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

interface ConfirmDialogProps {
  action: ConfirmAction;
  pending: boolean;
  onConfirm: () => void;
  onClose: () => void;
}

/** Dialog for confirming archive / compact / delete actions. */
export function ConfirmDialog({ action, pending, onConfirm, onClose }: ConfirmDialogProps) {
  const { t } = useTranslation();
  const title =
    action?.type === 'compact' ? t('Compact this thread?')
    : action?.type === 'delete' ? t('Delete this thread?')
    : t('Archive this thread?');
  const desc =
    action?.type === 'compact'
      ? t('Compaction permanently compresses context and cannot be undone.')
      : action?.type === 'delete'
        ? t('Deletion permanently removes the thread and all its turns. This cannot be undone.')
        : t('Archived threads move to the read-only archive group.');
  const confirmLabel =
    action?.type === 'compact' ? t('Compact')
    : action?.type === 'delete' ? t('Delete')
    : t('Archive');
  const isDestructive = action?.type === 'delete';
  return (
    <AlertDialog open={action !== null} onOpenChange={(o) => !o && !pending && onClose()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{desc}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>{t('Cancel')}</AlertDialogCancel>
          <AlertDialogAction
            disabled={pending}
            onClick={onConfirm}
            className={isDestructive ? 'bg-destructive text-destructive-foreground hover:bg-destructive/90' : undefined}
          >
            {pending ? <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" /> : null}
            {confirmLabel}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
