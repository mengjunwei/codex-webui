/**
 * Minimal REST API client for Codex WebUI backend.
 * Will be replaced by Hey API generated SDK later.
 */

const BASE = '/api';

async function request<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    headers: { 'Content-Type': 'application/json' },
    ...options,
  });
  if (!res.ok) {
    throw new Error(`API error ${res.status}: ${await res.text()}`);
  }
  return res.json() as Promise<T>;
}

export interface Thread {
  id: string;
  name?: string | null;
  preview?: string;
  createdAt?: number;
  updatedAt?: number;
}

export interface ThreadStartResponse {
  thread: Thread;
  model: string;
}

export interface ThreadListResponse {
  data: Thread[];
  nextCursor: string | null;
}

export interface TurnItemData {
  type: string;
  id: string;
  text?: string;
  content?: Array<{ type: string; text?: string }>;
  summary?: string[];
  phase?: string;
  server?: string;
  tool?: string;
  arguments?: unknown;
  result?: unknown;
}

export interface Turn {
  id: string;
  items?: TurnItemData[];
  status?: string;
}

export interface ThreadWithTurns extends Thread {
  turns?: Turn[];
}

export interface ThreadResumeResponse {
  thread: ThreadWithTurns;
  model: string;
}

export interface TurnStartResponse {
  turn: Turn;
}

export interface Model {
  id: string;
  name?: string;
}

export interface ModelListResponse {
  data: Model[];
  nextCursor: string | null;
}

export const api = {
  createThread(params: { model?: string; cwd?: string }) {
    return request<ThreadStartResponse>('/threads', {
      method: 'POST',
      body: JSON.stringify(params),
    });
  },

  listThreads() {
    return request<ThreadListResponse>('/threads');
  },

  readThread(threadId: string) {
    return request<{ thread: Thread }>(`/threads/${threadId}?includeTurns=true`);
  },

  sendMessage(threadId: string, text: string) {
    return request<TurnStartResponse>(`/threads/${threadId}/turns`, {
      method: 'POST',
      body: JSON.stringify({
        input: [{ type: 'text', text }],
      }),
    });
  },

  interruptTurn(threadId: string, turnId: string) {
    return request<{ ok: boolean }>(
      `/threads/${threadId}/turns/${turnId}/interrupt`,
      { method: 'POST' },
    );
  },

  resumeThread(threadId: string) {
    return request<ThreadResumeResponse>(`/threads/${threadId}/resume`, {
      method: 'POST',
      body: JSON.stringify({}),
    });
  },

  listModels() {
    return request<ModelListResponse>('/models');
  },
};
