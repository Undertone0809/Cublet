import type { Conversation } from '../types';

export function monthKey(value: number | null): string | null {
  if (value === null || !Number.isFinite(value)) return null;
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return null;
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}`;
}

export function monthLabel(key: string | null, locale: 'en' | 'zh'): string {
  if (!key) return locale === 'en' ? 'Unknown date' : '日期未知';
  const [year, month] = key.split('-').map(Number);
  return new Date(year, month - 1, 15).toLocaleDateString(locale === 'en' ? 'en-US' : 'zh-CN', { year: 'numeric', month: 'long' });
}

export function groupConversations(conversations: Conversation[]) {
  const groups = new Map<string | null, Conversation[]>();
  for (const conversation of conversations) {
    const key = monthKey(conversation.updated_at_ms);
    const items = groups.get(key);
    if (items) items.push(conversation); else groups.set(key, [conversation]);
  }
  return [...groups].sort(([a], [b]) => a === null ? 1 : b === null ? -1 : b.localeCompare(a)).map(([key, items]) => ({ key, items }));
}

export function monthBulkCandidates(conversations: Conversation[], includeCurrent: boolean) {
  return conversations.filter((item) => !item.external && monthKey(item.updated_at_ms) !== null && (includeCurrent || item.status === 'archived'));
}

export function toggleSelection(selected: Set<string>, items: Conversation[]) {
  const next = new Set(selected);
  const remove = items.length > 0 && items.every((item) => next.has(item.id));
  for (const item of items) { if (remove) next.delete(item.id); else next.add(item.id); }
  return next;
}

export function selectionSummary(items: Conversation[], locale: 'en' | 'zh') {
  const labels = groupConversations(items).map(({ key }) => monthLabel(key, locale));
  return `${locale === 'en' ? 'Last activity' : '最近活动'}: ${labels.join(' · ')} (${Intl.DateTimeFormat().resolvedOptions().timeZone})`;
}
