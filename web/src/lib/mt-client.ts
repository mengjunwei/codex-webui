/**
 * 多租户 API 客户端（手写，字段名严格对齐后端 handlers.rs 的 serde rename）。
 *
 * 后端 API 规则：
 * - 所有 camelCase 字段通过 #[serde(rename)] 定义
 * - 列表端点返回直接数组（非 {items:[...]} 包装）
 * - teamId 查询参数（非 team_id）
 */

import { getApiToken } from '../auth-token';

// ── 工具函数 ──────────────────────────────────────────────────

function authHeaders(): Record<string, string> {
  const token = getApiToken();
  return token ? { 'Authorization': `Bearer ${token}` } : {};
}

export async function mtFetch<T>(
  path: string,
  method: string = 'GET',
  body?: unknown,
): Promise<T> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...authHeaders(),
  };

  const response = await fetch(`/api/mt${path}`, {
    method,
    headers,
    body: body ? JSON.stringify(body) : undefined,
  });

  if (!response.ok) {
    const errorData = await response.json().catch(() => ({}));
    throw new Error(
      (errorData as { message?: string }).message ||
      `API error: ${response.status} ${response.statusText}`,
    );
  }

  // 204 No Content
  if (response.status === 204) return undefined as T;
  return response.json();
}

// ── 类型定义（字段名严格对齐后端 serde rename）─────────────────

/** 后端 AuthResp: { user, accessToken, refreshToken, expiresIn } */
export interface AuthResponse {
  user: { id: string; email: string; display_name: string | null };
  accessToken: string;
  refreshToken: string;
  expiresIn: number;
}

export interface TeamDto {
  id: string;
  name: string;
  owner_id: string;
  created_at: number;
  updated_at: number;
}

/** 后端 MemberView */
export interface MemberDto {
  user_id: string;
  role: string;
  joined_at: number;
  display_name?: string;
  email?: string;
}

/** 后端 ApiKeyResp */
export interface ApiKeyResp {
  id: string;
  team_id: string;
  provider: string;
  key_hint: string;
  set_by: string;
  is_active: boolean;
  created_at: number;
}

/** 后端 thread::Model */
export interface ThreadDto {
  id: string;
  team_id: string;
  created_by_user_id: string;
  title: string | null;
  cwd?: string;
  status: string;
  created_at: number;
  updated_at: number;
  last_activity_at: number;
}

/** 后端 pending_server_request::Model */
export interface PendingApproval {
  generation: number;
  request_id: string;
  thread_id: string;
  team_id: string | null;
  turn_id: string | null;
  item_id: string | null;
  method: string;
  params_json: string;
  status: string;
  resolved_by: string | null;
  created_at: number;
  updated_at: number;
  resolved_at: number | null;
}

/** 后端 audit_log::Model */
export interface AuditEntry {
  id: string;
  team_id: string | null;
  user_id: string | null;
  thread_id: string | null;
  event_type: string;
  tool_name: string | null;
  payload_json: string;
  decision: string | null;
  created_at: number;
}

// ── 后端实体类型（从 backend-rs/src/db/entity.rs 推断）───────────────────

/** 后端 thread::Model */
export interface ThreadDto {
  id: string;
  team_id: string;
  created_by_user_id: string;
  title: string | null;
  cwd?: string;
  status: string;
  /** workspace 归属:"personal"(个人 workspace) / "team"(团队 workspace)。 */
  workspace_type: 'personal' | 'team';
  created_at: number;
  updated_at: number;
  last_activity_at: number;
}

/** 后端 token_usage_snapshot::Model */
export interface TokenUsageSnapshot {
  thread_id: string;
  turn_id: string;
  team_id: string | null;
  total_tokens: number;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  reasoning_output_tokens: number;
  last_total_tokens: number;
  last_input_tokens: number;
  last_cached_input_tokens: number;
  last_output_tokens: number;
  model_context_window: number | null;
  raw_payload: string;
  updated_at: number;
}

/** 后端 turn_diff::Model */
export interface TurnDiffDto {
  thread_id: string;
  turn_id: string;
  team_id: string | null;
  diff: string;
  updated_at: number;
}

/** 后端 turn_error::Model */
export interface TurnErrorDto {
  thread_id: string;
  turn_id: string;
  team_id: string | null;
  message: string;
  created_at: number;
}

/** 后端 pending_server_request::Model */
export interface PendingApproval {
  generation: number;
  request_id: string;
  thread_id: string;
  team_id: string | null;
  turn_id: string | null;
  item_id: string | null;
  method: string;
  params_json: string;
  status: string;
  resolved_by: string | null;
  created_at: number;
  updated_at: number;
  resolved_at: number | null;
}

/** 后端 audit_log::Model */
export interface AuditEntry {
  id: string;
  team_id: string | null;
  actor_user_id: string;
  action: string;
  detail: string | null;
  created_at: number;
}

// ── 权限相关类型（对齐后端 GET /api/mt/me 响应）──────────────────

/** 团队内角色:owner(所有者) / admin(管理员) / member(普通成员)。 */
export type Role = 'owner' | 'admin' | 'member';

/** 团队权限全集(对齐后端 TeamPermission 枚举)。 */
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

// ── 请求 Body 类型 ────────────────────────────────────────────

export interface RegisterBody { email: string; password: string; }
export interface LoginBody { email: string; password: string; }
export interface CreateTeamBody { name: string; }
export interface JoinBody { code: string; }
export interface CreateInvitationBody { expiresAt?: number; maxUses?: number; }
export interface SetKeyBody { key: string; provider?: string; }
export interface CreateThreadBody { teamId?: string; cwd?: string; [key: string]: unknown; }
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export interface ResolveApprovalBody { requestId?: string; approved: boolean; result?: any; }
export interface RenameThreadBody { name: string; }
export interface InvokeThreadBody { method: string; threadId?: string; params?: unknown; }

// ── 认证 API ──────────────────────────────────────────────────

export const authApi = {
  register: (body: RegisterBody) =>
    mtFetch<AuthResponse>('/auth/register', 'POST', body),
  login: (body: LoginBody) =>
    mtFetch<AuthResponse>('/auth/login', 'POST', body),
  refresh: (body: { refreshToken: string }) =>
    mtFetch<AuthResponse>('/auth/refresh', 'POST', body),
  /** 当前登录用户身份(含 is_platform_admin 与各 team 角色/权限)。 */
  getMe: () => mtFetch<MeResponse>('/me'),
};

// ── Team API（列表返回直接数组）──────────────────────────────

export const teamsApi = {
  create: (body: CreateTeamBody) =>
    mtFetch<TeamDto>('/teams', 'POST', body),
  list: () =>
    mtFetch<TeamDto[]>('/teams'),
  getMembers: (teamId: string) =>
    mtFetch<MemberDto[]>(`/teams/${teamId}/members`),
  removeMember: (teamId: string, userId: string) =>
    mtFetch<void>(`/teams/${teamId}/members/${userId}`, 'DELETE'),
  createInvitation: (teamId: string, body: CreateInvitationBody) =>
    mtFetch<unknown>(`/teams/${teamId}/invitations`, 'POST', body),
  join: (body: JoinBody) =>
    mtFetch<TeamDto>('/teams/join', 'POST', body),
  setApiKey: (teamId: string, body: SetKeyBody) =>
    mtFetch<ApiKeyResp>(`/teams/${teamId}/api-key`, 'POST', body),
  listApiKeys: (teamId: string) =>
    mtFetch<ApiKeyResp[]>(`/teams/${teamId}/api-key`),
  listAudit: (teamId: string, limit?: number) =>
    mtFetch<AuditEntry[]>(`/teams/${teamId}/audit${limit ? `?limit=${limit}` : ''}`),
  /** 转移团队所有权给另一成员(仅 owner)。 */
  transferOwner: (teamId: string, newOwnerUserId: string) =>
    mtFetch<void>(`/teams/${teamId}/transfer`, 'POST', { newOwnerUserId }),
  /** 解散团队(仅 owner)。 */
  dissolve: (teamId: string) =>
    mtFetch<void>(`/teams/${teamId}`, 'DELETE'),
  /** 调整成员角色(owner/admin/member)。 */
  setMemberRole: (teamId: string, userId: string, role: Role) =>
    mtFetch<void>(`/teams/${teamId}/members/${userId}/role`, 'PATCH', { role }),
};

// ── Threads API（列表返回直接数组；查询参数 teamId camelCase）──

export const threadsApi = {
  create: (body: CreateThreadBody) =>
    mtFetch<unknown>('/threads', 'POST', body),
  /** 单 team 列表(按 teamId 过滤)。 */
  list: (teamId: string) =>
    mtFetch<ThreadDto[]>(`/threads?teamId=${encodeURIComponent(teamId)}`),
  /** 聚合列表:当前用户所有团队 workspace + 个人 workspace 的全部会话。 */
  listMine: () => mtFetch<ThreadDto[]>('/threads/me'),
  startTurn: (threadId: string, body: Record<string, unknown>) =>
    mtFetch<unknown>(`/threads/${threadId}/turns`, 'POST', body),
  invoke: (threadId: string, body: InvokeThreadBody) =>
    mtFetch<unknown>(`/threads/${threadId}/invoke`, 'POST', body),
  archive: (threadId: string) =>
    mtFetch<void>(`/threads/${threadId}/archive`, 'POST'),
  rename: (threadId: string, body: RenameThreadBody) =>
    mtFetch<void>(`/threads/${threadId}/name`, 'PATCH', body),
  /** 删除会话(含归档):codex thread/delete + 清 PG + 删 rollout。 */
  remove: (threadId: string) =>
    mtFetch<void>(`/threads/${threadId}`, 'DELETE'),
};

// ── Approvals API ─────────────────────────────────────────────

export const approvalsApi = {
  list: (threadId: string) =>
    mtFetch<PendingApproval[]>(`/threads/${threadId}/approvals`),
  respond: (threadId: string, requestId: string, body: ResolveApprovalBody) =>
    mtFetch<void>(`/threads/${threadId}/approvals`, 'POST', { ...body, requestId }),
};

// ── 补充数据 API（后端 /api/mt/threads/{id}/...）─────────────────

export const tokenUsageApi = {
  get: (threadId: string) =>
    mtFetch<TokenUsageSnapshot[]>(`/threads/${threadId}/token-usage`),
};

export const turnDiffApi = {
  list: (threadId: string) =>
    mtFetch<TurnDiffDto[]>(`/threads/${threadId}/turn-diffs`),
};

export const turnErrorApi = {
  list: (threadId: string) =>
    mtFetch<TurnErrorDto[]>(`/threads/${threadId}/turn-errors`),
};
export interface InvitationDto { id: string; code: string; created_at: number; expires_at: number; max_uses: number; used_count: number; }
