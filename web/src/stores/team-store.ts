/**
 * Team store (zustand) — manages current team context for multi-tenant mode.
 *
 * All /api/mt/* endpoints require teamId. This store provides
 * the current team ID for the authenticated user.
 */
import { create } from 'zustand';
import { teamsApi, type TeamDto } from '@/lib/mt-client';

interface TeamStore {
  teams: TeamDto[];
  currentTeamId: string | null;
  currentTeam: TeamDto | null;
  loading: boolean;
  error: string | null;

  loadTeams: () => Promise<void>;
  setCurrentTeam: (teamId: string) => void;
  createTeam: (name: string) => Promise<TeamDto>;
  joinTeam: (code: string) => Promise<TeamDto>;
  refreshCurrentTeam: () => void;
}

export const useTeamStore = create<TeamStore>((set, get) => ({
  teams: [],
  currentTeamId: null,
  currentTeam: null,
  loading: false,
  error: null,

  loadTeams: async () => {
    set({ loading: true, error: null });
    try {
      const data = await teamsApi.list();
      const teams = data.teams ?? [];
      set({ teams, loading: false });

      // Auto-select first team if none selected.
      const { currentTeamId } = get();
      if (!currentTeamId && teams.length > 0) {
        get().setCurrentTeam(teams[0].id);
      } else if (currentTeamId && !teams.some(t => t.id === currentTeamId)) {
        // Current team no longer exists — select first.
        get().setCurrentTeam(teams[0]?.id ?? null);
      }
    } catch (err) {
      set({
        error: err instanceof Error ? err.message : 'Failed to load teams',
        loading: false,
      });
    }
  },

  setCurrentTeam: (teamId: string | null) => {
    const team = get().teams.find(t => t.id === teamId) ?? null;
    set({ currentTeamId: teamId, currentTeam: team });
  },

  createTeam: async (name: string) => {
    const team = await teamsApi.create({ name });
    set(state => ({ teams: [...state.teams, team] }));
    get().setCurrentTeam(team.id);
    return team;
  },

  joinTeam: async (code: string) => {
    const team = await teamsApi.joinWithCode(code);
    set(state => ({ teams: [...state.teams, team] }));
    get().setCurrentTeam(team.id);
    return team;
  },

  refreshCurrentTeam: () => {
    const { currentTeamId, teams } = get();
    const team = teams.find(t => t.id === currentTeamId) ?? null;
    set({ currentTeam: team });
  },
}));
