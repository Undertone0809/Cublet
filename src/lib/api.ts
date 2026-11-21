import type { CleanupOperationSummary, CleanupPlan, CleanupResult, CompactionPlan, CodexStatus, ManagedCleanupPlan, ManagedCleanupResult, StorageSnapshot, QuitCodexResult, WorktreeRemovalResult } from '../types';

import type { Worktree } from '../types';

function nativeInvoke<T>(command: string, args: Record<string, unknown> = {}): Promise<T> | undefined {
  if (typeof window === 'undefined') return undefined;
  if (window.electronAPI) return window.electronAPI.invoke<T>(command, args);
  return undefined;
}

export async function listWorktrees(): Promise<Worktree[]> {
  const native = nativeInvoke<Worktree[]>('list_worktrees');
  if (native) return native;
  return [
    { path: '~/.codex/worktrees/a/project', project: 'project', branch: 'feature/cleanup', head: 'demo', common_dir: '~/projects/project/.git', registration_id: '~/projects/project/.git/worktrees/a', bytes: 20480000, changed_files: 0, ignored_files: 0, created_at_ms: Date.now() - 45 * 86400000, updated_at_ms: Date.now() - 2 * 86400000, dirty: false, locked: false, main: false, can_remove: true, reason: '' },
    { path: '~/.codex/worktrees/b/project', project: 'project', branch: 'draft', head: 'demo2', common_dir: '~/projects/project/.git', registration_id: '~/projects/project/.git/worktrees/b', bytes: 40960000, changed_files: 4, ignored_files: 2, created_at_ms: Date.now() - 120 * 86400000, updated_at_ms: Date.now() - 90 * 86400000, dirty: true, locked: false, main: false, can_remove: true, reason: '' },
  ];
}
export async function removeWorktrees(worktrees: Worktree[], confirmed: boolean): Promise<WorktreeRemovalResult> {
  const native = nativeInvoke<WorktreeRemovalResult>('remove_worktrees', { worktrees, confirmed });
  if (native) return native;
  throw new Error('Demo only: no worktrees were deleted. Use the desktop app.');
}
export async function removeWorktree(worktree: Worktree): Promise<void> {
  const result = await removeWorktrees([worktree], true);
  if (result.failed.length > 0) throw new Error(result.failed.join('; '));
}
export async function purgeRecovery(operationId: string, confirmation: string): Promise<number> {
  const native = nativeInvoke<number>('purge_recovery', { operationId, confirmation });
  if (native) return native;
  throw new Error('Demo only: no recovery files were deleted.');
}

const mockConversationReclaimableBytes = (479.1 + 126.4 + 88.7 + 74.2 + 61.8) * 1024 ** 2;
const mockDatabaseReclaimableBytes = 15.6 * 1024 ** 3;
const mockManagedReclaimableBytes = 3 * 1024 ** 3 + 500 * 1024 ** 2 + 17 * 1024 ** 2;
const demoMonth = (offset: number) => new Date(new Date().getFullYear(), new Date().getMonth() + offset, 15, 12).getTime();
let demoCodexRunning = true;
const demoManagedOperations: ManagedCleanupPlan[] = [];
const demoManagedItems: Record<string, { id: string; label: string; path: string; bytes: number; file_count: number; modified_ms: number; safety: string }> = {
  'cache-browser': { id: 'cache-browser', label: 'Browser cache', path: '~/.codex/cache/browser', bytes: 9.4 * 1024 ** 2, file_count: 214, modified_ms: Date.now() - 4 * 86400000, safety: 'managed:rebuildable' },
  'cache-tmp': { id: 'cache-tmp', label: 'Temporary files', path: '~/.codex/.tmp', bytes: 7.6 * 1024 ** 2, file_count: 366, modified_ms: Date.now() - 2 * 86400000, safety: 'managed:rebuildable' },
  'automation-runs': { id: 'automation-runs', label: 'Completed automation runs', path: '~/.codex/automations/runs', bytes: 3 * 1024 ** 3, file_count: 1098, modified_ms: Date.now() - 9 * 86400000, safety: 'managed:generated output' },
  'plugin-vendor': { id: 'plugin-vendor', label: 'Plugin vendor imports', path: '~/.codex/vendor_imports', bytes: 310 * 1024 ** 2, file_count: 740, modified_ms: Date.now() - 16 * 86400000, safety: 'managed:reinstallable' },
  'skills-cache': { id: 'skills-cache', label: 'Skill package cache', path: '~/.codex/skills', bytes: 190 * 1024 ** 2, file_count: 380, modified_ms: Date.now() - 11 * 86400000, safety: 'managed:reinstallable' },
};

const mockSnapshot: StorageSnapshot = {
  scanned_at: new Date().toISOString(),
  codex_home: '~/.codex',
  codex_running: demoCodexRunning,
  total_bytes: 83 * 1024 ** 3,
  file_count: 24_810,
  reclaimable_bytes: mockDatabaseReclaimableBytes + mockConversationReclaimableBytes + mockManagedReclaimableBytes,
  categories: [
    { id: 'external-conversations', label: '外部对话记录', bytes: 1024 ** 3, file_count: 1, reclaimable_bytes: 0, safety: 'protected:仅统计' },
    { id: 'conversations', label: '对话', bytes: 55.6 * 1024 ** 3, file_count: 13_523, reclaimable_bytes: mockConversationReclaimableBytes, safety: 'managed:可管理' },
    { id: 'databases', label: '数据库', bytes: 16.4 * 1024 ** 3, file_count: 11, reclaimable_bytes: mockDatabaseReclaimableBytes, safety: 'conditional:退出 Codex 后' },
    { id: 'worktrees', label: 'Worktrees', bytes: 3.2 * 1024 ** 3, file_count: 11_822, reclaimable_bytes: 0, safety: 'protected:仅检查' },
    { id: 'automations', label: '自动化产物', bytes: 3 * 1024 ** 3, file_count: 1_098, reclaimable_bytes: 3 * 1024 ** 3, safety: 'managed:可管理' },
    { id: 'media', label: '图片与附件', bytes: 180 * 1024 ** 2, file_count: 2_240, reclaimable_bytes: 0, safety: 'protected:仅检查' },
    { id: 'extensions', label: '插件与技能', bytes: 500 * 1024 ** 2, file_count: 1_120, reclaimable_bytes: 500 * 1024 ** 2, safety: 'managed:可管理' },
    { id: 'cache', label: '缓存', bytes: 17 * 1024 ** 2, file_count: 580, reclaimable_bytes: 17 * 1024 ** 2, safety: 'managed:可管理' },
  ],
  conversations: [
    { id: 'demo-external', title: 'External agent record', cwd: '~/projects/example', status: 'archived', updated_at_ms: demoMonth(-1), bytes: 1024 ** 3, external: true, missing_file: false, rollout_path: '~/Library/Application Support/Example/agents/sessions/external.jsonl' },
    { id: 'demo-1', title: 'Rudder 产品一致性审计', cwd: '~/projects/rudder-oss', status: 'current', updated_at_ms: Date.now() - 5 * 60_000, bytes: 479.1 * 1024 ** 2, external: false, missing_file: false, rollout_path: '~/.codex/sessions/2026/08/28/demo-1.jsonl' },
    { id: 'demo-2', title: '每日性能评估', cwd: '~/projects/rudder-oss', status: 'archived', updated_at_ms: demoMonth(-1), bytes: 126.4 * 1024 ** 2, external: false, missing_file: false, rollout_path: '~/.codex/archived_sessions/demo-2.jsonl' },
    { id: 'demo-3', title: '插件发现与验证', cwd: '~/projects/rudder-oss', status: 'archived', updated_at_ms: demoMonth(-2), bytes: 88.7 * 1024 ** 2, external: false, missing_file: false, rollout_path: '~/.codex/archived_sessions/demo-3.jsonl' },
    { id: 'demo-4', title: 'Flomo 月度整理', cwd: '~/projects/uranus', status: 'current', updated_at_ms: Date.now() - 3 * 60 * 60_000, bytes: 74.2 * 1024 ** 2, external: false, missing_file: false, rollout_path: '~/.codex/sessions/2026/08/28/demo-4.jsonl' },
    { id: 'demo-5', title: 'Release 0.7.13', cwd: '~/projects/rudder-oss', status: 'archived', updated_at_ms: demoMonth(-1), bytes: 61.8 * 1024 ** 2, external: false, missing_file: false, rollout_path: '~/.codex/archived_sessions/demo-5.jsonl' },
    { id: 'demo-6', title: 'Missing record example', cwd: '~/projects/example', status: 'missing', updated_at_ms: null, bytes: 0, external: false, missing_file: true, rollout_path: '~/.codex/sessions/missing.jsonl' },
  ],
  warnings: ['演示模式：桌面运行时不可用，使用本地样例数据'],
};

// Keep preview totals consistent with the category breakdown.
mockSnapshot.total_bytes = mockSnapshot.categories.reduce((sum, category) => sum + category.bytes, 0);
mockSnapshot.file_count = mockSnapshot.categories.reduce((sum, category) => sum + category.file_count, 0);

// Browser previews use a long synthetic list to exercise desktop scrolling.
mockSnapshot.conversations.push(...Array.from({ length: 120 }, (_, index) => ({ ...mockSnapshot.conversations[2], id: `sample-${index}`, title: `Sample conversation ${index + 1}`, bytes: (index + 1) * 16384, updated_at_ms: demoMonth(-(index % 12)), rollout_path: `~/.codex/archived_sessions/sample-${index}.jsonl` })));

export async function scanStorage(): Promise<StorageSnapshot> {
  const native = nativeInvoke<StorageSnapshot>('scan_storage_fast');
  if (native) return native;
  await new Promise((resolve) => window.setTimeout(resolve, 320));
  return { ...mockSnapshot, codex_running: demoCodexRunning, scanned_at: new Date().toISOString() };
}

export async function getCodexStatus(): Promise<CodexStatus> {
  const native = nativeInvoke<CodexStatus>('get_codex_status');
  if (native) return native;
  return { running: demoCodexRunning, process_count: demoCodexRunning ? 1 : 0, app_process_count: demoCodexRunning ? 1 : 0, cli_process_count: 0 };
}

export async function quitCodex(): Promise<QuitCodexResult> {
  const native = nativeInvoke<QuitCodexResult>('quit_codex');
  if (native) return native;
  demoCodexRunning = false;
  return { requested: true, still_running: false, message: 'Demo Codex process closed.' };
}

export async function createCleanupPlan(threadIds: string[], selectionSummary?: string): Promise<CleanupPlan> {
  const native = nativeInvoke<CleanupPlan>('create_cleanup_plan', { threadIds });
  if (native) {
    const plan = await native;
    return { ...plan, selection_summary: selectionSummary };
  }
  const entries = mockSnapshot.conversations.filter((conversation) => threadIds.includes(conversation.id)).map((conversation) => ({
    thread_id: conversation.id,
    title: conversation.title,
    rollout_path: conversation.rollout_path,
    fingerprint: conversation.missing_file ? null : { path: conversation.rollout_path, size: conversation.bytes, modified_ms: conversation.updated_at_ms ?? 0, inode: null },
    bytes: conversation.bytes,
  }));
  return {
    operation_id: `demo-cleanup-${Date.now()}`,
    selection_summary: selectionSummary,
    codex_home: mockSnapshot.codex_home,
    created_at: new Date().toISOString(),
    thread_ids: threadIds,
    entries,
    database_fingerprints: [],
    estimated_bytes: entries.reduce((sum, entry) => sum + entry.bytes, 0),
  };
}

export async function executeCleanupPlan(plan: CleanupPlan): Promise<CleanupResult> {
  const native = nativeInvoke<CleanupResult>('execute_cleanup_plan', { plan });
  if (native) return native;
  throw new Error('破坏性清理仅可在桌面应用中执行');
}

export async function restoreCleanupOperation(operationId: string): Promise<CleanupResult> {
  const native = nativeInvoke<CleanupResult>('restore_cleanup_operation', { operationId });
  if (native) return native;
  throw new Error('恢复操作仅可在桌面应用中执行');
}

export async function restoreCompactionOperation(operationId: string): Promise<CleanupResult> {
  const native = nativeInvoke<CleanupResult>('restore_compaction_operation', { operationId });
  if (native) return native;
  throw new Error('恢复操作仅可在桌面应用中执行');
}

export async function createCompactionPlan(): Promise<CompactionPlan> {
  const native = nativeInvoke<CompactionPlan>('create_compaction_plan');
  if (native) return native;
  return { operation_id: `demo-compact-${Date.now()}`, codex_home: mockSnapshot.codex_home, created_at: new Date().toISOString(), databases: [], fingerprints: [], estimated_bytes: 0 };
}

export async function executeCompactionPlan(plan: CompactionPlan): Promise<CompactionPlan> {
  const native = nativeInvoke<CompactionPlan>('execute_compaction_plan', { plan });
  if (native) return native;
  throw new Error('数据库压缩仅可在桌面应用中执行');
}

export async function listCleanupOperations(): Promise<CleanupOperationSummary[]> {
  const native = nativeInvoke<CleanupOperationSummary[]>('list_cleanup_operations');
  if (native) return native;
  return demoManagedOperations.map((plan) => ({ operation_id: plan.operation_id, operation_type: 'managed', created_at: plan.created_at, status: 'completed', thread_count: plan.entries.length, database_count: 0, reclaimed_bytes: plan.estimated_bytes }));
}

export async function listTemporaryCleanupOperations(): Promise<CleanupOperationSummary[]> {
  const operations = await nativeInvoke<{
    operation_id: string;
    created_at: string;
    status: string;
    item_count: number;
    moved_bytes: number;
  }[]>('list_live_cleanup_operations');
  return (operations ?? []).map(({ item_count, moved_bytes, ...operation }) => ({
    ...operation,
    operation_type: 'temporary',
    thread_count: item_count,
    database_count: 0,
    reclaimed_bytes: moved_bytes,
  }));
}

export async function restoreTemporaryCleanupOperation(operationId: string): Promise<void> {
  const native = nativeInvoke('restore_live_cleanup_operation', { operationId });
  if (native) { await native; return; }
  throw new Error('Demo only: no files were restored.');
}

export async function purgeTemporaryCleanupOperation(operationId: string): Promise<number> {
  const native = nativeInvoke<{ freed_bytes: number }>('purge_live_cleanup_operation', { operationId, confirmation: operationId });
  if (native) return (await native).freed_bytes;
  throw new Error('Demo only: no recovery files were deleted.');
}

export async function createManagedCleanupPlan(categoryId: string, itemIds: string[] = []): Promise<ManagedCleanupPlan> {
  const native = nativeInvoke<ManagedCleanupPlan>('create_managed_cleanup_plan', { categoryId, itemIds: itemIds.length > 0 ? itemIds : null });
  if (native) return native;
  const categoryItems = Object.values(demoManagedItems).filter((item) => {
    if (categoryId === 'cache') return item.id.startsWith('cache-');
    if (categoryId === 'automations') return item.id.startsWith('automation-');
    if (categoryId === 'extensions') return item.id.startsWith('plugin-') || item.id.startsWith('skills-');
    return false;
  }).filter((item) => itemIds.length === 0 || itemIds.includes(item.id));
  const entries = categoryItems.map((item) => ({ ...item, category_id: categoryId, modified_ms: item.modified_ms, can_remove: true }));
  return { operation_id: `demo-managed-${Date.now()}`, codex_home: mockSnapshot.codex_home, created_at: new Date().toISOString(), category_id: categoryId, entries, estimated_bytes: entries.reduce((sum, item) => sum + item.bytes, 0) };
}

export async function executeManagedCleanupPlan(plan: ManagedCleanupPlan): Promise<ManagedCleanupResult> {
  const native = nativeInvoke<ManagedCleanupResult>('execute_managed_cleanup_plan', { plan });
  if (native) return native;
  if (demoCodexRunning) throw new Error('Please close Codex before managing local files.');
  const removed = new Set(plan.entries.map((entry) => entry.id));
  demoManagedOperations.push(plan);
  plan.entries.forEach((entry) => { delete demoManagedItems[entry.id]; });
  const category = mockSnapshot.categories.find((item) => item.id === plan.category_id);
  if (category) { category.bytes = Math.max(0, category.bytes - plan.estimated_bytes); category.reclaimable_bytes = Math.max(0, category.reclaimable_bytes - plan.estimated_bytes); category.file_count = Math.max(0, category.file_count - plan.entries.reduce((sum, item) => sum + item.file_count, 0)); }
  mockSnapshot.total_bytes = Math.max(0, mockSnapshot.total_bytes - plan.estimated_bytes);
  return { operation_id: plan.operation_id, moved_items: removed.size, reclaimed_bytes: plan.estimated_bytes, warnings: [] };
}

export async function restoreManagedCleanupOperation(operationId: string): Promise<ManagedCleanupResult> {
  const native = nativeInvoke<ManagedCleanupResult>('restore_managed_cleanup_operation', { operationId });
  if (native) return native;
  const plan = demoManagedOperations.find((item) => item.operation_id === operationId);
  if (!plan) throw new Error('Managed operation was not found.');
  plan.entries.forEach((entry) => {
    if (!demoManagedItems[entry.id]) demoManagedItems[entry.id] = { id: entry.id, label: entry.label, path: entry.path, bytes: entry.bytes, file_count: entry.file_count, modified_ms: entry.modified_ms ?? Date.now(), safety: entry.safety };
  });
  const category = mockSnapshot.categories.find((item) => item.id === plan.category_id);
  if (category) { category.bytes += plan.estimated_bytes; category.reclaimable_bytes += plan.estimated_bytes; category.file_count += plan.entries.reduce((sum, item) => sum + item.file_count, 0); }
  mockSnapshot.total_bytes += plan.estimated_bytes;
  return { operation_id: operationId, moved_items: plan.entries.length, reclaimed_bytes: 0, warnings: [] };
}
