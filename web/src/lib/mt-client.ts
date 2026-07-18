/**
 * 多租户 API 客户端（手写）。
 * 
 * 用于调用后端 /api/mt/* 端点（team 管理、threads、approvals 等）。
 * 这些端点不在 OpenAPI SDK 生成中（utoipa 注解待补全）。
 */

import { getApiToken } from '../auth-token';

// 类型定义
export interface CreateTeamRequest {
  name: string;
}

export interface TeamDto {
  id: string;
  name: string;
  owner_id: string;
  created_at: number;
  updated_at: number;
}

export interface ListTeamsResponse {
  teams: TeamDto[];
}

export interface CreateInvitationRequest {
  max_uses: number;
  expires_hours: number;
}

export interface InvitationDto {
  id: string;
  code: string;
  created_at: number;
  expires_at: number;
  max_uses: number;
  used_count: number;
}

export interface TeamMemberDto {
  user_id: string;
  role: string;
  joined_at: number;
}

export interface CreateThreadRequest {
  team_id: string;
}

export interface StartTurnRequest {
  threadId: string;
  team_id: string;
  [key: string]: unknown;
}

export interface InvokeThreadRequest {
  method: string;
  params?: unknown;
}

export interface PendingApproval {
  generation: number;
  request_id: string;
  thread_id: string;
  turn_id?: string;
  item_id?: string;
  method: string;
  params_json: string;
  status: string;
  created_at: number;
}

export interface ApprovalResponse {
  approved: boolean;
  result?: unknown;
}

export interface TokenUsageDto {
  thread_id: string;
  turn_id: string;
  total_tokens: number;
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
  reasoning_output_tokens: number;
  model_context_window?: number;
  updated_at: number;
}

export interface TurnDiffDto {
  thread_id: string;
  turn_id: string;
  diff: string;
  updated_at: number;
}

export interface TurnErrorDto {
  thread_id: string;
  turn_id: string;
  message: string;
  created_at: number;
}

export interface AuditEntry {
  id: number;
  team_id?: string;
  user_id?: string;
  thread_id?: string;
  event_type: string;
  tool_name?: string;
  payload_json: string;
  decision?: string;
  created_at: number;
}

export interface SetApiKeyRequest {
  api_key: string;
  provider: string;
}

export interface TeamApiKey {
  id: string;
  team_id: string;
  provider: string;
  key_hint: string;
  set_by: string;
  is_active: boolean;
  created_at: number;
}

export interface RegisterRequest {
  email: string;
  password: string;
  display_name?: string;
}

export interface LoginRequest {
  email: string;
  password: string;
}

export interface RefreshRequest {
  refresh_token: string;
}

export interface AuthTokens {
  access_token: string;
  refresh_token: string;
  token_type: string;
  expires_in: number;
}

export interface PaginatedResponse<T> {
  items: T[];
  total: number;
}

// 工具函数
function authHeaders(): Record<string, string> {
  const token = getApiToken();
  return token ? { 'Authorization': `Bearer ${token}` } : {};
}

async function mtFetch<T>(
  path: string,
  method: string = 'GET',
  body?: unknown
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
      `API error: ${response.status} ${response.statusText}`
    );
  }

  return response.json();
}

// 认证 API
export const authApi = {
  register: (data: RegisterRequest) =>
    mtFetch<AuthTokens>('/auth/register', 'POST', data),

  login: (data: LoginRequest) =>
    mtFetch<AuthTokens>('/auth/login', 'POST', data),

  refresh: (data: RefreshRequest) =>
    mtFetch<AuthTokens>('/auth/refresh', 'POST', data),
};

// Team API
export const teamsApi = {
  create: (data: CreateTeamRequest) =>
    mtFetch<TeamDto>('/teams', 'POST', data),

  list: () =>
    mtFetch<ListTeamsResponse>('/teams'),

  getMembers: (teamId: string) =>
    mtFetch<TeamMemberDto[]>(`/teams/${teamId}/members`),

  removeMember: (teamId: string, userId: string) =>
    mtFetch(`/teams/${teamId}/members/${userId}`, 'DELETE'),

  createInvitation: (teamId: string, data: CreateInvitationRequest) =>
    mtFetch<InvitationDto>(`/teams/${teamId}/invitations`, 'POST', data),

  joinWithCode: (code: string) =>
    mtFetch<TeamDto>('/teams/join', 'POST', { code }),

  setApiKey: (teamId: string, data: SetApiKeyRequest) =>
    mtFetch<TeamApiKey>(`/teams/${teamId}/api-key`, 'POST', data),

  listApiKeys: (teamId: string) =>
    mtFetch<TeamApiKey[]>(`/teams/${teamId}/api-key`),

  listAudit: (teamId: string, limit?: number) =>
    mtFetch<PaginatedResponse<AuditEntry>>(`/threads/team/${teamId}/audit?limit=${limit || 100}`),
};

export interface ThreadListFilters {
  archived?: boolean;
  limit?: number;
  sortKey?: string;
  cursor?: string;
  cwd?: string;
}

// Threads API
export const threadsApi = {
  create: (data: CreateThreadRequest): Promise<any> =>
    mtFetch('/threads', 'POST', data),

  list: (teamId: string, filters?: ThreadListFilters): Promise<any[]> => {
    const params = new URLSearchParams({ team_id: teamId });
    if (filters) {
      if (filters.archived !== undefined) params.set('archived', String(filters.archived));
      if (filters.limit !== undefined) params.set('limit', String(filters.limit));
      if (filters.sortKey) params.set('sortKey', filters.sortKey);
      if (filters.cursor) params.set('cursor', filters.cursor);
      if (filters.cwd) params.set('cwd', filters.cwd);
    }
    return mtFetch<any[]>(`/threads?${params.toString()}`);
  },

  startTurn: (threadId: string, data: StartTurnRequest): Promise<any> =>
    mtFetch(`/threads/${threadId}/turns`, 'POST', data),

  invoke: (threadId: string, data: InvokeThreadRequest): Promise<any> =>
    mtFetch(`/threads/${threadId}/invoke`, 'POST', data),

  rollback: (threadId: string, numTurns: number): Promise<any> =>
    mtFetch(`/threads/${threadId}/rollback`, 'POST', { numTurns }),

  archive: (threadId: string): Promise<any> =>
    mtFetch(`/threads/${threadId}/archive`, 'POST'),

  rename: (threadId: string, name: string): Promise<any> =>
    mtFetch(`/threads/${threadId}/rename`, 'POST', { name }),
};

// Approvals API
export const approvalsApi = {
  list: (threadId: string, status?: string) =>
    mtFetch<PaginatedResponse<PendingApproval>>(
      `/threads/${threadId}/approvals?status=${status || 'pending'}`
    ),

  respond: (threadId: string, requestId: string, data: ApprovalResponse) =>
    mtFetch(`/threads/${threadId}/approvals/${requestId}`, 'POST', data),
};

// Token Usage API
export const tokenUsageApi = {
  getLatest: (threadId: string) =>
    mtFetch<TokenUsageDto>(`/threads/${threadId}/token-usage`),

  /** 获取线程所有 turn 的 token 使用量（数组），等价于 list */
  get: (threadId: string) =>
    mtFetch<TokenUsageDto[]>(`/threads/${threadId}/token-usage/list`),

  list: (threadId: string) =>
    mtFetch<TokenUsageDto[]>(`/threads/${threadId}/token-usage/list`),
};

// Turn Diff API
export const turnDiffApi = {
  list: (threadId: string) =>
    mtFetch<TurnDiffDto[]>(`/threads/${threadId}/turn-diffs`),
};

// Turn Error API
export const turnErrorApi = {
  list: (threadId: string) =>
    mtFetch<TurnErrorDto[]>(`/threads/${threadId}/turn-errors`),
};
