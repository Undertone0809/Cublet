/** Selection policy is independent of renderer state and never reads the filesystem. */
export type GrowthSnapshot = { id: string; startedAtMs: number; updatedAtMs: number };
export type GrowthHistory = { scopeId: string; current?: GrowthSnapshot; snapshots: GrowthSnapshot[] };
export type GrowthBaselineChoice =
  | { mode: 'previous' }
  | { mode: 'snapshot'; id: string; updatedAtMs?: number }
  | { mode: 'day'; dayKey?: string; anchor?: GrowthSnapshot };
export type GrowthPreferences = { version: 1; selections: { scopeId: string; choice: GrowthBaselineChoice }[] };
export const GROWTH_PREFERENCES_KEY = 'mac-storage-manager.growth-baseline.v1';

export function localDayKey(timestamp: number): string {
  const day = new Date(timestamp);
  return `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, '0')}-${String(day.getDate()).padStart(2, '0')}`;
}
export function localDayStart(timestamp: number): number {
  const day = new Date(timestamp);
  day.setHours(0, 0, 0, 0);
  return day.getTime();
}
function validSnapshot(value: unknown): value is GrowthSnapshot {
  if (!value || typeof value !== 'object') return false;
  const item = value as Partial<GrowthSnapshot>;
  return typeof item.id === 'string' && item.id.length > 0
    && Number.isFinite(item.startedAtMs) && Number.isFinite(item.updatedAtMs)
    && item.startedAtMs! > 0 && item.updatedAtMs! >= item.startedAtMs!;
}
/** An unsupported/failed history RPC must not be mistaken for an empty catalog. */
export function validateGrowthHistory(value: unknown): GrowthHistory {
  if (!value || typeof value !== 'object') throw new Error('Scan history is unavailable');
  const history = value as GrowthHistory;
  if (typeof history.scopeId !== 'string' || !history.scopeId || !Array.isArray(history.snapshots)
    || (history.current !== undefined && !validSnapshot(history.current))
    || !history.snapshots.every(validSnapshot)) throw new Error('Scan history is unavailable');
  return history;
}
function validChoice(value: unknown): value is GrowthBaselineChoice {
  if (!value || typeof value !== 'object') return false;
  const choice = value as GrowthBaselineChoice;
  if (choice.mode === 'previous') return true;
  if (choice.mode === 'snapshot') return typeof choice.id === 'string' && choice.id.length > 0
    && (choice.updatedAtMs === undefined || Number.isFinite(choice.updatedAtMs));
  return choice.mode === 'day' && (choice.dayKey === undefined || /^\d{4}-\d{2}-\d{2}$/.test(choice.dayKey))
    && (choice.anchor === undefined || validSnapshot(choice.anchor));
}
export function readGrowthPreferences(raw: string | null): GrowthPreferences {
  const empty: GrowthPreferences = { version: 1, selections: [] };
  try {
    const data = raw ? JSON.parse(raw) : undefined;
    if (data?.version !== 1 || !Array.isArray(data.selections)) return empty;
    return { version: 1, selections: data.selections.filter((item: { scopeId?: unknown; choice?: unknown }) =>
      item && typeof item.scopeId === 'string' && item.scopeId.length > 0 && validChoice(item.choice)).slice(-32) };
  } catch { return empty; }
}
export function growthPreferenceFor(state: GrowthPreferences, scopeId: string): GrowthBaselineChoice {
  return state.selections.find(item => item.scopeId === scopeId)?.choice ?? { mode: 'day' };
}
export function withGrowthPreference(state: GrowthPreferences, scopeId: string, choice: GrowthBaselineChoice): GrowthPreferences {
  return { version: 1, selections: [...state.selections.filter(item => item.scopeId !== scopeId), { scopeId, choice }].slice(-32) };
}
export type GrowthBaselineResolution = {
  status: 'ready' | 'no-current' | 'no-baseline' | 'awaiting-next' | 'baseline-unavailable';
  choice: GrowthBaselineChoice;
  current?: GrowthSnapshot;
  baseline?: GrowthSnapshot;
  firstOfDay?: boolean;
};
/** Endpoints are complete measurements. Day means the endpoint's local calendar
 * day, not a rolling 24-hour window. Pin the first choice until that day changes. */
export function resolveGrowthBaseline(history: GrowthHistory, choice: GrowthBaselineChoice = { mode: 'day' }): GrowthBaselineResolution {
  const current = history.current;
  if (!current) return { status: 'no-current', choice };
  const earlier = history.snapshots.filter(item => item.id !== current.id && item.updatedAtMs <= current.updatedAtMs)
    .sort((a, b) => b.updatedAtMs - a.updatedAtMs || b.startedAtMs - a.startedAtMs || a.id.localeCompare(b.id));
  let baseline: GrowthSnapshot | undefined;
  let resolvedChoice = choice;
  let firstOfDay = false;
  if (choice.mode === 'previous') baseline = earlier[0];
  else if (choice.mode === 'snapshot') {
    baseline = earlier.find(item => item.id === choice.id);
    if (!baseline) return { status: 'baseline-unavailable', choice, current };
  } else {
    const dayKey = localDayKey(current.updatedAtMs);
    const dayStart = localDayStart(current.updatedAtMs);
    if (choice.dayKey === dayKey && choice.anchor) {
      baseline = choice.anchor.id === current.id ? current : earlier.find(item => item.id === choice.anchor!.id);
      if (!baseline) return { status: 'baseline-unavailable', choice, current };
    } else {
      baseline = earlier.find(item => item.updatedAtMs < dayStart)
        ?? [...earlier, current].sort((a, b) => a.updatedAtMs - b.updatedAtMs || a.startedAtMs - b.startedAtMs || a.id.localeCompare(b.id))[0];
    }
    firstOfDay = baseline.updatedAtMs >= dayStart;
    resolvedChoice = { mode: 'day', dayKey, anchor: baseline };
  }
  return {
    status: !baseline ? 'no-baseline' : baseline.id === current.id ? 'awaiting-next' : 'ready',
    choice: resolvedChoice, current, baseline, firstOfDay,
  };
}
