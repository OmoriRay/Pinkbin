import type { RecommendedGroup } from './diskCleanerApi';

const key = 'pinkbin.recommended-selection.v1';
export function restoreRecommendedSelection(planId: string, groups: readonly RecommendedGroup[]) {
  const available = new Set(groups.map(group => group.id).filter(id => typeof id === 'string' && id.length > 0));
  try {
    const stored = JSON.parse(sessionStorage.getItem(key) || 'null');
    if (stored?.planId === planId) {
      return new Set<string>(Array.isArray(stored.ids) ? stored.ids.filter((id: unknown) => typeof id === 'string' && available.has(id)) : []);
    }
  } catch { return new Set<string>(); }
  return available;
}
export function saveRecommendedSelection(planId: string, ids: ReadonlySet<string>) {
  try { sessionStorage.setItem(key, JSON.stringify({ planId, ids: [...ids] })); return true; }
  catch { return false; }
}
export function selectedRecommendationTotals(groups: readonly RecommendedGroup[], ids: ReadonlySet<string>) {
  return groups.reduce((total, group) => ids.has(group.id) ? { files: total.files + group.file_count, bytes: total.bytes + group.bytes } : total, { files: 0, bytes: 0 });
}
