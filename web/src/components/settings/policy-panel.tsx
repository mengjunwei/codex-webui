/**
 * 策略管理组件（spec 2026-07-23-policy-engine-design）
 *
 * 支持两层作用域：global（平台管理员）与 team（owner/admin）。
 * 通过 props 决定目标，组件内调对应 REST API。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Plus, Trash2, ShieldCheck, Pencil } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { showSnackbar } from '@/stores/snackbar-store';
import { getApiErrorMessage } from '@/lib/api-error';
import {
  policyApi,
  type CreatePolicyRuleBody,
  type PolicyAction,
  type PolicyMatchMode,
  type PolicyRule,
  type PolicyRuleType,
  type PolicyRoleFilter,
  type UpdatePolicyRuleBody,
} from '@/lib/mt-client';

const RULE_TYPES: PolicyRuleType[] = ['command', 'tool', 'skill', 'plugin', 'mcp'];
const MATCH_MODES: PolicyMatchMode[] = ['blacklist', 'whitelist', 'regex', 'exact'];
const ACTIONS: PolicyAction[] = ['deny', 'allow'];
const ROLES: PolicyRoleFilter[] = ['owner', 'admin', 'member'];

const SCOPE_TEAM: 'global' | 'team' = 'team';

function emptyDraft(): CreatePolicyRuleBody {
  return {
    scope: SCOPE_TEAM,
    rule_type: 'command',
    match_mode: 'blacklist',
    pattern: '',
    action: 'deny',
    priority: 0,
    enabled: true,
    description: '',
    role: undefined,
  };
}

function draftFromRule(rule: PolicyRule): CreatePolicyRuleBody {
  return {
    scope: rule.scope,
    team_id: rule.team_id ?? undefined,
    role: rule.role ?? undefined,
    rule_type: rule.rule_type,
    match_mode: rule.match_mode,
    pattern: rule.pattern,
    action: rule.action,
    priority: rule.priority,
    enabled: rule.enabled,
    description: rule.description ?? '',
  };
}

interface PanelProps {
  /** 'global' 调 /policies/global；'team' 调 /teams/{teamId}/policies。 */
  scope: 'global' | 'team';
  /** scope=team 时必填。 */
  teamId?: string;
}

export function PolicyPanel({ scope, teamId }: PanelProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState<PolicyRule | null>(null);
  const [creating, setCreating] = useState(false);

  const queryKey = scope === 'global'
    ? ['policies', 'global'] as const
    : ['policies', 'team', teamId] as const;

  const listQuery = useQuery({
    queryKey,
    enabled: scope === 'global' || !!teamId,
    queryFn: async () => {
      if (scope === 'global') return (await policyApi.listGlobal()).items;
      if (!teamId) return [];
      return (await policyApi.listTeam(teamId)).items;
    },
  });

  const invalidate = () => {
    void queryClient.invalidateQueries({ queryKey });
  };

  const createMut = useMutation({
    mutationFn: async (body: CreatePolicyRuleBody) => {
      if (scope === 'global') return policyApi.createGlobal(body);
      if (!teamId) throw new Error('teamId required');
      return policyApi.createTeam(teamId, body);
    },
    onSuccess: () => {
      invalidate();
      setCreating(false);
      showSnackbar(t('策略已创建'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const updateMut = useMutation({
    mutationFn: async (vars: { id: string; body: UpdatePolicyRuleBody }) => {
      if (scope === 'global') return policyApi.updateGlobal(vars.id, vars.body);
      if (!teamId) throw new Error('teamId required');
      return policyApi.updateTeam(teamId, vars.id, vars.body);
    },
    onSuccess: () => {
      invalidate();
      setEditing(null);
      showSnackbar(t('策略已更新'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const deleteMut = useMutation({
    mutationFn: async (id: string) => {
      if (scope === 'global') return policyApi.deleteGlobal(id);
      if (!teamId) throw new Error('teamId required');
      return policyApi.deleteTeam(teamId, id);
    },
    onSuccess: () => {
      invalidate();
      showSnackbar(t('策略已删除'), 'success');
    },
    onError: (err) => showSnackbar(getApiErrorMessage(err), 'error'),
  });

  const rules = listQuery.data ?? [];

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-2 text-lg font-semibold">
            <ShieldCheck className="h-5 w-5" />
            {scope === 'global' ? t('全局策略') : t('团队策略')}
          </h2>
          <p className="text-sm text-muted-foreground">
            {scope === 'global'
              ? t('命令审查与 skill / plugin / mcp 使用限制。命中 deny 立即拦截；命中 allow 由原决策表继续判定。')
              : t('本团队策略覆盖全局默认。team > global，按 priority desc 排序。')}
          </p>
        </div>
        <Button size="sm" onClick={() => setCreating(true)}>
          <Plus className="mr-1 h-4 w-4" />
          {t('新建规则')}
        </Button>
      </div>

      {listQuery.isLoading && (
        <p className="text-sm text-muted-foreground">{t('加载中…')}</p>
      )}
      {listQuery.error && (
        <p className="text-sm text-destructive">{getApiErrorMessage(listQuery.error)}</p>
      )}

      <div className="space-y-2">
        {rules.length === 0 && !listQuery.isLoading && (
          <p className="text-sm text-muted-foreground">{t('暂无规则')}</p>
        )}
        {rules.map((r) => (
          <div key={r.id} className="rounded-lg border border-border bg-card/40 p-4">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="min-w-0 flex-1 space-y-1">
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant={r.action === 'deny' ? 'destructive' : 'default'}>
                    {r.action}
                  </Badge>
                  <Badge variant="outline">{r.rule_type}</Badge>
                  <Badge variant="outline">{r.match_mode}</Badge>
                  {r.role && <Badge variant="secondary">role={r.role}</Badge>}
                  <Badge variant={r.enabled ? 'default' : 'outline'}>
                    {r.enabled ? t('启用') : t('禁用')}
                  </Badge>
                  <span className="text-xs text-muted-foreground">
                    priority={r.priority}
                  </span>
                </div>
                <code className="block break-all rounded bg-muted px-2 py-1 text-sm">
                  {r.pattern}
                </code>
                {r.description && (
                  <p className="text-xs text-muted-foreground">{r.description}</p>
                )}
              </div>
              <div className="flex shrink-0 gap-2">
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8"
                  aria-label={t('编辑')}
                  onClick={() => setEditing(r)}
                >
                  <Pencil className="h-4 w-4" />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 text-destructive"
                  aria-label={t('删除')}
                  onClick={() => {
                    if (window.confirm(t('确定删除该规则？'))) {
                      deleteMut.mutate(r.id);
                    }
                  }}
                >
                  <Trash2 className="h-4 w-4" />
                </Button>
              </div>
            </div>
          </div>
        ))}
      </div>

      <PolicyRuleDialog
        open={creating || !!editing}
        initial={editing ? draftFromRule(editing) : { ...emptyDraft(), scope }}
        title={editing ? t('编辑策略') : t('新建策略')}
        onClose={() => {
          setCreating(false);
          setEditing(null);
        }}
        onSubmit={(draft) => {
          if (editing) {
            const body: UpdatePolicyRuleBody = {
              rule_type: draft.rule_type,
              match_mode: draft.match_mode,
              pattern: draft.pattern,
              action: draft.action,
              priority: draft.priority,
              enabled: draft.enabled,
              description: draft.description || undefined,
              role: draft.role,
            };
            updateMut.mutate({ id: editing.id, body });
          } else {
            createMut.mutate(draft);
          }
        }}
        submitting={createMut.isPending || updateMut.isPending}
      />
    </div>
  );
}

// ── 编辑/创建表单 ──────────────────────────────────────────────

interface DialogProps {
  open: boolean;
  initial: CreatePolicyRuleBody;
  title: string;
  onClose: () => void;
  onSubmit: (body: CreatePolicyRuleBody) => void;
  submitting?: boolean;
}

function PolicyRuleDialog({ open, initial, title, onClose, onSubmit, submitting }: DialogProps) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState<CreatePolicyRuleBody>(initial);

  // 打开对话框时同步 initial。
  // initial 来自 props 每次重渲染是新对象，但因 open=false 时不渲染，状态保留可避免抖动。
  // 真正打开（open: false→true）时需要重置——通过 key 强制 Dialog 重建更简单。
  return (
    <Dialog open={open} onOpenChange={(o) => { if (!o) onClose(); }}>
      <DialogContent className="max-w-xl" key={`${open}-${initial.pattern}-${initial.priority}`}>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
        </DialogHeader>

        <div className="grid gap-3 sm:grid-cols-2">
          <Field label={t('类型（rule_type）')}>
            <select
              className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              value={draft.rule_type}
              onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                setDraft({ ...draft, rule_type: e.target.value as PolicyRuleType })
              }
            >
              {RULE_TYPES.map((v) => (
                <option key={v} value={v}>{v}</option>
              ))}
            </select>
          </Field>

          <Field label={t('匹配模式')}>
            <select
              className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              value={draft.match_mode}
              onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                setDraft({ ...draft, match_mode: e.target.value as PolicyMatchMode })
              }
            >
              {MATCH_MODES.map((v) => (
                <option key={v} value={v}>{v}</option>
              ))}
            </select>
          </Field>

          <Field label={t('动作')}>
            <select
              className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              value={draft.action}
              onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                setDraft({ ...draft, action: e.target.value as PolicyAction })
              }
            >
              {ACTIONS.map((v) => (
                <option key={v} value={v}>{v}</option>
              ))}
            </select>
          </Field>

          <Field label={t('角色过滤（可选）')}>
            <select
              className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              value={draft.role ?? '__none__'}
              onChange={(e: React.ChangeEvent<HTMLSelectElement>) => {
                const v = e.target.value;
                setDraft({ ...draft, role: v === '__none__' ? undefined : v as PolicyRoleFilter });
              }}
            >
              <option value="__none__">{t('所有角色')}</option>
              {ROLES.map((v) => (
                <option key={v} value={v}>{v}</option>
              ))}
            </select>
          </Field>

          <Field label={t('优先级（priority desc）')}>
            <Input
              type="number"
              value={draft.priority ?? 0}
              onChange={(e) => setDraft({ ...draft, priority: Number(e.target.value) })}
            />
          </Field>

          <Field label={t('启用')}>
            <div className="flex h-10 items-center">
              <Switch
                checked={draft.enabled ?? true}
                onCheckedChange={(v: boolean) => setDraft({ ...draft, enabled: v })}
              />
            </div>
          </Field>
        </div>

        <Field label={t('pattern（按 match_mode 解释：exact=完全相等 / blacklist,whitelist=子串忽略大小写 / regex=正则）')}>
          <Textarea
            value={draft.pattern}
            onChange={(e) => setDraft({ ...draft, pattern: e.target.value })}
            placeholder={draft.rule_type === 'command' ? 'rm -rf /' : draft.rule_type === 'tool' ? 'shell.exec' : ''}
            className="min-h-[80px]"
          />
        </Field>

        <Field label={t('描述（可选）')}>
          <Input
            value={draft.description ?? ''}
            onChange={(e) => setDraft({ ...draft, description: e.target.value })}
          />
        </Field>

        <DialogFooter>
          <Button variant="outline" onClick={onClose}>{t('取消')}</Button>
          <Button
            disabled={submitting || !draft.pattern.trim()}
            onClick={() => onSubmit(draft)}
          >
            {submitting ? t('提交中…') : t('保存')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="space-y-1">
      <span className="text-xs font-medium text-muted-foreground">{label}</span>
      {children}
    </label>
  );
}