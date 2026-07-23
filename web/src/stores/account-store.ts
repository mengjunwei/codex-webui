/**
 * Zustand store for realtime Codex account state.
 *
 * TODO: AccountRateLimitsResponseDto / AccountReadResponseDto /
 *       LoginAccountResponseDto / RateLimitSnapshotDto 来自旧 OpenAPI SDK,已下线。
 *       当前用本地最小 any 别名,待新 mt-client API 注解补全后再恢复强类型。
 */
import { create } from 'zustand';
import type {
  AccountLoginCompletedNotification,
  AccountUpdatedNotification,
} from '@/types/account';

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AccountReadResponse = any;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type LoginAccountResponse = any;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AccountRateLimitsResponse = any;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type RateLimitSnapshot = any;

interface LoginState {
  inProgress: boolean;
  loginId: string | null;
  method: LoginAccountResponse['type'] | null;
  response: LoginAccountResponse | null;
  lastResult: AccountLoginCompletedNotification | null;
}

interface AccountState {
  account: AccountReadResponse | null;
  authMode: AccountUpdatedNotification['authMode'];
  planType: AccountUpdatedNotification['planType'];
  rateLimits: AccountRateLimitsResponse | null;
  login: LoginState;

  setAccount: (account: AccountReadResponse | null) => void;
  setAccountUpdated: (payload: AccountUpdatedNotification) => void;
  setRateLimits: (rateLimits: AccountRateLimitsResponse | null) => void;
  setRateLimitSnapshot: (snapshot: RateLimitSnapshot) => void;
  setLoginStarted: (response: LoginAccountResponse) => void;
  setLoginCompleted: (payload: AccountLoginCompletedNotification) => void;
  clearLogin: () => void;
}

const emptyLogin: LoginState = {
  inProgress: false,
  loginId: null,
  method: null,
  response: null,
  lastResult: null,
};

function loginIdFromResponse(response: LoginAccountResponse): string | null {
  return 'loginId' in response ? (response.loginId ?? null) : null;
}

export const useAccountStore = create<AccountState>((set, get) => ({
  account: null,
  authMode: null,
  planType: null,
  rateLimits: null,
  login: emptyLogin,

  setAccount: (account) =>
    set({
      account,
      authMode: account?.account?.type === 'chatgpt' ? 'chatgpt' : null,
      planType: account?.account?.type === 'chatgpt' ? account.account.planType : null,
    }),

  setAccountUpdated: (payload) =>
    set({ authMode: payload.authMode, planType: payload.planType }),

  setRateLimits: (rateLimits) => set({ rateLimits }),

  setRateLimitSnapshot: (snapshot) => {
    const existing = get().rateLimits;
    set({
      rateLimits: {
        rateLimits: snapshot,
        rateLimitsByLimitId: existing?.rateLimitsByLimitId ?? null,
      },
    });
  },

  setLoginStarted: (response) =>
    set({
      login: {
        inProgress: response.type === 'chatgpt' || response.type === 'chatgptDeviceCode',
        loginId: loginIdFromResponse(response),
        method: response.type,
        response,
        lastResult: null,
      },
    }),

  setLoginCompleted: (payload) =>
    set((state) => ({
      login: { ...state.login, inProgress: false, lastResult: payload },
    })),

  clearLogin: () => set({ login: emptyLogin }),
}));
