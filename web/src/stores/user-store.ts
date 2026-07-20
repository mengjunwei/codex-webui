/**
 * 当前用户身份 store(zustand):存 GET /api/mt/me 响应
 * (user + is_platform_admin + 各 team 角色/权限)。
 *
 * 供 usePermission / useIsPlatformAdmin / useCurrentRole hook 驱动 UI 显隐。
 * 登录成功后或应用挂载时调用 loadMe();登出时调用 clearMe()。
 */
import { create } from 'zustand';
import { authApi, type MeResponse } from '@/lib/mt-client';

interface UserStore {
  me: MeResponse | null;
  loading: boolean;
  error: string | null;
  /** 拉取 /api/mt/me 并写入 store;失败时写 error,不抛出。 */
  loadMe: () => Promise<void>;
  /** 清空身份(用于登出)。 */
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
      set({
        loading: false,
        error: e instanceof Error ? e.message : 'failed to load user',
      });
    }
  },
  clearMe: () => set({ me: null, loading: false, error: null }),
}));
