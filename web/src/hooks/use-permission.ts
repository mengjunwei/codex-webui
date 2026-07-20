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

/** 当前用户是否为平台超级管理员(绕过所有团队权限检查)。 */
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
