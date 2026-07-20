/**
 * 权限驱动 hook:基于 user-store(me) + team-store(currentTeamId)
 * 判定当前用户在当前团队内的权限/角色,供 UI 元素显隐使用。
 */
import { useMemo } from 'react';
import { useUserStore } from '@/stores/user-store';
import { useTeamStore } from '@/stores/team-store';
import type { TeamPermission, Role } from '@/lib/mt-client';

/** 当前用户在 currentTeam 是否持有 perm(仅按团队成员权限判断,不含平台管理员短路)。 */
export function usePermission(perm: TeamPermission): boolean {
  const me = useUserStore((s) => s.me);
  const currentTeamId = useTeamStore((s) => s.currentTeamId);
  return useMemo(() => {
    if (!me || !currentTeamId) return false;
    const m = me.teams.find((t) => t.team_id === currentTeamId);
    return m?.permissions.includes(perm) ?? false;
  }, [me, currentTeamId, perm]);
}

/** 当前用户是否为平台超级管理员。
 *  注意:仅对 require_platform_admin_layer 守护的全局路由生效(全局配置/全局日志/公共工作区写);
 *  team 级 require_permission 不绕过——平台管理员若非该 team 成员,team 操作仍 403。 */
export function useIsPlatformAdmin(): boolean {
  return useUserStore((s) => s.me?.is_platform_admin ?? false);
}

/** 当前用户在 currentTeam 的角色(无 team/非成员返回 null)。 */
export function useCurrentRole(): Role | null {
  const me = useUserStore((s) => s.me);
  const currentTeamId = useTeamStore((s) => s.currentTeamId);
  return useMemo(() => {
    if (!me || !currentTeamId) return null;
    return me.teams.find((t) => t.team_id === currentTeamId)?.role ?? null;
  }, [me, currentTeamId]);
}
