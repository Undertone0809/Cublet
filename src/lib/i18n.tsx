import { version } from '../../package.json';
import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';

export type Locale = 'en' | 'zh';

const STORAGE_KEY = 'codex-storage-manager.locale.v1';

const messages = {
  en: {
    purgeAcknowledgment: 'I understand: permanent deletion cannot be undone.',
    cleanupChoices: "Choose what to clean",
    cleanupChoicesCopy: "Only conversations and individual Git worktrees can be cleaned here.",
    conversationOverviewCopy: "Select conversations individually or by month. Recovery copies stay until you clear them.",
    worktreeOverviewCopy: "Review each project, branch, path, age and disk size before removal.",
    otherAppData: "Other app data",
    otherAppDataCopy: "Storage by category, largest first. Read-only.",

    worktreeTitle: "Individual worktrees",
    worktreeSafetyCopy: "Only verified Git worktrees are shown. Uncommitted and ignored files will be discarded after confirmation; main worktrees and locked registrations stay protected.",
    noWorktrees: "No Git worktrees found in the Codex worktrees directory.",
    branch: "Branch",
    worktreeDirty: "Uncommitted changes",
    worktreeClean: "No uncommitted changes",
    worktreeChangedFiles: "Uncommitted files: {count}",
    worktreeIgnoredFiles: "Ignored files: {count}",
    worktreeCreatedAt: "Created {time}",
    worktreeUpdatedAt: "Last updated {time}",
    mainWorktree: "Main worktree",
    lockedWorktree: "Locked",
    deleteWorktree: "Delete worktree",
    deleteWorktrees: "Delete worktrees",
    selectRemovableWorktrees: "Select all removable worktrees",
    selectedWorktrees: "{count} selected",
    deleteSelectedWorktrees: "Delete selected ({count})",
    confirmDeleteWorktree: "Permanently delete this worktree",
    confirmDeleteWorktrees: "Permanently delete selected worktrees",
    worktreeDeleteCopy: "Git will permanently remove {count} selected worktree(s) and their registrations. Uncommitted and ignored files inside them will be discarded; branches are kept. This cannot be undone here.",
    worktreeDeleteWarning: "Uncommitted and ignored files will be permanently discarded. Review the paths and counts below before continuing.",
    deletingWorktree: "Removing with Git… Do not close this window.",
    worktreeRemoved: "Worktree removed. Git registration and path removal verified.",
    worktreesRemoved: "Removed {count} worktrees. Git registrations and paths verified.",
    worktreeRemovalPartial: "Removed {removed} worktrees; {failed} could not be removed. Rescan and review the remaining entries.",
    worktreeProtectedMain: "Main worktree is protected",
    worktreeProtectedLocked: "Locked worktree is protected",
    worktreeProtectedSymlink: "Symlink worktree is protected",
    worktreeProtectedRegistration: "Git registration is unavailable; review manually",
    worktreeUnknownState: "Unknown Worktree state; review manually",

    purgeRecovery: "Clear recovery copies",
    purgeCopy: "Permanently delete the recovery files and database backups for this conversation operation only. You will lose the ability to restore it. Acknowledge the irreversible deletion below. Other operations and live files are not deleted.",
    purgeConfirmation: "Type exact operation ID",
    confirmPurge: "Permanently clear recovery",
    purgeComplete: "Recovery copies permanently cleared ({bytes} allocated files removed). Actual free space can differ due to APFS snapshots or shared blocks.",
    purged: "Recovery permanently cleared",
    purging: "Permanent clear in progress / incomplete — retry to finish",
    appName: 'Cublet',
    mainNavigation: 'Main navigation',
    overview: 'Overview',
    conversations: 'Conversations',
    maintenance: 'Maintenance',
    activity: 'Recovery & history',
    activityShort: 'Recovery',
    settings: 'Settings',
    scanLocation: 'Scan location',
    localMode: `Local mode · ${version}`,
    scannedAt: 'Scanned at {time}',
    scanning: 'Scanning',
    localReadOnly: 'Local read-only',
    rescan: 'Rescan',
    retry: 'Retry',
    preparingScan: 'Preparing scan',
    unableToRead: 'Unable to read Codex local data. Try again.',
    readingLocalData: 'Reading Codex local data…',
    activityReadError: 'Activity log read failed: {error}',
    temporaryActivityReadError: 'Temporary file recovery log could not be read: {error}',
    totalStorage: 'Total storage',
    estimatedReclaimable: 'Estimated reclaimable',
    conversationStorage: 'Conversation storage',
    scannedFiles: 'Scanned {count} files',
    currentlyAttributable: 'Currently attributable',
    indexedRecords: '{count} indexed records',
    storageBreakdown: 'Storage breakdown',
    sortedByAllocated: 'Sorted by allocated disk space',
    reclaimableNow: 'Reclaimable now',
    actionable: 'Actionable',
    pendingReview: 'Review needed',
    archived: 'Archived',
    current: 'Current',
    missing: 'Missing file',
    databaseCompressionTitle: 'Log database can be compacted',
    databaseCompressionCopy: 'About {bytes} of free pages. Codex must be closed first.',
    viewPlan: 'Review database',

    databaseReadyCopy: 'About {bytes} of free pages are ready to reclaim.',

    files: '{count} files',
    categoryConversations: 'Conversations',
    categoryDatabases: 'Databases',
    categoryWorktrees: 'Worktrees',
    categoryAutomations: 'Automation artifacts',
    categoryMedia: 'Images & attachments',
    categoryExtensions: 'Plugins & skills',
    categoryCache: 'Codex cache & temporary files',
    categoryElectronSupport: 'Desktop app data',
    categoryElectronCache: 'Desktop app cache',
    categoryBackups: 'Backups',
    categoryOther: 'Uncategorized files',
    afterCodexExit: 'After Codex exits',
    futureVersion: 'Future version',
    inspectOnly: 'Inspect only',
    handleCarefully: 'Handle carefully',
    needsConfirmation: 'Needs confirmation',
    unknown: 'Unknown',
    searchPlaceholder: 'Search title or project',
    filterStatus: 'Filter by status',
    allStatuses: 'All statuses',
    sort: 'Sort',
    sortSize: 'Largest first',
    sortRecent: 'Most recent',
    selectCurrentList: 'Select current list',
    status: 'Status',
    recentActivity: 'Recent activity',
    size: 'Size',
    shownCount: 'Showing {shown} · {total} indexed',
    indexIssues: 'Index issues detected',
    externalRecord: 'External · read only',
    externalBadge: 'External',
    externalFilesCopy: '{count} indexed records · {files} available files counted',
    recordSizeTotal: '{bytes} summed record sizes',
    categoryExternal: 'External conversation files',
    warningCount: '{count} scan notices. Open Activity to review details.',
    selectedConversations: '{count} conversations selected',
    estimatedRelease: '{bytes} of selected records · recoverable deletion',
    conversationView: 'Conversation view',
    listView: 'List',
    monthView: 'By month',
    monthSelected: '{count} of {total} selected',
    monthCount: '{count} conversations',
    selectMonth: 'Select',
    deselectMonth: 'Deselect',
    unknownManualOnly: 'Select individually',
    noMatchingConversations: 'No conversations match these filters.',
    hiddenSelected: '{count} selected conversations are hidden by filters',
    clearSelection: 'Clear selection',
    planSelectionDetails: 'Selection: {current} current · {missing} missing files',
    planMissingCopy: 'Missing files: only their index records will be removed; no file space is attributed to them.',
    planCurrentCopy: 'Current conversations are included. Check the titles below before deleting.',
    createCleanupPlan: 'Delete',
    maintenanceTitle: 'Maintenance checks',
    maintenanceCopy: 'Only actions with clear ownership are shown. Database compaction and managed file changes require Codex to be closed.',
    compactDatabase: 'Compact log database',
    compactEstimate: 'Estimated recovery: {bytes} of free pages',
    afterCodexExitBadge: 'After Codex exits',
    inspectWorktrees: 'Inspect worktrees',
    worktreeCopy: '{count} files may include uncommitted code',
    inspectionOnly: 'Inspect only',
    manage: 'Manage',
    manageCache: 'Manage cache',
    manageAutomations: 'Manage automation artifacts',
    manageExtensions: 'Manage plugins & skills',
    managedCopy: '{bytes} can be moved to a recoverable quarantine',
    noManagedItems: 'Nothing safe to manage here',
    managedPlanTitle: 'Review {category}',
    managedPlanCopy: 'Select local items to move to a recoverable quarantine. Codex must be closed first.',
    managedPlanWarning: 'Definitions and configuration are protected. Only generated files and reinstallable packages are listed.',
    managedPlanRunningWarning: 'Close Codex before moving items so local files stay consistent.',
    managedItemCount: '{count} items selected',
    modified: 'Modified {time}',
    moveToQuarantine: 'Move to quarantine',
    managedComplete: 'Items moved. You can restore them from Activity.',
    indexConsistency: 'Index consistency',
    noIndexIssues: 'No index issues detected',
    needsReview: 'Needs review',
    normal: 'Normal',
    viewDetails: 'View details',
    activityScanComplete: 'Latest scan complete',
    scanSummary: '{date} · {bytes} · {count} files',
    scanPromptTitle: 'Scan prompts',
    scanPromptCopy: 'Statuses to resolve before cleanup',
    noPendingPrompts: 'No pending prompts',
    recoverableOperations: 'Recoverable operations',
    recoverableCopy: 'Quarantine copies are kept until you confirm they are no longer needed.',
    databaseOperation: 'Database compaction · {count} databases · {bytes}',
    conversationOperation: '{count} conversations · {bytes}',
    temporaryOperation: '{count} temporary files · {bytes}',
    temporaryPurgeCopy: 'Permanently delete the temporary files in this recovery operation. This cannot be undone.',
    temporaryPurgeComplete: 'Temporary file recovery copies permanently deleted.',
    managedOperation: 'Managed files · {count} items · {bytes}',
    restored: 'Restored',
    unfinishedRecoverable: 'Incomplete, recoverable',
    failedRecoverable: 'Failed, recoverable',
    recoverable: 'Recoverable',
    restore: 'Restore',
    planConversationTitle: 'Delete',
    planDatabaseTitle: 'Compact database',
    validatingLocalData: 'Validating local data…',
    cleanupPlanCopy: 'Delete {count} conversations with {bytes} of record files. Files move to a recoverable quarantine and still occupy disk space. No archive step is required; worktrees and separate attachments are untouched.',
    cleanupPlanWarning: 'No archive step is required. Selected conversations and their index records are removed directly; files remain recoverable from Activity.',
    cleanupPlanRunningWarning: 'Close Codex before executing so the database and conversation files stay consistent. No archive step is required.',
    compactionPlanCopy: 'This will compact {count} databases and may recover {bytes} of free pages. Results are integrity-checked before replacing the originals.',
    compactionPlanWarning: 'Close all Codex processes first. Original databases are kept in the recovery directory for manual rollback.',
    operationComplete: 'Operation complete. Rescan to see the latest storage usage.',
    close: 'Close',
    executePlan: 'Compact database',
    deleteConversations: 'Delete',
    completed: 'Completed',
    dialogClose: 'Close dialog',
    language: 'Language',
    english: 'English',
    chinese: 'Chinese (Simplified)',
    languageSaved: 'Saved locally on this Mac',
    appearance: 'Appearance',
    theme: 'Theme',
    themeDescription: 'Follow macOS appearance or choose Light or Dark.',
    systemTheme: 'System',
    light: 'Light',
    dark: 'Dark',
    themeSaved: 'Saved locally on this Mac',
    appVersion: `Version ${version}`,
    languageCurrent: 'Current language',
    localizedWarningFallback: 'A local data warning needs review.',
  },
  zh: {
    purgeAcknowledgment: '我了解：永久删除后无法恢复',
    cleanupChoices: "选择要清理的内容",
    cleanupChoicesCopy: "这里只清理对话和独立的 Git工作树。",
    conversationOverviewCopy: "逐条或按月份选择对话。恢复副本会保留，直到你明确清空。",
    worktreeOverviewCopy: "逐个查看项目、分支、路径、时间和占用，再决定删除。",
    otherAppData: "其他应用数据",
    otherAppDataCopy: "按占用从大到小列出各类数据，仅供查看。",

    worktreeTitle: "独立工作树",
    worktreeSafetyCopy: "这里只显示已验证的 Git工作树。确认后会丢弃其中未提交和被忽略的文件；主工作树和锁定注册仍受保护。",
    noWorktrees: "Codex worktrees 目录中未发现 Git工作树。",
    branch: "分支",
    worktreeDirty: "有未提交更改",
    worktreeClean: "没有未提交更改",
    worktreeChangedFiles: "未提交文件：{count} 个",
    worktreeIgnoredFiles: "被忽略文件：{count} 个",
    worktreeCreatedAt: "创建于 {time}",
    worktreeUpdatedAt: "最近更新 {time}",
    mainWorktree: "主工作树",
    lockedWorktree: "已锁定",
    deleteWorktree: "删除工作树",
    deleteWorktrees: "删除工作树",
    selectRemovableWorktrees: "全选可删除的工作树",
    selectedWorktrees: "已选 {count} 个",
    deleteSelectedWorktrees: "删除所选（{count}）",
    confirmDeleteWorktree: "永久删除此工作树",
    confirmDeleteWorktrees: "永久删除所选工作树",
    worktreeDeleteCopy: "将通过 Git 永久删除所选的 {count} 个工作树及其注册信息。里面未提交和被忽略的文件会一并丢弃，但不会删除分支。此操作无法在这里撤销。",
    worktreeDeleteWarning: "未提交和被忽略的文件会被永久丢弃。请在继续前核对下面的路径和数量。",
    deletingWorktree: "正在通过 Git 删除…请勿关闭窗口。",
    worktreeRemoved: "工作树已删除，已核实目录和 Git 注册信息均已移除。",
    worktreesRemoved: "已删除 {count} 个工作树，已核实目录和 Git 注册信息均已移除。",
    worktreeRemovalPartial: "已删除 {removed} 个工作树，另有 {failed} 个未能删除。请重新扫描并检查剩余项目。",
    worktreeProtectedMain: "主工作树受保护",
    worktreeProtectedLocked: "锁定的工作树受保护",
    worktreeProtectedSymlink: "符号链接工作树受保护",
    worktreeProtectedRegistration: "Git 注册信息不可用，请手动检查",
    worktreeUnknownState: "工作树状态未知，请手动检查",

    purgeRecovery: "清空恢复副本",
    purgeCopy: "仅永久删除本次对话操作的恢复文件与数据库备份；之后无法恢复。请勾选确认，了解永久删除无法恢复。不会删除其他操作或现有对话文件。",
    purgeConfirmation: "输入完整操作 ID",
    confirmPurge: "永久清空恢复副本",
    purgeComplete: "恢复副本已永久清空（已删除文件的分配占用 {bytes}）。APFS 快照或共享块可能使实际可用空间变化不同。",
    purged: "恢复副本已永久清空",
    purging: "正在永久清空 / 尚未完成，请重试完成",
    appName: 'Cublet',
    mainNavigation: '主导航',
    overview: '空间总览',
    conversations: '对话',
    maintenance: '维护',
    activity: '恢复与历史',
    activityShort: '恢复',
    settings: '设置',
    scanLocation: '扫描位置',
    localMode: `本地模式 · ${version}`,
    scannedAt: '扫描于 {time}',
    scanning: '扫描中',
    localReadOnly: '本地只读',
    rescan: '重新扫描',
    retry: '重试',
    preparingScan: '正在准备扫描',
    unableToRead: '暂时无法读取 Codex 本地数据，请重试。',
    readingLocalData: '正在读取 Codex 本地数据…',
    activityReadError: '活动记录读取失败：{error}',
    temporaryActivityReadError: '临时文件恢复记录读取失败：{error}',
    totalStorage: 'Codex 总占用',
    estimatedReclaimable: '预计可释放',
    conversationStorage: '对话占用',
    scannedFiles: '扫描 {count} 个文件',
    currentlyAttributable: '当前可明确归因',
    indexedRecords: '{count} 条索引记录',
    storageBreakdown: '存储构成',
    sortedByAllocated: '按实际磁盘块占用排序',
    reclaimableNow: '当前可释放',
    actionable: '可管理',
    pendingReview: '待检查',
    archived: '已归档',
    current: '当前',
    missing: '文件缺失',
    databaseCompressionTitle: '日志数据库可以压缩',
    databaseCompressionCopy: '空闲页约 {bytes}，需要退出 Codex 后执行。',
    viewPlan: '检查数据库',

    databaseReadyCopy: '约 {bytes} 空闲页可回收。',

    files: '{count} 个文件',
    categoryConversations: '对话',
    categoryDatabases: '数据库',
    categoryWorktrees: '工作树',
    categoryAutomations: '自动化产物',
    categoryMedia: '图片与附件',
    categoryExtensions: '插件与技能',
    categoryCache: 'Codex 缓存与临时文件',
    categoryElectronSupport: '桌面应用数据',
    categoryElectronCache: '桌面应用缓存',
    categoryBackups: '备份',
    categoryOther: '未分类文件',
    afterCodexExit: '退出 Codex 后',
    futureVersion: '后续版本',
    inspectOnly: '仅检查',
    handleCarefully: '谨慎操作',
    needsConfirmation: '需确认',
    unknown: '未知',
    searchPlaceholder: '搜索标题或项目',
    filterStatus: '按状态筛选',
    allStatuses: '全部状态',
    sort: '排序',
    sortSize: '按占用排序',
    sortRecent: '按最近活动',
    selectCurrentList: '选择当前列表',
    status: '状态',
    recentActivity: '最近活动',
    size: '占用',
    shownCount: '显示 {shown} 条 · 共 {total} 条索引',
    indexIssues: '存在索引异常',
    externalRecord: '外部 · 仅统计',
    externalBadge: '外部',
    externalFilesCopy: '{count} 条索引 · 已统计 {files} 个可用文件',
    recordSizeTotal: '记录大小合计 {bytes}',
    categoryExternal: '外部对话记录',
    warningCount: '{count} 条扫描提示，可在活动记录中查看详情。',
    selectedConversations: '已选 {count} 个对话',
    estimatedRelease: '所选记录 {bytes} · 可恢复删除',
    conversationView: '对话视图',
    listView: '列表',
    monthView: '按月份',
    monthSelected: '已选 {count} / {total} 个',
    monthCount: '{count} 个对话',
    selectMonth: '选择',
    deselectMonth: '取消选择',
    unknownManualOnly: '请逐条选择',
    noMatchingConversations: '没有符合筛选条件的对话。',
    hiddenSelected: '有 {count} 个已选对话被筛选隐藏',
    clearSelection: '清空选择',
    planSelectionDetails: '所选对话：当前 {current} 个 · 文件缺失 {missing} 个',
    planMissingCopy: '文件缺失的对话仅移除索引，不计入文件清理大小。',
    planCurrentCopy: '包含当前对话，请核对下方标题后再删除。',
    createCleanupPlan: '删除',
    maintenanceTitle: '维护检查',
    maintenanceCopy: '只展示可以明确判断的维护动作。数据库压缩和可管理文件操作需要退出 Codex。',
    compactDatabase: '压缩日志数据库',
    compactEstimate: '预计可回收 {bytes} 空闲页',
    afterCodexExitBadge: '退出后执行',
    inspectWorktrees: '检查工作树',
    worktreeCopy: '{count} 个文件，可能包含未提交代码',
    inspectionOnly: '仅检查',
    manage: '管理',
    manageCache: '管理缓存',
    manageAutomations: '管理自动化产物',
    manageExtensions: '管理插件与技能',
    managedCopy: '约 {bytes} 可移入可恢复隔离区',
    noManagedItems: '这里没有可安全管理的内容',
    managedPlanTitle: '检查{category}',
    managedPlanCopy: '选择要移入可恢复隔离区的本地内容。执行前需要退出 Codex。',
    managedPlanWarning: '配置定义会受到保护，只列出生成产物和可重新安装的包。',
    managedPlanRunningWarning: '为保证本地文件一致，移动前需要退出 Codex。',
    managedItemCount: '已选择 {count} 项',
    modified: '修改于 {time}',
    moveToQuarantine: '移入隔离区',
    managedComplete: '已移入隔离区，可在活动记录中恢复。',
    indexConsistency: '索引一致性',
    noIndexIssues: '未发现索引异常',
    needsReview: '需复核',
    normal: '正常',
    viewDetails: '查看详情',
    activityScanComplete: '最近一次扫描完成',
    scanSummary: '{date} · {bytes} · {count} 个文件',
    scanPromptTitle: '扫描提示',
    scanPromptCopy: '需要在清理前处理的状态',
    noPendingPrompts: '当前没有待处理提示',
    recoverableOperations: '可恢复操作',
    recoverableCopy: '清理和压缩的隔离副本会保留，直到你确认不再需要恢复。',
    databaseOperation: '数据库压缩 · {count} 个数据库 · {bytes}',
    conversationOperation: '{count} 个对话 · {bytes}',
    temporaryOperation: '{count} 项临时文件 · {bytes}',
    temporaryPurgeCopy: '永久删除本次操作的临时文件恢复副本。此操作无法撤销。',
    temporaryPurgeComplete: '临时文件恢复副本已永久删除。',
    managedOperation: '文件管理 · {count} 项 · {bytes}',
    restored: '已恢复',
    unfinishedRecoverable: '未完成，可恢复',
    failedRecoverable: '失败，可恢复',
    recoverable: '可恢复',
    restore: '恢复',
    planConversationTitle: '删除',
    planDatabaseTitle: '压缩数据库',
    validatingLocalData: '正在校验本地数据…',
    cleanupPlanCopy: '删除 {count} 个对话，记录文件共 {bytes}。文件会移入可恢复隔离区，仍占用磁盘空间。无需先归档，工作树和独立附件不会被触碰。',
    cleanupPlanWarning: '无需先归档。选中的对话及其索引会直接移除，记录文件会保留在活动记录中以便恢复。',
    cleanupPlanRunningWarning: '为保证数据库和对话记录文件一致，执行前需要退出 Codex。无需先归档。',
    compactionPlanCopy: '将压缩 {count} 个数据库，预计回收 {bytes} 空闲页。压缩结果会先做完整性检查再替换原文件。',
    compactionPlanWarning: '请先退出所有 Codex 进程。原数据库会保留在恢复目录，可用于人工回退。',
    operationComplete: '操作已完成。重新扫描后可查看最新占用。',
    close: '关闭',
    executePlan: '压缩数据库',
    deleteConversations: '删除',
    completed: '已完成',
    dialogClose: '关闭对话框',
    language: '界面语言',
    english: 'English',
    chinese: '简体中文',
    languageSaved: '仅保存在这台 Mac 上',
    appearance: '外观',
    theme: '主题',
    themeDescription: '跟随 macOS 外观，或选择浅色、深色主题。',
    systemTheme: '跟随系统',
    light: '浅色（Light）',
    dark: '深色（Dark）',
    themeSaved: '仅保存在这台 Mac 上',
    appVersion: `版本 ${version}`,
    languageCurrent: '当前语言',
    localizedWarningFallback: '本地数据需要复核。',
  },
} as const;

export type TranslationKey = keyof typeof messages.en;

function readStoredLocale(): Locale {
  if (typeof window === 'undefined') return 'en';
  try {
    return window.localStorage.getItem(STORAGE_KEY) === 'zh' ? 'zh' : 'en';
  } catch {
    return 'en';
  }
}

function interpolate(template: string, params?: Record<string, string | number>) {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (_, key: string) => String(params[key] ?? `{${key}}`));
}

const categoryKeys: Record<string, TranslationKey> = {
  'external-conversations': 'categoryExternal',
  conversations: 'categoryConversations',
  databases: 'categoryDatabases',
  worktrees: 'categoryWorktrees',
  automations: 'categoryAutomations',
  media: 'categoryMedia',
  extensions: 'categoryExtensions',
  cache: 'categoryCache',
  'electron-support': 'categoryElectronSupport',
  'electron-cache': 'categoryElectronCache',
  backups: 'categoryBackups',
  other: 'categoryOther',
};

const categorySafetyKeys: Record<string, TranslationKey> = {
  'external-conversations': 'inspectOnly',
  conversations: 'actionable',
  databases: 'afterCodexExit',
  worktrees: 'inspectOnly',
  automations: 'actionable',
  media: 'inspectOnly',
  extensions: 'actionable',
  cache: 'actionable',
  'electron-support': 'handleCarefully',
  'electron-cache': 'futureVersion',
  backups: 'needsConfirmation',
};

export function categoryLabel(id: string, locale: Locale) {
  const key = categoryKeys[id] ?? 'categoryOther';
  return messages[locale][key];
}

export function safetyLabel(id: string, safety: string, locale: Locale) {
  const categoryKey = categorySafetyKeys[id];
  if (categoryKey) return messages[locale][categoryKey];
  const kind = safety.split(':', 1)[0];
  const key: TranslationKey = kind === 'managed' ? 'actionable' : kind === 'conditional' ? 'needsConfirmation' : kind === 'protected' ? 'inspectOnly' : 'unknown';
  return messages[locale][key];
}

export function localizeWarning(warning: string, locale: Locale) {
  if (locale === 'zh') return warning;
  if (warning.includes('演示模式：桌面运行时不可用，使用本地样例数据')) return 'Demo mode: desktop runtime unavailable; showing local sample data.';
  if (warning.includes('破坏性清理仅可在桌面应用中执行')) return 'Destructive cleanup is only available in the desktop app.';
  if (warning.includes('恢复操作仅可在桌面应用中执行')) return 'Restore is only available in the desktop app.';
  if (warning.includes('数据库压缩仅可在桌面应用中执行')) return 'Database compaction is only available in the desktop app.';
  if (warning.includes('优雅退出仅支持 macOS')) return 'Graceful quit is only supported on macOS.';
  if (warning.includes('Codex 未接受退出请求')) return 'Codex did not accept the quit request.';
  if (warning.includes('已请求 Codex 优雅退出')) return 'Quit requested. Check again after Codex finishes closing.';
  if (warning.includes('部分对话索引指向的对话记录文件已不存在')) return 'Some indexed conversations point to missing conversation files.';
  if (warning.includes('扫描到了目录，但未发现可计量文件')) return 'Directories were found, but no measurable files were detected.';
  if (warning.includes('发现未完成的数据库压缩')) return 'An unfinished database compaction needs recovery review.';
  if (warning.includes('数据库压缩操作')) return 'An unfinished database compaction is available for recovery.';
  if (/[\u3400-\u9fff]/u.test(warning)) return messages.en.localizedWarningFallback;
  return warning;
}

export function localizeWorktreeReason(reason: string, locale: Locale) {
  if (!reason) return '';
  const key: TranslationKey = reason.includes('Main worktree')
    ? 'worktreeProtectedMain'
    : reason.includes('Locked worktree')
      ? 'worktreeProtectedLocked'
      : reason.includes('Symlink')
        ? 'worktreeProtectedSymlink'
        : reason.includes('registration') || reason.includes('registered') || reason.includes('Git')
          ? 'worktreeProtectedRegistration'
          : 'worktreeUnknownState';
  return messages[locale][key];
}

export function formatDateTime(value: string | number, locale: Locale) {
  return new Date(value).toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN');
}

export function formatTime(value: string | number, locale: Locale) {
  return new Date(value).toLocaleTimeString(locale === 'en' ? 'en-US' : 'zh-CN', { hour: '2-digit', minute: '2-digit' });
}

type I18nContextValue = {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  t: (key: TranslationKey, params?: Record<string, string | number>) => string;
  formatNumber: (value: number) => string;
};

const I18nContext = createContext<I18nContextValue | null>(null);

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocale] = useState<Locale>(readStoredLocale);

  useEffect(() => {
    document.documentElement.lang = locale === 'en' ? 'en' : 'zh-CN';
    document.title = messages[locale].appName;
    try {
      window.localStorage.setItem(STORAGE_KEY, locale);
    } catch {
      // Local persistence is best-effort in restricted webviews.
    }
  }, [locale]);

  const value = useMemo<I18nContextValue>(() => ({
    locale,
    setLocale,
    t: (key, params) => interpolate(messages[locale][key], params),
    formatNumber: (number) => number.toLocaleString(locale === 'en' ? 'en-US' : 'zh-CN'),
  }), [locale]);

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n() {
  const context = useContext(I18nContext);
  if (!context) throw new Error('useI18n must be used inside I18nProvider');
  return context;
}
