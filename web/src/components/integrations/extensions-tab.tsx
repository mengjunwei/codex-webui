/**
 * Extensions 标签页 — 集群扩展分发管理(skill / plugin / mcp)。
 *
 * - 列表:GET /api/mt/extensions(登录可读),每项展示 kind Badge + name + enabled + 删除。
 * - 添加(仅 platform admin):POST /api/mt/extensions,按 kind 分三态表单:
 *   - skill:目录上传,FileReader 读 ArrayBuffer → 分块 base64(防大文件 call stack 溢出)。
 *   - plugin:marketplace 安装(默认 openai-api-curated)。
 *   - mcp:内联配置段合并(段内键值,无段头)。
 *
 * 删除走 AlertDialog 二次确认;空态提示;上传/删除中禁用按钮。
 * 仿 team-members.tsx 骨架。
 */
import { useState, useEffect, useCallback, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Badge } from '@/components/ui/badge';
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
import { Trash2, Upload, Puzzle, Package, Wrench } from 'lucide-react';
import {
  extensionsApi,
  type ExtensionListItem,
  type UploadExtensionBody,
  type UploadFile,
} from '@/lib/mt-client';
import { showSnackbar } from '@/stores/snackbar-store';
import { useIsPlatformAdmin } from '@/hooks/use-permission';

type ExtKind = 'skill' | 'plugin' | 'mcp';

/** kind → badge 变体三态映射:skill=default / plugin=secondary / mcp=outline。 */
function kindBadgeVariant(kind: string): 'default' | 'secondary' | 'outline' {
  if (kind === 'skill') return 'default';
  if (kind === 'plugin') return 'secondary';
  return 'outline';
}

/**
 * 分块 base64 编码(防大文件 call stack 溢出)。
 * 以 32KB 为一段循环 String.fromCharCode.apply 再拼接,避免单次 apply 参数过多爆栈;
 * 最后对整体二进制串 btoa 一次。
 */
function bytesToBase64(bytes: Uint8Array): string {
  const CHUNK = 0x8000; // 32KB,远低于引擎 call stack 上限
  let binary = '';
  for (let i = 0; i < bytes.length; i += CHUNK) {
    const slice = bytes.subarray(i, i + CHUNK);
    // fromCharCode.apply 接受 array-like,Uint8Array 满足;cast 绕开 TS 类型。
    binary += String.fromCharCode.apply(
      null,
      slice as unknown as number[],
    );
  }
  return btoa(binary);
}

/** 把 File 读成 ArrayBuffer(FileReader.readAsArrayBuffer)。 */
function readFileArrayBuffer(file: File): Promise<ArrayBuffer> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as ArrayBuffer);
    reader.onerror = () => reject(reader.error ?? new Error('read failed'));
    reader.readAsArrayBuffer(file);
  });
}

/** 目录选择 input 的非标准属性(webkitdirectory),React TS 类型未收录,需 cast 透传。 */
const directoryInputProps = { webkitdirectory: '' } as Record<string, string>;

export function ExtensionsTab() {
  const { t } = useTranslation();
  const isPlatformAdmin = useIsPlatformAdmin();

  const [list, setList] = useState<ExtensionListItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [uploading, setUploading] = useState(false);

  // 删除二次确认状态
  const [deleteTarget, setDeleteTarget] = useState<ExtensionListItem | null>(null);
  const [deleting, setDeleting] = useState(false);

  // 添加表单状态
  const [kind, setKind] = useState<ExtKind>('skill');
  const [name, setName] = useState('');
  const [marketplace, setMarketplace] = useState('');
  const [configText, setConfigText] = useState('');
  const [files, setFiles] = useState<File[]>([]);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const loadList = useCallback(async () => {
    setLoading(true);
    try {
      const data = await extensionsApi.list();
      setList(data ?? []);
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadList();
  }, [loadList]);

  const resetForm = () => {
    setName('');
    setMarketplace('');
    setConfigText('');
    setFiles([]);
    if (fileInputRef.current) fileInputRef.current.value = '';
  };

  const handleUpload = async () => {
    const trimmedName = name.trim();
    if (!trimmedName) {
      showSnackbar(t('Name is required'), 'error');
      return;
    }
    const body: UploadExtensionBody = { kind, name: trimmedName };

    if (kind === 'skill') {
      if (files.length === 0) {
        showSnackbar(t('Select a directory'), 'error');
        return;
      }
      const uploaded: UploadFile[] = [];
      try {
        for (const f of files) {
          const buf = await readFileArrayBuffer(f);
          const content_base64 = bytesToBase64(new Uint8Array(buf));
          // rel_path = webkitRelativePath 去掉首层目录名(目录选择器会带顶层目录名)。
          const rel =
            (f.webkitRelativePath || f.name).split('/').slice(1).join('/') ||
            f.name;
          uploaded.push({ rel_path: rel, content_base64 });
        }
      } catch (e: unknown) {
        showSnackbar(String(e), 'error');
        return;
      }
      body.files = uploaded;
    } else if (kind === 'plugin') {
      if (marketplace.trim()) body.marketplace = marketplace.trim();
    } else {
      // mcp
      if (!configText.trim()) {
        showSnackbar(t('Config text is required'), 'error');
        return;
      }
      body.config_text = configText;
    }

    setUploading(true);
    try {
      await extensionsApi.upload(body);
      showSnackbar(t('Added'), 'success');
      resetForm();
      void loadList();
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setUploading(false);
    }
  };

  const handleDelete = async () => {
    if (!deleteTarget) return;
    setDeleting(true);
    try {
      await extensionsApi.remove(deleteTarget.id);
      showSnackbar(t('Deleted'), 'success');
      setDeleteTarget(null);
      void loadList();
    } catch (e: unknown) {
      showSnackbar(String(e), 'error');
    } finally {
      setDeleting(false);
    }
  };

  return (
    <div className="space-y-6">
      {/* 扩展列表 */}
      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-medium">{t('Extensions')}</h2>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void loadList()}
            disabled={loading}
          >
            {loading ? t('Loading...') : t('Refresh')}
          </Button>
        </div>
        {loading && list.length === 0 ? (
          <p className="text-sm text-muted-foreground">{t('Loading...')}</p>
        ) : list.length === 0 ? (
          <p className="text-sm text-muted-foreground">{t('No extensions')}</p>
        ) : (
          <div className="space-y-2">
            {list.map((ext) => (
              <div
                key={ext.id}
                className="flex items-center justify-between rounded-lg border p-3"
              >
                <div className="flex items-center gap-2">
                  <Badge variant={kindBadgeVariant(ext.kind)}>{ext.kind}</Badge>
                  <span className="text-sm font-medium">{ext.name}</span>
                  {!ext.enabled && (
                    <Badge variant="outline">{t('Disabled')}</Badge>
                  )}
                </div>
                <div className="flex items-center gap-2">
                  {isPlatformAdmin && (
                    <Button
                      size="sm"
                      variant="destructive"
                      onClick={() => setDeleteTarget(ext)}
                    >
                      <Trash2 className="h-4 w-4" />
                      {t('Delete')}
                    </Button>
                  )}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 添加扩展区(仅平台管理员可见) */}
      {isPlatformAdmin && (
        <div className="space-y-4 border-t pt-4">
          <div className="flex items-center gap-2">
            <Upload className="h-4 w-4" />
            <span className="text-sm font-medium">{t('Add extension')}</span>
          </div>

          {/* kind 选择 */}
          <div className="flex flex-wrap gap-2">
            <Button
              size="sm"
              variant={kind === 'skill' ? 'default' : 'outline'}
              onClick={() => setKind('skill')}
            >
              <Puzzle className="h-4 w-4" />
              {t('Skill')}
            </Button>
            <Button
              size="sm"
              variant={kind === 'plugin' ? 'default' : 'outline'}
              onClick={() => setKind('plugin')}
            >
              <Package className="h-4 w-4" />
              {t('Plugin')}
            </Button>
            <Button
              size="sm"
              variant={kind === 'mcp' ? 'default' : 'outline'}
              onClick={() => setKind('mcp')}
            >
              <Wrench className="h-4 w-4" />
              {t('MCP')}
            </Button>
          </div>

          {/* name(所有 kind 共用) */}
          <div className="space-y-1">
            <label className="text-xs text-muted-foreground">{t('Name')}</label>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t('Extension name')}
            />
          </div>

          {/* 动态表单:按 kind 渲染不同字段 */}
          {kind === 'skill' && (
            <div className="space-y-1">
              <label className="text-xs text-muted-foreground">
                {t('Directory')}
              </label>
              <input
                ref={fileInputRef}
                type="file"
                multiple
                {...directoryInputProps}
                onChange={(e) => setFiles(Array.from(e.target.files ?? []))}
                className="block w-full text-sm text-muted-foreground file:mr-3 file:rounded file:border-0 file:bg-primary file:px-3 file:py-1.5 file:text-primary-foreground hover:file:bg-primary/80"
              />
              {files.length > 0 && (
                <p className="text-xs text-muted-foreground">
                  {t('{{count}} files selected', { count: files.length })}
                </p>
              )}
            </div>
          )}
          {kind === 'plugin' && (
            <div className="space-y-1">
              <label className="text-xs text-muted-foreground">
                {t('Marketplace')}
              </label>
              <Input
                value={marketplace}
                onChange={(e) => setMarketplace(e.target.value)}
                placeholder="openai-api-curated"
              />
            </div>
          )}
          {kind === 'mcp' && (
            <div className="space-y-1">
              <label className="text-xs text-muted-foreground">
                {t('Config text')}
              </label>
              <Textarea
                value={configText}
                onChange={(e) => setConfigText(e.target.value)}
                placeholder={'command = "node"\nargs = ["server.js"]'}
                className="min-h-32 font-mono text-xs"
              />
            </div>
          )}

          <Button
            onClick={() => void handleUpload()}
            disabled={uploading || !name.trim()}
          >
            {uploading ? t('Uploading...') : t('Add')}
          </Button>
        </div>
      )}
      {!isPlatformAdmin && (
        <p className="border-t pt-4 text-xs text-muted-foreground">
          {t('Only platform admins can add or delete extensions.')}
        </p>
      )}

      {/* 删除二次确认 */}
      <AlertDialog
        open={deleteTarget !== null}
        onOpenChange={(o) => !o && !deleting && setDeleteTarget(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('Delete extension')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t(
                'Delete extension "{{name}}"? This removes it from all nodes.',
                { name: deleteTarget?.name ?? '' },
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={deleting}>
              {t('Cancel')}
            </AlertDialogCancel>
            <AlertDialogAction
              disabled={deleting}
              onClick={(e) => {
                e.preventDefault();
                void handleDelete();
              }}
            >
              {t('Delete')}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
