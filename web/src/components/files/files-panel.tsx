/**
 * Combined file browser panel: tree sidebar + file viewer.
 * Syncs root directory from the active thread's cwd.
 */
import { useEffect } from 'react';
import { FileTree } from './file-tree';
import { FileViewer } from './file-viewer';
import { useFilesStore } from '@/stores/files-store';
import { useTimelineStore } from '@/stores/timeline-store';

export function FilesPanel() {
  const selectedFile = useFilesStore((s) => s.selectedFile);
  const setRootDir = useFilesStore((s) => s.setRootDir);
  const threadCwd = useTimelineStore((s) => s.threadCwd);

  // Sync file tree root with current thread's cwd
  useEffect(() => {
    void setRootDir(threadCwd);
  }, [threadCwd, setRootDir]);

  return (
    <div className="flex min-h-0 flex-1">
      <div className="flex w-56 shrink-0 flex-col border-r border-border bg-muted/20">
        <div className="px-3 py-2 text-xs font-medium text-muted-foreground">
          Explorer
        </div>
        <FileTree />
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        {selectedFile ? (
          <FileViewer />
        ) : (
          <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
            Select a file to view
          </div>
        )}
      </div>
    </div>
  );
}
