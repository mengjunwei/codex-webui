# 权限加固 批次3b：前端权限 UI 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** 前端接入后端权限系统——user-store + usePermission hook 驱动 UI 显隐，成员管理支持角色变更/转让/解散，设置页加平台管理员管理，全局写操作按 is_platform_admin 收紧。

**Architecture:** 四层——(1) 基础设施（mt-client 类型+方法、user-store、use-permission hook）；(2) me 数据流（登录/挂载拉 me、account-settings 改读 store）；(3) team 管理 UI（角色下拉、转让、解散、权限守卫）；(4) 平台管理员 tab + 全局写 UI 适配。前端用 Zustand，无单元测试，验证靠 `npm run build`（tsc + vite）+ lint。

**Tech Stack:** React + TypeScript + Zustand + Vite + shadcn/ui（Badge/Button/Dialog 等）。

## Global Constraints

- 中文 UI 文案 + 中文注释。
- `npm run build`（在 `web/` 下，= `tsc -b && vite build`）零错误；`npm run lint` 无新增 error。
- 权限码字符串与后端 `TeamPermission::code()` 严格一致（`team:member:list` 等 12 个）。
- 角色值 `owner`/`admin`/`member`。
- 无权限的入口**主动隐藏**（不靠点了报错）；平台管理员 tab 非管理员不可见。
- 不改后端（批次3a 已完成）；不改认证 token 存储机制。
- 提交 220e4a9（用户 config.rs）不涉及前端，无需关注。

---

### Task 1: 前端权限基础设施（类型 + user-store + hook）

**Files:**
- Modify: `web/src/lib/mt-client.ts`（加类型 + 4 个 API 方法）
- Create: `web/src/stores/user-store.ts`（zustand，存 /me 响应）
- Create: `web/src/hooks/use-permission.ts`（usePermission / useIsPlatformAdmin / useCurrentRole）

**Interfaces:**
- Produces: `MeResponse`/`Role`/`TeamPermission` 类型；`authApi.getMe`、`teamsApi.transferOwner/dissolve/setMemberRole`；`useUserStore`（me/loadMe/clearMe）；`usePermission(perm)`/`useIsPlatformAdmin()`/`useCurrentRole()`

- [ ] **Step 1: mt-client.ts 加类型与方法**

在 `web/src/lib/mt-client.ts` 类型区加：

```ts
export type Role = 'owner' | 'admin' | 'member';

export const TEAM_PERMISSIONS = [
  'team:member:list', 'team:member:invite', 'team:member:remove', 'team:member:role:write',
  'team:api_key:read', 'team:api_key:write', 'team:audit:read',
  'team:thread:create', 'team:thread:read', 'team:turn:write',
  'team:owner:transfer', 'team:dissolve',
] as const;
export type TeamPermission = typeof TEAM_PERMISSIONS[number];

/** 后端 GET /api/mt/me 响应(字段 snake_case,对齐 handlers.rs serde_json::json!)。 */
export interface MeResponse {
  user: { id: string; email: string; display_name: string | null };
  is_platform_admin: boolean;
  teams: Array<{ team_id: string; role: Role; permissions: TeamPermission[] }>;
}
```

`authApi` 加 `getMe`：

```ts
export const authApi = {
  register: (body: RegisterBody) => mtFetch<AuthResponse>('/auth/register', 'POST', body),
  login: (body: LoginBody) => mtFetch<AuthResponse>('/auth/login', 'POST', body),
  refresh: (body: { refreshToken: string }) => mtFetch<AuthResponse>('/auth/refresh', 'POST', body),
  getMe: () => mtFetch<MeResponse>('/me'),
};
```

`teamsApi` 加 3 个方法：

```ts
  transferOwner: (teamId: string, newOwnerUserId: string) =>
    mtFetch<void>(`/teams/${teamId}/transfer`, 'POST', { newOwnerUserId }),
  dissolve: (teamId: string) =>
    mtFetch<void>(`/teams/${teamId}`, 'DELETE'),
  setMemberRole: (teamId: string, userId: string, role: Role) =>
    mtFetch<void>(`/teams/${teamId}/members/${userId}/role`, 'PATCH', { role }),
```

- [ ] **Step 2: user-store.ts**

新建 `web/src/stores/user-store.ts`（参考 team-store.ts 的 zustand 模式）：

```ts
//! 当前用户身份 store:存 GET /api/mt/me 响应(user + is_platform_admin + 各 team 角色/权限)。
//! 供 usePermission hook 驱动 UI 显隐。登录后/挂载时 loadMe()。
import { create } from 'zustand';
import { authApi, type MeResponse } from '@/lib/mt-client';

interface UserStore {
  me: MeResponse | null;
  loading: boolean;
  error: string | null;
  loadMe: () => Promise<void>;
  clearMe: () => void;
}

export const useUserStore = create<UserStore>((set) => ({
  me: null,
  loading: false,
  error: null,
  loadMe: async () => {
    set({ loading: true, error: null });
    try {
      const me = await authApi.getMe();
      set({ me, loading: false });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : 'failed to load user' });
    }
  },
  clearMe: () => set({ me: null, loading: false, error: null }),
}));
```

> 确认 `@/` 别名指向 `web/src/`（看现有 store 的 import 风格，如 team-store 用 `@/lib/mt-client`）。

- [ ] **Step 3: use-permission.ts hook**

新建 `web/src/hooks/use-permission.ts`：

```ts
//! 权限驱动 hook:基于 user-store(me) + team-store(currentTeamId) 判定当前用户权限。
import { useMemo } from 'react';
import { useUserStore } from '@/stores/user-store';
import { useTeamStore } from '@/stores/team-store';
import type { TeamPermission } from '@/lib/mt-client';

/** 当前用户在 currentTeam 是否持有 perm。 */
export function usePermission(perm: TeamPermission): boolean {
  const me = useUserStore((s) => s.me);
  const currentTeamId = useTeamStore((s) => s.currentTeamId);
  return useMemo(() => {
    if (!me || !currentTeamId) return false;
    const m = me.teams.find((t) => t.team_id === currentTeamId);
    return m?.permissions.includes(perm) ?? false;
  }, [me, currentTeamId, perm]);
}

/** 当前用户是否为平台超级管理员。 */
export function useIsPlatformAdmin(): boolean {
  return useUserStore((s) => s.me?.is_platform_admin ?? false);
}

/** 当前用户在 currentTeam 的角色(无 team/非成员返回 null)。 */
export function useCurrentRole() {
  const me = useUserStore((s) => s.me);
  const currentTeamId = useTeamStore((s) => s.currentTeamId);
  return useMemo(() => {
    if (!me || !currentTeamId) return null;
    return me.teams.find((t) => t.team_id === currentTeamId)?.role ?? null;
  }, [me, currentTeamId]);
}
```

- [ ] **Step 4: 构建验证**

Run（`web/`）: `npm run build`
Expected: tsc + vite build 零错误。

- [ ] **Step 5: Commit**

```bash
git add web/src/lib/mt-client.ts web/src/stores/user-store.ts web/src/hooks/use-permission.ts
git commit -m "feat(web): 权限基础设施——MeResponse 类型 + user-store + usePermission hook"
```

---

### Task 2: me 数据流（登录/挂载拉取 + account-settings 改读 store）

**Files:**
- Modify: `web/src/routes/login-route.tsx`（登录成功后 loadMe）
- Modify: `web/src/routes/authenticated-layout.tsx`（挂载时 loadMe effect）
- Modify: `web/src/components/settings/account/account-settings.tsx`（JWT 解码 → 读 user-store）

- [ ] **Step 1: login-route 登录后 loadMe**

在 `login-route.tsx` handleLogin 成功分支（setApiToken/setRefreshToken/resetSocket 之后、navigate 之前）加：

```ts
    void useUserStore.getState().loadMe();
```

> 也可 `import { useUserStore } from '@/stores/user-store'` 后用 `.getState()`（非 hook 上下文）。登录失败分支（clearApiToken）加 `useUserStore.getState().clearMe()`。

- [ ] **Step 2: authenticated-layout 挂载 loadMe**

在 `authenticated-layout.tsx`（参考 `useCodexSocket(true)` 旁边，约 L162）加 effect：

```ts
  useEffect(() => {
    if (!useUserStore.getState().me) {
      void useUserStore.getState().loadMe();
    }
  }, []);
```

> 确保 import useEffect + useUserStore。同时 auth-expired 处理（L277-285）加 `useUserStore.getState().clearMe()`。

- [ ] **Step 3: account-settings 改读 user-store**

`account-settings.tsx` L41-53 的 JWT 解码 effect 替换为读 user-store：

```ts
  const me = useUserStore((s) => s.me);
  // 删除原 JWT atob 解码 effect;user 来 self me
  const user = me ? { id: me.user.id, email: me.user.email } : null;
  const loading = useUserStore((s) => s.loading) && !me;
```

> 适配组件实际用的 user/loading 变量名。import useUserStore。

- [ ] **Step 4: 构建验证**

Run（`web/`）: `npm run build`
Expected: 零错误。

- [ ] **Step 5: Commit**

```bash
git add web/src/routes/login-route.tsx web/src/routes/authenticated-layout.tsx web/src/components/settings/account/account-settings.tsx
git commit -m "feat(web): me 数据流——登录/挂载拉取 + account-settings 改读 user-store"
```

---

### Task 3: team 管理 UI（角色变更 + 转让 + 解散 + 权限守卫）

**Files:**
- Modify: `web/src/components/team/team-members.tsx`（角色 badge 三色 + 角色下拉 + 转让 + 解散 + 权限守卫）
- Modify: `web/src/components/team/team-settings.tsx`（API key 权限守卫 + 修 currentTeam）

- [ ] **Step 1: team-members 改造**

`team-members.tsx`：
- import `usePermission`, `useUserStore`, `teamsApi.setMemberRole/transferOwner/dissolve`, `Role` 类型, 现有 shadcn 组件（Select/Dialog/AlertDialog 等）
- 角色 badge：`owner`→default、`admin`→secondary(蓝)、`member`→outline
- 每个非 owner 成员行：若 `usePermission('team:member:role:write')` 显示角色 Select（admin↔member），onChange 调 `setMemberRole` 后 `loadMembers` 刷新
- Remove 按钮：用 `usePermission('team:member:remove')` 守卫（替代只看被操作人 role）
- Dialog 顶部危险操作区：
  - `usePermission('team:owner:transfer')` → 显示"转让队长"（Select 选成员 + 确认 AlertDialog → `transferOwner`）
  - `usePermission('team:dissolve')` → 显示"解散团队"（红色，AlertDialog 二次确认输入团队名 → `dissolve`，成功后清 currentTeam + loadTeams）
- 操作后 `showSnackbar`（@/stores/snackbar-store）反馈

> 完整 JSX 较长，参考现有 L88-109 结构扩展。二次确认用 shadcn AlertDialog。

- [ ] **Step 2: team-settings 权限守卫**

`team-settings.tsx`：
- L23 `const [team] = useState<TeamDto | null>(null)` 改为从 `useTeamStore((s) => s.currentTeam)` 读（修复 team info 块永假 bug）
- API key 区：`usePermission('team:api_key:read')` 守卫列表显示，`usePermission('team:api_key:write')` 守卫设置入口

- [ ] **Step 3: 构建验证**

Run（`web/`）: `npm run build && npm run lint`
Expected: 零错误（lint 无新增 error）。

- [ ] **Step 4: Commit**

```bash
git add web/src/components/team/team-members.tsx web/src/components/team/team-settings.tsx
git commit -m "feat(web): team 管理 UI——角色变更 + 转让 + 解散 + 权限守卫"
```

---

### Task 4: 设置页平台管理员 tab + 全局写 UI 适配

**Files:**
- Modify: `web/src/components/settings/settings-page.tsx`（加 platform tab，isPlatformAdmin 守卫）
- Modify: `web/src/components/settings/setting-helpers.ts`（sectionLabel 加 platform）
- Create: `web/src/components/settings/platform-admin-panel.tsx`（管理员列表 + 增删）

**Interfaces:**
- Consumes: 后端 `/api/settings` 写、`/api/logs` 已是平台管理员专属（批次3a）。本 task 加前端管理 UI + 显隐。
- 注：后端平台管理员的增删 API（POST/DELETE 管理员）**批次3a 未实现**（只有 bootstrap via config）。本 task 前端 panel 先做**只读展示 + 提示"通过 config.toml admin_emails 配置"**，增删 API 留后续（在 panel 标注）。若要完整增删，需后端补 API（超本批次）。

- [ ] **Step 1: setting-helpers 加 platform label**

`setting-helpers.ts` sectionLabel（L61-72）加 `platform: '平台管理'`（或 `Platform`）。

- [ ] **Step 2: settings-page 加 platform tab**

`settings-page.tsx`：
- SECTIONS（L23）加 `'platform'`
- Tab 按钮渲染（L74-85）：platform tab 仅 `useIsPlatformAdmin()` 为 true 时渲染
- 加 `{section === 'platform' && <PlatformAdminPanel />}`

- [ ] **Step 3: platform-admin-panel.tsx**

新建 `web/src/components/settings/platform-admin-panel.tsx`：

```tsx
//! 平台管理员面板:展示当前管理员(is_platform_admin=true 的用户)+ 提示配置方式。
//! 增删 API 待后端补(当前仅 config.toml admin_emails bootstrap);此处只读 + 引导。
import { useUserStore } from '@/stores/user-store';
// import { useTranslation } from '...'; // 用项目现有 i18n

export function PlatformAdminPanel() {
  const me = useUserStore((s) => s.me);
  return (
    <div className="space-y-4">
      <h2 className="text-lg font-semibold">平台管理</h2>
      <p className="text-sm text-muted-foreground">
        平台管理员可修改全局配置、读取全局日志。当前管理员通过 <code>config.toml</code> 的{' '}
        <code>[security] admin_emails</code> 在启动时 bootstrap。
      </p>
      <div className="rounded-lg border p-3">
        <span className="text-sm font-medium">{me?.user.email}</span>
        <span className="ml-2 text-xs text-muted-foreground">(你,平台管理员)</span>
      </div>
      <p className="text-xs text-muted-foreground">
        提示:增删管理员的 API 尚未实现,当前通过配置文件管理。
      </p>
    </div>
  );
}
```

- [ ] **Step 4: 全局写操作 UI 适配（非管理员隐藏）**

设置页其他 tab 中涉及全局写（settings PATCH、files 写、logs）的入口：若存在前端入口，用 `useIsPlatformAdmin()` 守卫隐藏。grep settings-page 及其子组件的写操作入口，按需加守卫。若这些 tab 本就是平台管理员专属内容（如全局 settings），整个 tab 用 isPlatformAdmin 守卫。

> 实际范围：settings-page 的 general/security/files 等 tab 若含全局配置写，非管理员应看不到写控件。最小实现：在 platform tab 之外，对明显的全局写入口加 isPlatformAdmin 守卫；复杂度高的留手动验证。

- [ ] **Step 5: 构建验证**

Run（`web/`）: `npm run build && npm run lint`
Expected: 零错误。

- [ ] **Step 6: Commit**

```bash
git add web/src/components/settings/settings-page.tsx web/src/components/settings/setting-helpers.ts web/src/components/settings/platform-admin-panel.tsx
git commit -m "feat(web): 设置页平台管理员 tab + 全局写 UI 适配"
```

---

## Self-Review 结果

**1. Spec 覆盖**：批次3b 覆盖 spec §4.9 前端（usePermission、成员角色管理、转让/解散、平台管理员 tab、全局 UI 适配）。account-settings JWT 解码替换、team-selector role badge 为可选增强（本批次不做 team-selector，留后续）。
**2. 占位符**：基础设施代码完整；team-members 改造因 JSX 长参考现有结构扩展（给明确点）；平台管理员增删 API 后端未实现，panel 做只读 + 引导（诚实标注，非占位）。
**3. 类型一致**：`TeamPermission` 12 个码与后端 code() 一致；`Role` 三值；MeResponse snake_case 对齐后端。
**4. 测试缺口**：前端无单元测试设施，用 build（tsc+vite）+ lint 验证；手动验证留批次4。

## 批次3b 完成后

- 前端权限 UI 完整：权限驱动显隐 + 成员管理 + 转让/解散 + 平台管理员
- 进入批次4（限流加固 + 文档同步 + 回归测试 + DB 测试设施）
