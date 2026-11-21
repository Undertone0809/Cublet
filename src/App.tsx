import { BrandLogo } from "./components/BrandLogo";
import { WorktreeList } from './components/WorktreeList';
import { motion } from 'framer-motion';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  AlertTriangle,
  Archive,
  Check,
  ChevronRight,
  CircleHelp,
  Database,
  FolderGit2,
  History,
  LayoutDashboard,
  LoaderCircle,
  MessagesSquare,
  Moon,
  RefreshCw,
  Settings,
  Sun,
  Trash2,
} from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Badge } from './components/ui/badge';
import { Button } from './components/ui/button';
import { Checkbox } from './components/ui/checkbox';
import { Alert, AlertDescription } from './components/ui/alert';
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from './components/ui/dialog';
import { WorkspaceSearch } from './components/WorkspaceSearch';
import { NativeSelect } from './components/ui/native-select';
import { Tabs, TabsList, TabsTrigger, TabsContent } from './components/ui/tabs';
import { groupConversations, monthBulkCandidates, monthLabel, selectionSummary, toggleSelection } from './lib/conversation-months';
import {
  createCleanupPlan,
  createCompactionPlan,
  executeCleanupPlan,
  executeCompactionPlan,
  getCodexStatus,
  listCleanupOperations,
  listTemporaryCleanupOperations,
  purgeRecovery,
  purgeTemporaryCleanupOperation,
  restoreCleanupOperation,
  restoreCompactionOperation,
  restoreManagedCleanupOperation,
  restoreTemporaryCleanupOperation,
  scanStorage,
} from './lib/api';
import { categoryLabel, formatDateTime, formatTime, localizeWarning, useI18n, type Locale, type TranslationKey } from './lib/i18n';
import { useTheme } from './lib/theme-context';
import { formatBytes, formatRelativeTime } from './lib/utils';
import type { CleanupOperationSummary, CleanupPlan, CompactionPlan, Conversation, StorageSnapshot, View } from './types';
import './codex-tools.css';
import './cleanup-compact.css';

const navItems: { id: View; labelKey: TranslationKey; icon: typeof LayoutDashboard }[] = [
  { id: 'overview', labelKey: 'overview', icon: LayoutDashboard },
  { id: 'conversations', labelKey: 'conversations', icon: MessagesSquare },
  { id: 'worktrees', labelKey: 'categoryWorktrees', icon: FolderGit2 },
  { id: 'activity', labelKey: 'activity', icon: History },
  { id: 'settings', labelKey: 'settings', icon: Settings },
];

function StatusBadge({ status, external = false }: { status: Conversation['status']; external?: boolean }) {
  const { t } = useI18n();
  if (external && status !== 'missing') return <Badge variant="outline">{t('externalBadge')}</Badge>;
  if (status === 'archived') return <Badge variant="secondary"><Archive size={12} />{t('archived')}</Badge>;
  if (status === 'missing') return <Badge variant="destructive"><AlertTriangle size={12} />{t('missing')}</Badge>;
  return <Badge variant="secondary"><span className="status-dot" />{t('current')}</Badge>;
}

function Overview({ snapshot, onOpenConversations, onOpenWorktrees }: { snapshot: StorageSnapshot; onOpenConversations: () => void; onOpenWorktrees: () => void }) {
  const { t, locale, formatNumber } = useI18n();
  const conversations = snapshot.categories.filter((c) => ['conversations', 'external-conversations'].includes(c.id)).reduce((n, c) => n + c.bytes, 0);
  const worktrees = snapshot.categories.find((c) => c.id === 'worktrees')?.bytes ?? 0;
  const otherCategories = snapshot.categories
    .filter((category) => !['conversations', 'external-conversations', 'worktrees'].includes(category.id))
    .sort((a, b) => b.bytes - a.bytes);
  const other = otherCategories.reduce((sum, category) => sum + category.bytes, 0);
  return <div className="page-stack">
    <div className="section-heading-row"><div><h2>{t('totalStorage')} · {formatBytes(snapshot.total_bytes)}</h2><p>{t('scannedFiles', { count: formatNumber(snapshot.file_count) })}</p></div></div>
    <section className="surface-section"><div className="section-heading-row"><div><h2>{t('cleanupChoices')}</h2><p>{t('cleanupChoicesCopy')}</p></div></div>
      <button type="button" className="cleanup-choice" onClick={onOpenConversations}><MessagesSquare /><span><strong>{t('conversations')}</strong><small>{t('conversationOverviewCopy')}</small></span><b>{formatBytes(conversations)}</b><ChevronRight /></button>
      <button type="button" className="cleanup-choice" onClick={onOpenWorktrees}><FolderGit2 /><span><strong>Worktrees</strong><small>{t('worktreeOverviewCopy')}</small></span><b>{formatBytes(worktrees)}</b><ChevronRight /></button>
      <div className="other-storage"><span><strong>{t('otherAppData')}</strong><small>{t('otherAppDataCopy')}</small></span><b>{formatBytes(other)}</b></div>
      <dl className="storage-breakdown" aria-label={t('otherAppData')}>
        {otherCategories.map((category) => <div className="storage-breakdown-row" key={category.id}>
          <dt>{categoryLabel(category.id, locale)}<small>{t('files', { count: formatNumber(category.file_count) })}</small></dt>
          <dd>{formatBytes(category.bytes)}</dd>
        </div>)}
      </dl>
    </section>
  </div>;
}

function ConversationList({ snapshot, selected, setSelected, onCreatePlan }: { snapshot: StorageSnapshot; selected: Set<string>; setSelected: (value: Set<string>) => void; onCreatePlan: () => void }) {
  const { locale, t, formatNumber } = useI18n();
  const [query, setQuery] = useState('');
  const [statusFilter, setStatusFilter] = useState('all');
  const [sort, setSort] = useState('size');
  const [conversationView, setConversationView] = useState('list');
  const [expandedMonths, setExpandedMonths] = useState<Set<string>>(new Set());
  const parentRef = useRef<HTMLDivElement>(null);
  const filteredConversations = useMemo(() => {
    const normalized = query.trim().toLowerCase();
    return snapshot.conversations
      .filter((conversation) => !normalized || `${conversation.title} ${conversation.cwd}`.toLowerCase().includes(normalized))
      .filter((conversation) => statusFilter === 'all' || conversation.status === statusFilter)
      .sort((a, b) => sort === 'size' ? b.bytes - a.bytes : (b.updated_at_ms ?? 0) - (a.updated_at_ms ?? 0));
  }, [query, snapshot.conversations, sort, statusFilter]);
  const virtualizer = useVirtualizer({ count: filteredConversations.length, getScrollElement: () => parentRef.current, estimateSize: () => 64, overscan: 12 });
  const selectedBytes = snapshot.conversations.reduce((sum, item) => sum + (selected.has(item.id) ? item.bytes : 0), 0);
  const groups = useMemo(() => groupConversations(filteredConversations), [filteredConversations]);
  const deletionProtected = snapshot.categories.find((item) => item.id === 'conversations')?.safety.startsWith('protected') ?? false;
  const selectable = filteredConversations.filter((item) => !item.external && !deletionProtected);
  const visibleIds = new Set(filteredConversations.map((item) => item.id));
  const hiddenSelected = [...selected].filter((id) => !visibleIds.has(id)).length;


  function toggle(id: string) {
    if (deletionProtected || snapshot.conversations.find((item) => item.id === id)?.external) return;
    const next = new Set(selected);
    if (next.has(id)) next.delete(id); else next.add(id);
    setSelected(next);
  }

  function toggleVisible() {
    const next = new Set(selected);
    const allSelected = selectable.length > 0 && selectable.every((conversation) => next.has(conversation.id));
    selectable.forEach((conversation) => allSelected ? next.delete(conversation.id) : next.add(conversation.id));
    setSelected(next);
  }

  return (
    <motion.div className="page-stack conversation-page" initial={false} animate={{ opacity: 1 }} transition={{ duration: 0.22 }}>
      <Tabs className="conversation-tabs" value={conversationView} onValueChange={(value) => setConversationView(String(value))}>
      <div className="conversation-toolbar">
        <TabsList aria-label={t('conversationView')}><TabsTrigger value="list">{t('listView')}</TabsTrigger><TabsTrigger value="months">{t('monthView')}</TabsTrigger></TabsList>
        <div className="list-toolbar">
          <WorkspaceSearch value={query} onChange={setQuery} label={t('searchPlaceholder')} clearLabel={locale === 'zh' ? '清空搜索' : 'Clear search'} />
          <NativeSelect value={statusFilter} onValueChange={setStatusFilter} aria-label={t('filterStatus')}><option value="all">{t('allStatuses')}</option><option value="current">{t('current')}</option><option value="archived">{t('archived')}</option><option value="missing">{t('missing')}</option></NativeSelect>
          <NativeSelect value={sort} onValueChange={setSort} aria-label={t('sort')}><option value="size">{t('sortSize')}</option><option value="recent">{t('sortRecent')}</option></NativeSelect>
        </div>
      </div>

      <TabsContent value="list"><div className="table-shell cleanup-surface">
        <div className="table-head conversation-grid"><label className="check-wrap"><Checkbox disabled={!selectable.length} checked={selectable.length > 0 && selectable.every((conversation) => selected.has(conversation.id))} indeterminate={selectable.some((conversation) => selected.has(conversation.id)) && !selectable.every((conversation) => selected.has(conversation.id))} onCheckedChange={toggleVisible} aria-label={t('selectCurrentList')} /></label><span>{t('conversations')}</span><span>{t('status')}</span><span>{t('recentActivity')}</span><span className="align-end">{t('size')}</span></div>
        <div className="virtual-list" ref={parentRef}>
          {filteredConversations.length === 0 && <p className="cleanup-empty" role="status">{t('noMatchingConversations')}</p>}
          <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
            {virtualizer.getVirtualItems().map((item) => {
              const conversation = filteredConversations[item.index];
              const isSelected = selected.has(conversation.id);
              return <div className={`conversation-row conversation-grid ${isSelected ? 'is-selected' : ''}`} key={conversation.id} data-index={item.index} ref={virtualizer.measureElement} style={{ position: 'absolute', top: 0, left: 0, width: '100%', transform: `translateY(${item.start}px)` }}>
                <label className="check-wrap"><Checkbox disabled={conversation.external || deletionProtected} checked={isSelected} onCheckedChange={() => toggle(conversation.id)} aria-label={`${t('conversations')}: ${conversation.title}`} /></label>
                <div className="conversation-main"><strong>{conversation.title}</strong><small title={conversation.external ? conversation.rollout_path : conversation.cwd}>{conversation.external ? `${t('externalRecord')} · ${conversation.rollout_path}` : conversation.cwd}</small></div>
                <StatusBadge status={conversation.status} external={conversation.external} />
                <span className="conversation-time">{formatRelativeTime(conversation.updated_at_ms, locale)}</span>
                <span className="align-end conversation-size">{formatBytes(conversation.bytes)}</span>
              </div>;
            })}
          </div>
        </div>
        <div className="table-footer"><span>{t('shownCount', { shown: formatNumber(filteredConversations.length), total: formatNumber(snapshot.conversations.length) })}</span>{snapshot.warnings.length > 0 && <span className="footer-warning"><AlertTriangle size={13} />{t('indexIssues')}</span>}</div>
      </div></TabsContent>
      <TabsContent value="months">
        <div className="month-list cleanup-surface">
          {groups.length === 0 && <p className="cleanup-empty" role="status">{t('noMatchingConversations')}</p>}
          {groups.map(({ key, items }) => {
            const candidates = deletionProtected ? [] : monthBulkCandidates(items, true);
            const count = items.filter((item) => selected.has(item.id)).length;
            const all = candidates.length > 0 && candidates.every((item) => selected.has(item.id));
            return <section className="month-section" key={key ?? 'unknown'}>
              <div className="month-heading"><button type="button" className="month-expand" aria-expanded={expandedMonths.has(key ?? 'unknown')} onClick={() => setExpandedMonths((current) => { const next = new Set(current); const id = key ?? 'unknown'; if (next.has(id)) next.delete(id); else next.add(id); return next; })}><ChevronRight size={16} /><div><strong>{monthLabel(key, locale)}</strong><span>{t('monthCount', { count: formatNumber(items.length) })} · {t('recordSizeTotal', { bytes: formatBytes(items.reduce((sum, item) => sum + item.bytes, 0)) })}</span></div></button><span className="month-selection-count">{t('monthSelected', { count, total: items.length })}</span>{key !== null ? <Button variant="outline" size="sm" disabled={!candidates.length} onClick={() => setSelected(toggleSelection(selected, candidates))}>{all ? t('deselectMonth') : t('selectMonth')}</Button> : <span className="month-selection-count">{t('unknownManualOnly')}</span>}</div>
              {expandedMonths.has(key ?? 'unknown') && <div className="month-conversations">{items.map((conversation) => <div className={`conversation-row conversation-grid ${selected.has(conversation.id) ? 'is-selected' : ''}`} key={conversation.id}><label className="check-wrap"><Checkbox disabled={conversation.external || deletionProtected} checked={selected.has(conversation.id)} onCheckedChange={() => toggle(conversation.id)} aria-label={`${t('conversations')}: ${conversation.title}`} /></label><div className="conversation-main"><strong>{conversation.title}</strong><small title={conversation.cwd}>{conversation.cwd}</small></div><StatusBadge status={conversation.status} external={conversation.external} /><span className="conversation-time">{formatRelativeTime(conversation.updated_at_ms, locale)}</span><span className="align-end conversation-size">{formatBytes(conversation.bytes)}</span></div>)}</div>}
            </section>;
          })}
        </div>
      </TabsContent></Tabs>

      {selected.size > 0 && <footer className="selection-bar cleanup-surface">
        <div className="selection-summary"><strong>{t('selectedConversations', { count: formatNumber(selected.size) })}</strong><span>{t('estimatedRelease', { bytes: formatBytes(selectedBytes) })}</span>{hiddenSelected > 0 && <span>{t('hiddenSelected', { count: hiddenSelected })}</span>}</div>
        <div className="selection-actions"><Button variant="outline" disabled={!selected.size} onClick={() => setSelected(new Set())}>{t('clearSelection')}</Button><Button variant="destructive" disabled={!selected.size} onClick={onCreatePlan}><Trash2 data-icon="inline-start" />{t('createCleanupPlan')}</Button></div>
      </footer>}
    </motion.div>
  );
}

function operationTitleKey(type: CleanupOperationSummary['operation_type']) {
  switch (type) {
    case 'temporary': return 'temporaryOperation';
    case 'compaction': return 'databaseOperation';
    case 'managed': return 'managedOperation';
    case 'cleanup': return 'conversationOperation';
  }
}

function Activity({ snapshot, operations, onRestore, restoring, onPurge }: { onPurge: (operation: CleanupOperationSummary) => void; snapshot: StorageSnapshot | null; operations: CleanupOperationSummary[]; onRestore: (operation: CleanupOperationSummary) => void; restoring: string | null }) {
  const { locale, t, formatNumber } = useI18n();
  const operationStatus = (status: string) => status === 'purged' ? t('purged') : status === 'purging' ? t('purging') : status === 'restored' ? t('restored') : ['running', 'prepared', 'moving', 'restoring'].includes(status) ? t('unfinishedRecoverable') : status === 'failed' ? t('failedRecoverable') : t('recoverable');
  return <motion.div className="page-stack" initial={false} animate={{ opacity: 1 }} transition={{ duration: 0.22 }}>
    <section className="activity-section">
      <div className="section-heading-row"><div><h2>{t('recoverableOperations')}</h2>{operations.length > 0 && <p>{t('recoverableCopy')}</p>}</div></div>
      <div className="operation-list cleanup-surface">
        {operations.length === 0 && <p className="cleanup-empty" role="status">{locale === 'zh' ? '暂无清理记录。' : 'No cleanup history yet.'}</p>}
        {operations.map((operation) => {
          const bytes = formatBytes(operation.reclaimed_bytes);
          const count = formatNumber(operation.operation_type === 'compaction' ? operation.database_count : operation.thread_count);
          const temporary = operation.operation_type === 'temporary';
          const canRestore = temporary
            ? ['completed', 'failed', 'prepared', 'moving', 'restoring'].includes(operation.status)
            : !['restored', 'purging', 'purged'].includes(operation.status);
          const canPurge = temporary
            ? ['completed', 'failed', 'purging'].includes(operation.status)
            : operation.operation_type === 'cleanup' && ['completed', 'restored', 'purging'].includes(operation.status);
          return <div className="operation-row" key={`${operation.operation_type}:${operation.operation_id}`}>
            <div>
              <strong>{t(operationTitleKey(operation.operation_type), { count, bytes })}</strong>
              {operation.selection_summary && <span>{operation.selection_summary}</span>}
              <span>{formatDateTime(operation.created_at, locale)} · {operationStatus(operation.status)}</span>
            </div>
            <div className="cleanup-row-actions">
              <Button variant="outline" size="sm" onClick={() => onRestore(operation)} disabled={!canRestore || restoring !== null}>
                {restoring === operation.operation_id ? <LoaderCircle className="spin" size={14} /> : <History size={14} />}{t('restore')}
              </Button>
              {canPurge && <Button variant="destructive" size="sm" disabled={restoring !== null} onClick={() => onPurge(operation)}>{t('purgeRecovery')}</Button>}
            </div>
          </div>;
        })}
      </div>
    </section>
    {snapshot && <details className="activity-section"><summary>{t('scanPromptTitle')}</summary><p>{t('scanSummary', { date: formatDateTime(snapshot.scanned_at, locale), bytes: formatBytes(snapshot.total_bytes), count: formatNumber(snapshot.file_count) })}</p>{snapshot.warnings.map((warning, index) => <p key={index}>{localizeWarning(warning, locale)}</p>)}</details>}
  </motion.div>;
}

function SettingsPage() {
  const { locale, setLocale, t } = useI18n();
  const { theme, resolvedTheme, setTheme } = useTheme();
  const ThemeIcon = resolvedTheme === 'dark' ? Moon : Sun;
  return <motion.div className="page-stack" initial={false} animate={{ opacity: 1 }} transition={{ duration: 0.22 }}>
    <section className="settings-section" aria-labelledby="language-setting-title">
      <div className="settings-section-heading"><div><h2 id="language-setting-title">{t('language')}</h2></div><span className="settings-saved">{t('languageSaved')}</span></div>
      <div className="settings-control-row">
        <div className="settings-language-preview"><span className="language-symbol" aria-hidden="true">文</span><div><strong>{t('languageCurrent')}</strong><span>{locale === 'en' ? t('english') : t('chinese')}</span></div></div>
        <NativeSelect value={locale} onValueChange={(value) => setLocale(value as Locale)} aria-label={t('language')}>
          <option value="en">{t('english')}</option>
          <option value="zh">{t('chinese')}</option>
        </NativeSelect>
      </div>
    </section>
    <section className="settings-section" aria-labelledby="theme-setting-title">
      <div className="settings-section-heading"><div><h2 id="theme-setting-title">{t('appearance')}</h2><p>{t('themeDescription')}</p></div><span className="settings-saved">{t('themeSaved')}</span></div>
      <div className="settings-control-row">
        <div className="settings-language-preview"><span className="language-symbol" aria-hidden="true"><ThemeIcon size={18} /></span><div><strong>{t('theme')}</strong><span>{theme === 'system' ? t('systemTheme') : theme === 'dark' ? t('dark') : t('light')}</span></div></div>
        <NativeSelect value={theme} onValueChange={(value) => setTheme(value as 'system' | 'light' | 'dark')} aria-label={t('theme')}>
          <option value="system">{t('systemTheme')}</option>
          <option value="light">{t('light')}</option>
          <option value="dark">{t('dark')}</option>
        </NativeSelect>
      </div>
    </section>
    <section className="settings-section settings-about"><div><h2>{t('appName')}</h2><p>{t('localMode')}</p></div><span>{t('appVersion')}</span></section>
  </motion.div>;
}

function App({ embedded = false, onStorageChanged }: { embedded?: boolean; onStorageChanged?: () => void }) {
  const { locale, t, formatNumber } = useI18n();
  const [view, setView] = useState<View>(embedded ? 'conversations' : 'overview');
  const [snapshot, setSnapshot] = useState<StorageSnapshot | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [isScanning, setIsScanning] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);
  const [planOpen, setPlanOpen] = useState(false);
  const [planKind, setPlanKind] = useState<'conversation' | 'database'>('conversation');
  const [plan, setPlan] = useState<CleanupPlan | CompactionPlan | null>(null);
  const [planBusy, setPlanBusy] = useState(false);
  const [writerRunning, setWriterRunning] = useState(false);
  const [planError, setPlanError] = useState<string | null>(null);
  const [planWarnings, setPlanWarnings] = useState<string[]>([]);
  const [planComplete, setPlanComplete] = useState(false);
  const [planConversations, setPlanConversations] = useState<Conversation[]>([]);
  const [operations, setOperations] = useState<CleanupOperationSummary[]>([]);
  const [temporaryOperations, setTemporaryOperations] = useState<CleanupOperationSummary[]>([]);
  const [restoring, setRestoring] = useState<string | null>(null);
  const [purgeTarget, setPurgeTarget] = useState<CleanupOperationSummary | null>(null);
  const [purgeAcknowledged, setPurgeAcknowledged] = useState(false);
  const [purgeBusy, setPurgeBusy] = useState(false);
  const [purgeError, setPurgeError] = useState('');
  const [purgeMessage, setPurgeMessage] = useState<{ bytes: number; temporary: boolean } | null>(null);
  const purgeGate = useRef(false);
  async function runScan() {
    setIsScanning(true);
    setScanError(null);
    const [snapshotResult, operationsResult, temporaryResult] = await Promise.allSettled([scanStorage(), listCleanupOperations(), listTemporaryCleanupOperations()]);
    const errors: string[] = [];

    if (snapshotResult.status === 'fulfilled') {
      setSnapshot(snapshotResult.value);
      const protectedIndex = snapshotResult.value.categories.some((item) => item.id === 'conversations' && item.safety.startsWith('protected'));
      const existingIds = new Set(protectedIndex ? [] : snapshotResult.value.conversations.filter((item) => !item.external).map((item) => item.id));
      setSelected((current) => new Set([...current].filter((id) => existingIds.has(id))));
    } else {
      errors.push(snapshotResult.reason instanceof Error ? snapshotResult.reason.message : String(snapshotResult.reason));
    }

    if (operationsResult.status === 'fulfilled') {
      setOperations(operationsResult.value);
    } else {
      errors.push(t('activityReadError', { error: operationsResult.reason instanceof Error ? operationsResult.reason.message : String(operationsResult.reason) }));
    }
    if (temporaryResult.status === 'fulfilled') {
      setTemporaryOperations(temporaryResult.value);
    } else if (snapshotResult.status === 'fulfilled' || operationsResult.status === 'fulfilled') {
      errors.push(t('temporaryActivityReadError', { error: String(temporaryResult.reason) }));
    }

    if (errors.length > 0) setScanError(errors.join(locale === 'en' ? '; ' : '；'));
    setIsScanning(false);
    return snapshotResult.status === 'fulfilled' ? snapshotResult.value : undefined;
  }

  async function purgeOperation() {
    if (!purgeTarget || purgeGate.current || !purgeAcknowledged) return;
    purgeGate.current = true; setPurgeBusy(true); setPurgeError('');
    try {
      const temporary = purgeTarget.operation_type === 'temporary';
      const bytes = temporary
        ? await purgeTemporaryCleanupOperation(purgeTarget.operation_id)
        : await purgeRecovery(purgeTarget.operation_id, purgeTarget.operation_id);
      if (temporary) onStorageChanged?.();
      setPurgeTarget(null); setPurgeMessage({ bytes, temporary }); await runScan();
    } catch (e) { setPurgeError(String(e)); }
    finally { purgeGate.current = false; setPurgeBusy(false); }
  }

  useEffect(() => { void runScan(); }, []);

  async function openPlan(kind: 'conversation' | 'database') {
    setPlanKind(kind);
    setPlanOpen(true);
    setPlan(null);
    setPlanError(null);
    setPlanWarnings([]);
    setPlanComplete(false);
    setPlanBusy(true);
    setWriterRunning(false);
    try {
      const rows = snapshot?.conversations.filter((item) => selected.has(item.id)) ?? [];
      setPlanConversations(rows);
      const [review, writer] = await Promise.all([
        kind === 'conversation' ? createCleanupPlan(rows.map((item) => item.id), selectionSummary(rows, locale)) : createCompactionPlan(),
        getCodexStatus(),
      ]);
      setPlan(review);
      setWriterRunning(writer.running);
    } catch (error) {
      setPlanError(error instanceof Error ? error.message : String(error));
    } finally {
      setPlanBusy(false);
    }
  }

  async function executePlan() {
    if (!plan || writerRunning) return;
    setPlanBusy(true);
    setPlanError(null);
    try {
      if (planKind === 'conversation' && 'thread_ids' in plan) {
        const result = await executeCleanupPlan(plan);
        setPlanWarnings(result.warnings);
        setSelected(new Set());
        await runScan();
      } else if (planKind === 'database' && 'databases' in plan) {
        setPlan(await executeCompactionPlan(plan));
        await runScan();
      }
      setPlanComplete(true);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes('检测到 Codex 仍在运行')) setWriterRunning(true);
      else setPlanError(message);
    } finally {
      setPlanBusy(false);
    }
  }

  async function restoreOperation(operation: CleanupOperationSummary) {
    setRestoring(operation.operation_id);
    setScanError(null);
    try {
      switch (operation.operation_type) {
        case 'compaction': await restoreCompactionOperation(operation.operation_id); break;
        case 'managed': await restoreManagedCleanupOperation(operation.operation_id); break;
        case 'temporary':
          await restoreTemporaryCleanupOperation(operation.operation_id);
          onStorageChanged?.();
          break;
        case 'cleanup': await restoreCleanupOperation(operation.operation_id); break;
      }
      await runScan();
    } catch (error) {
      setScanError(error instanceof Error ? error.message : String(error));
    } finally {
      setRestoring(null);
    }
  }

  const pageTitle = t(navItems.find((item) => item.id === view)?.labelKey ?? 'overview');
  const lastScan = snapshot ? t('scannedAt', { time: formatTime(snapshot.scanned_at, locale) }) : t('preparingScan');
  const conversationsProtected = snapshot?.categories.some((item) => item.id === 'conversations' && item.safety.startsWith('protected'));
  const rescan = <Button variant="outline" size="sm" onClick={() => void runScan()} disabled={isScanning}>{isScanning ? <LoaderCircle className="spin" size={15} /> : <RefreshCw size={15} />}{t('rescan')}</Button>;
  const content = <>
    {!window.electronAPI && <Alert role="status"><AlertDescription>{locale === 'en' ? 'Demo preview — sample data only. No real Codex files or processes are changed.' : '演示预览：仅使用样例数据，不会修改真实 Codex 文件或进程。'}</AlertDescription></Alert>}
    {(!embedded || view === 'conversations') && (snapshot?.scan_mode === 'quick' || conversationsProtected) && <details className="codex-scan-info"><summary>{conversationsProtected ? (locale === 'zh' ? '会话只读 · 索引预览' : 'Read-only conversations · index preview') : (locale === 'zh' ? '索引预览' : 'Index preview')}</summary><p>{locale === 'zh' ? '会话大小仅包含记录文件，不含共享数据库。当前数据结构不支持安全删除的会话会保留。' : 'Conversation sizes cover record files, excluding shared databases. Unsupported conversation schemas remain read-only.'}</p></details>}
    {scanError && <div className="error-banner" role="alert"><AlertTriangle size={16} /><span>{localizeWarning(scanError, locale)}</span><Button variant="ghost" size="sm" onClick={() => void runScan()}>{t('retry')}</Button></div>}
    {purgeMessage !== null && <Alert role="status"><AlertDescription>{purgeMessage.temporary ? t('temporaryPurgeComplete') : t('purgeComplete', { bytes: formatBytes(purgeMessage.bytes) })}</AlertDescription></Alert>}
    {view === 'activity' ? <Activity snapshot={snapshot} operations={[...operations, ...temporaryOperations].sort((a, b) => b.created_at.localeCompare(a.created_at))} onRestore={(operation) => void restoreOperation(operation)} restoring={restoring} onPurge={(operation) => { setPurgeTarget(operation); setPurgeAcknowledged(false); setPurgeError(''); }} /> : snapshot ? <>
      {view === 'overview' && <Overview snapshot={snapshot} onOpenConversations={() => setView('conversations')} onOpenWorktrees={() => setView('worktrees')} />}
      {view === 'conversations' && <ConversationList snapshot={snapshot} selected={selected} setSelected={setSelected} onCreatePlan={() => void openPlan('conversation')} />}
      {view === 'worktrees' && <WorktreeList onChanged={runScan} />}
      {view === 'settings' && <SettingsPage />}
    </> : <div className="loading-state" role="status">{scanError ? <><AlertTriangle size={20} /><span>{t('unableToRead')}</span></> : <><LoaderCircle className="spin" size={20} /><span>{t('readingLocalData')}</span></>}</div>}
  </>;

  return <>
    {embedded ? <section className="codex-tools" aria-label={locale === 'zh' ? 'Codex 工具' : 'Codex tools'}>
      <header className="codex-tools-header">
        <nav className="codex-tools-nav" aria-label={locale === 'zh' ? 'Codex 工具导航' : 'Codex tools navigation'}>
          {navItems.filter(({ id }) => id === 'conversations' || id === 'worktrees' || id === 'activity').map(({ id, labelKey, icon: Icon }) => (
            <button key={id} type="button" aria-current={view === id ? 'page' : undefined} onClick={() => setView(id)}>
              <Icon size={15} /><span>{t(labelKey)}</span>
            </button>
          ))}
        </nav>
        <div className="codex-tools-scan"><span role="status">{isScanning ? t('scanning') : lastScan}</span>{rescan}</div>
      </header>
      <div className={`codex-tools-content${view === 'conversations' ? ' is-conversations' : ''}`}>{content}</div>
    </section> : <div className="app-shell">
      <aside className="app-sidebar">
        <div className="brand"><BrandLogo /></div>
        <nav className="app-nav" aria-label={t('mainNavigation')}>{navItems.map(({ id, labelKey, icon: Icon }) => <button type="button" key={id} className={`nav-item ${view === id ? 'is-active' : ''}`} onClick={() => setView(id)} aria-current={view === id ? 'page' : undefined}><Icon size={16} /><span className="nav-label-desktop">{t(labelKey)}</span><span className="nav-label-mobile">{id === 'activity' ? t('activityShort') : t(labelKey)}</span>{id === 'activity' && snapshot?.warnings.length ? <span className="nav-count">{snapshot.warnings.length}</span> : null}</button>)}</nav>
        <div className="sidebar-footer"><span>{t('scanLocation')}</span><strong>{snapshot?.codex_home ?? '~/.codex'}</strong><span className="sidebar-version">{t('localMode')}</span></div>
      </aside>
      <main className="app-main">
        <header className="topbar"><div><h1>{pageTitle}</h1><span>{lastScan}</span></div><div className="topbar-actions">{rescan}</div></header>
        <div className={`content-wrap ${view === 'conversations' ? 'is-conversations' : ''}`}>{content}</div>
      </main>
    </div>}
    <Dialog open={planOpen} onOpenChange={(nextOpen) => { if (!nextOpen && !planBusy) setPlanOpen(false); }}>
      <DialogContent className="plan-dialog" showCloseButton={!planBusy} closeLabel={t('dialogClose')}>
        <DialogHeader>
          <DialogTitle>{planKind === 'conversation' ? t('planConversationTitle') : t('planDatabaseTitle')}</DialogTitle>
          <DialogDescription>
            {planKind === 'conversation' && plan && 'thread_ids' in plan
              ? t('cleanupPlanCopy', { count: formatNumber(plan.thread_ids.length), bytes: formatBytes(plan.estimated_bytes) })
              : plan && 'databases' in plan
                ? t('compactionPlanCopy', { count: formatNumber(plan.databases.length), bytes: formatBytes(plan.estimated_bytes) })
                : t('validatingLocalData')}
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="plan-dialog-body">
          {planBusy && !plan ? <div className="plan-loading" role="status"><LoaderCircle className="spin" size={18} /><span>{t('validatingLocalData')}</span></div> : planError ? <Alert variant="destructive"><AlertTriangle /><AlertDescription>{localizeWarning(planError, locale)}</AlertDescription></Alert> : planKind === 'conversation' && plan && 'thread_ids' in plan ? <>
            <div className="plan-selection-details">
              <strong>{t('planSelectionDetails', { current: planConversations.filter((item) => item.status === 'current').length, missing: plan.entries.filter((item) => !item.fingerprint).length })}</strong>
              <span>{plan.selection_summary}</span>
              {planConversations.some((item) => item.status === 'current') && <span>{t('planCurrentCopy')}</span>}
              {plan.entries.some((item) => !item.fingerprint) && <span>{t('planMissingCopy')}</span>}
            </div>
            <ul className="plan-conversation-titles">{plan.entries.map((item) => <li key={item.thread_id}><span>{item.title}</span><small>{formatBytes(item.bytes)}</small></li>)}</ul>
            <Alert><CircleHelp /><AlertDescription>{t('cleanupPlanWarning')}</AlertDescription></Alert>
          </> : plan && 'databases' in plan ? <Alert><CircleHelp /><AlertDescription>{t('compactionPlanWarning')}</AlertDescription></Alert> : null}
          {writerRunning && <Alert role="status"><CircleHelp /><AlertDescription>
            {locale === 'zh' ? '请先退出 ChatGPT、Codex 和正在运行的 Codex 命令行，再重新检查。选中的对话会保留。' : 'Quit ChatGPT, Codex, and any running Codex command-line sessions, then check again. Your selection is preserved.'}
          </AlertDescription></Alert>}
          {planWarnings.length > 0 && <Alert role="status"><AlertTriangle /><AlertDescription>{planWarnings.map((warning) => localizeWarning(warning, locale)).join(locale === 'en' ? '; ' : '；')}</AlertDescription></Alert>}
          {planComplete && <Alert role="status"><Check /><AlertDescription>{t('operationComplete')}</AlertDescription></Alert>}
        </DialogBody>
        <DialogFooter>
          <Button variant="outline" onClick={() => setPlanOpen(false)} disabled={planBusy}>{t('close')}</Button>
          {writerRunning && <Button variant="outline" disabled={planBusy} onClick={() => void openPlan(planKind)}>{locale === 'zh' ? '重新检查' : 'Check again'}</Button>}
          <Button variant={planKind === 'conversation' ? 'destructive' : 'default'} onClick={() => void executePlan()} disabled={planBusy || !plan || Boolean(planError) || planComplete || writerRunning}>{planBusy ? <LoaderCircle className="spin" data-icon="inline-start" /> : planKind === 'conversation' ? <Trash2 data-icon="inline-start" /> : <Database data-icon="inline-start" />}{planComplete ? t('completed') : planKind === 'conversation' ? t('deleteConversations') : t('executePlan')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>

    <Dialog open={Boolean(purgeTarget)} onOpenChange={(open) => { if (!open && !purgeBusy) setPurgeTarget(null); }}><DialogContent showCloseButton={!purgeBusy} closeLabel={t('dialogClose')}><DialogHeader><DialogTitle>{t('purgeRecovery')}</DialogTitle><DialogDescription>{t(purgeTarget?.operation_type === 'temporary' ? 'temporaryPurgeCopy' : 'purgeCopy')}</DialogDescription></DialogHeader><DialogBody className="page-stack"><p><strong>{purgeTarget && t(operationTitleKey(purgeTarget.operation_type), { count: formatNumber(purgeTarget.thread_count), bytes: formatBytes(purgeTarget.reclaimed_bytes) })}</strong><br />{purgeTarget && formatDateTime(purgeTarget.created_at, locale)}<br />{purgeTarget?.selection_summary}</p><label className="month-include"><Checkbox checked={purgeAcknowledged} disabled={purgeBusy} onCheckedChange={(value) => setPurgeAcknowledged(Boolean(value))} />{t('purgeAcknowledgment')}</label>{purgeError && <Alert variant="destructive"><AlertDescription>{purgeError}</AlertDescription></Alert>}{purgeBusy && <p role="status">{t('purging')}</p>}</DialogBody><DialogFooter><Button variant="outline" disabled={purgeBusy} onClick={() => setPurgeTarget(null)}>{t('close')}</Button><Button variant="destructive" disabled={purgeBusy || !purgeAcknowledged} onClick={() => void purgeOperation()}>{purgeBusy ? <LoaderCircle className="spin" /> : <Trash2 />}{t('confirmPurge')}</Button></DialogFooter></DialogContent></Dialog>

  </>;
}

export default App;
