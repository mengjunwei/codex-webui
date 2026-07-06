/** Runtime parsers for approval-related protocol data. */
import type { NetworkPolicyAmendment, RawCommandDecision } from '@/types/approval';

const rawSimpleDecisions = new Set(['accept', 'acceptForSession', 'decline', 'cancel']);

/** Parses availableDecisions from raw socket/REST params with runtime validation. */
export function parseAvailableDecisions(value: unknown): RawCommandDecision[] | null {
  if (!Array.isArray(value)) return null;
  return value.filter((d): d is RawCommandDecision => {
    if (typeof d === 'string') return rawSimpleDecisions.has(d);
    return d !== null && typeof d === 'object' &&
      ('acceptWithExecpolicyAmendment' in d || 'applyNetworkPolicyAmendment' in d);
  });
}

/** Parses a value as a string array, filtering non-strings. */
export function parseStringArray(value: unknown): string[] | null {
  return Array.isArray(value) ? value.filter((s): s is string => typeof s === 'string') : null;
}

/** Parses network policy amendments with host/action validation. */
export function parseNetworkAmendments(value: unknown): NetworkPolicyAmendment[] | null {
  if (!Array.isArray(value)) return null;
  return value.filter((item): item is NetworkPolicyAmendment => {
    if (item === null || typeof item !== 'object') return false;
    const r = item as Record<string, unknown>;
    return typeof r.host === 'string' && (r.action === 'allow' || r.action === 'deny');
  });
}
