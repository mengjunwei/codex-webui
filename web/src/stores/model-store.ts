/**
 * Zustand store for session-level model and reasoning effort overrides.
 * These are applied per-turn via turn/start params.
 */
import { create } from 'zustand';

type ReasoningEffort = 'none' | 'minimal' | 'low' | 'medium' | 'high' | 'xhigh';

interface ModelState {
  /** Overridden model id — null means use the server default. */
  modelOverride: string | null;
  /** Overridden reasoning effort — null means use model default. */
  effortOverride: ReasoningEffort | null;

  setModelOverride: (model: string | null) => void;
  setEffortOverride: (effort: ReasoningEffort | null) => void;
  clearOverrides: () => void;
}

export const useModelStore = create<ModelState>((set) => ({
  modelOverride: null,
  effortOverride: null,

  setModelOverride: (model) => set({ modelOverride: model }),
  setEffortOverride: (effort) => set({ effortOverride: effort }),
  clearOverrides: () => set({ modelOverride: null, effortOverride: null }),
}));
