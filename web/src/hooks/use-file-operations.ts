/**
 * Centralizes file operation mutations, query invalidation, and selection sync.
 * All file management UI surfaces delegate to this hook.
 *
 * TODO: filesCreateFileMutation / filesCreateDirectoryMutation /
 *       filesRenamePathMutation / filesCopyPathMutation / filesMovePathMutation /
 *       filesDeletePathMutation / filesReadTreeQueryKey 等旧 SDK 函数已下线,
 *       待迁移到新 mt-client API。当前为占位 hook,调用不会真正执行操作。
 */
import { useTranslation } from 'react-i18next';
import { showSnackbar } from '@/stores/snackbar-store';

export function useFileOperations() {
  const { t } = useTranslation();

  // 占位 mutations — 调用方代码不变,仅在没有实际实现时显示 toast
  const noopMutation = {
    mutate: (_args: unknown) => {
      showSnackbar(t('File operations are temporarily disabled'), 'info');
    },
    isPending: false,
  } as const;

  const noopAsync = async (_filePath: string) => undefined;

  const noopRefresh = (_entryPath: string, _type: 'file' | 'directory') => undefined;

  return {
    createFile: noopMutation,
    createDirectory: noopMutation,
    renamePath: noopMutation,
    copyPath: noopMutation,
    movePath: noopMutation,
    deletePath: noopMutation,
    uploadFiles: noopMutation,
    downloadFile: noopAsync,
    refresh: noopRefresh,
  };
}
