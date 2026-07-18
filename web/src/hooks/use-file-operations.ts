/**
 * Centralizes file operation mutations, query invalidation, and selection sync.
 * All file management UI surfaces delegate to this hook.
 *
 * Uses the generated SDK for file operations (filesReadFile, filesWriteFile, etc.)
 * which are available in the current OpenAPI spec.
 */
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  createFile as sdkCreateFile,
  createDirectory as sdkCreateDirectory,
  renamePath as sdkRenamePath,
  copyPath as sdkCopyPath,
  movePath as sdkMovePath,
  deletePath as sdkDeletePath,
} from '@/generated/api';
import { showSnackbar } from '@/stores/snackbar-store';
import { getApiErrorMessage } from '@/lib/api-error';

/** Invalidates the tree query for a specific directory. */
function treeKey(dir: string) {
  return ['files', 'readTree', { query: { root: dir } }];
}

/** Extracts parent directory from a path. */
function parentDir(filePath: string): string {
  return filePath.substring(0, filePath.lastIndexOf('/')) || '/';
}

export function useFileOperations() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();

  const refreshTree = (dir: string) => {
    void queryClient.invalidateQueries({ queryKey: treeKey(dir) });
  };

  const createFile = useMutation({
    mutationFn: (vars: { path: string; content?: string }) =>
      sdkCreateFile({ body: { path: vars.path, content: vars.content } }),
    onSuccess: (_res, vars) => {
      refreshTree(parentDir(vars.path));
      showSnackbar(t('File created'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const createDirectory = useMutation({
    mutationFn: (vars: { path: string }) =>
      sdkCreateDirectory({ body: { path: vars.path } }),
    onSuccess: (_res, vars) => {
      refreshTree(parentDir(vars.path));
      showSnackbar(t('Directory created'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const deletePath = useMutation({
    mutationFn: (vars: { path: string }) =>
      sdkDeletePath({ query: { path: vars.path } }),
    onSuccess: (_res, vars) => {
      refreshTree(parentDir(vars.path));
      showSnackbar(t('Deleted'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const renamePath = useMutation({
    mutationFn: (vars: { sourcePath: string; destPath: string }) =>
      sdkRenamePath({ body: { sourcePath: vars.sourcePath, destPath: vars.destPath } }),
    onSuccess: (_res, vars) => {
      refreshTree(parentDir(vars.sourcePath));
      refreshTree(parentDir(vars.destPath));
      showSnackbar(t('Renamed'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const copyPath = useMutation({
    mutationFn: (vars: { sourcePath: string; destPath: string }) =>
      sdkCopyPath({ body: { sourcePath: vars.sourcePath, destPath: vars.destPath } }),
    onSuccess: (_res, vars) => {
      refreshTree(parentDir(vars.destPath));
      showSnackbar(t('Copied'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const movePath = useMutation({
    mutationFn: (vars: { sourcePath: string; destPath: string }) =>
      sdkMovePath({ body: { sourcePath: vars.sourcePath, destPath: vars.destPath } }),
    onSuccess: (_res, vars) => {
      refreshTree(parentDir(vars.sourcePath));
      refreshTree(parentDir(vars.destPath));
      showSnackbar(t('Moved'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  return {
    createFile,
    createDirectory,
    deletePath,
    renamePath,
    copyPath,
    movePath,
    refreshTree,
  };
}
