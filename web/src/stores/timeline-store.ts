/**
 * Zustand store for chat timeline state.
 * Manages threads, turns, items, and their streaming updates.
 */
import { create } from 'zustand';
import { api } from '../api';
import { getSocket } from '../socket';
import type { TimelineEntry, TurnItem } from '../types/timeline';

interface TimelineState {
  threadId: string | null;
  timeline: TimelineEntry[];
  loading: boolean;
  expandedReasoning: Set<string>;

  createThread: () => Promise<void>;
  sendMessage: (text: string) => Promise<void>;
  toggleReasoning: (itemId: string) => void;

  /** Internal: update or create a turn entry in the timeline. */
  updateCurrentTurn: (
    turnId: string,
    updater: (
      items: TurnItem[],
      completed: boolean,
    ) => { items: TurnItem[]; completed: boolean },
  ) => void;

  /** Internal: update a specific item within a turn. */
  updateTurnItem: (
    turnId: string,
    itemId: string,
    updater: (existing: TurnItem | undefined) => TurnItem,
  ) => void;

  setLoading: (loading: boolean) => void;
  expandReasoning: (itemId: string) => void;
  collapseReasoning: (itemId: string) => void;
}

export const useTimelineStore = create<TimelineState>((set, get) => ({
  threadId: null,
  timeline: [],
  loading: false,
  expandedReasoning: new Set<string>(),

  createThread: async () => {
    try {
      const res = await api.createThread({});
      set({ threadId: res.thread.id, timeline: [] });
      getSocket().emit('thread.subscribe', { threadId: res.thread.id });
    } catch (err) {
      set((s) => ({
        timeline: [
          ...s.timeline,
          {
            kind: 'system' as const,
            content: `Error: ${(err as Error).message}`,
          },
        ],
      }));
    }
  },

  sendMessage: async (text: string) => {
    const { threadId, loading } = get();
    if (!threadId || !text.trim() || loading) return;

    set((s) => ({
      timeline: [...s.timeline, { kind: 'user' as const, content: text }],
      loading: true,
    }));

    try {
      await api.sendMessage(threadId, text);
    } catch (err) {
      set((s) => ({
        timeline: [
          ...s.timeline,
          {
            kind: 'system' as const,
            content: `Error: ${(err as Error).message}`,
          },
        ],
        loading: false,
      }));
    }
  },

  toggleReasoning: (itemId: string) => {
    set((s) => {
      const next = new Set(s.expandedReasoning);
      if (next.has(itemId)) next.delete(itemId);
      else next.add(itemId);
      return { expandedReasoning: next };
    });
  },

  updateCurrentTurn: (turnId, updater) => {
    set((s) => {
      const { timeline } = s;
      const last = timeline[timeline.length - 1];

      if (last?.kind === 'turn' && last.turnId === turnId) {
        const result = updater(last.items, last.completed);
        return {
          timeline: [
            ...timeline.slice(0, -1),
            { ...last, items: result.items, completed: result.completed },
          ],
        };
      }

      const result = updater([], false);
      return {
        timeline: [
          ...timeline,
          { kind: 'turn' as const, turnId, ...result },
        ],
      };
    });
  },

  updateTurnItem: (turnId, itemId, updater) => {
    get().updateCurrentTurn(turnId, (items, completed) => {
      const idx = items.findIndex((it) => it.itemId === itemId);
      if (idx >= 0) {
        const updated = [...items];
        updated[idx] = updater(updated[idx]);
        return { items: updated, completed };
      }
      return { items: [...items, updater(undefined)], completed };
    });
  },

  setLoading: (loading: boolean) => set({ loading }),

  expandReasoning: (itemId: string) => {
    set((s) => ({
      expandedReasoning: new Set(s.expandedReasoning).add(itemId),
    }));
  },

  collapseReasoning: (itemId: string) => {
    set((s) => {
      const next = new Set(s.expandedReasoning);
      next.delete(itemId);
      return { expandedReasoning: next };
    });
  },
}));
