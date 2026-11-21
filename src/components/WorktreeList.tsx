import { useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, LoaderCircle, RefreshCw, Trash2 } from 'lucide-react';
import { Button } from './ui/button';
import { Checkbox } from './ui/checkbox';
import { Alert, AlertDescription } from './ui/alert';
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from './ui/dialog';
import { listWorktrees, removeWorktrees } from '../lib/api';
import { formatDateTime, localizeWorktreeReason, useI18n } from '../lib/i18n';
import { formatBytes } from '../lib/utils';
import type { Worktree } from '../types';

export function WorktreeList({ onChanged }: { onChanged: () => Promise<unknown> }) {
  const { locale, t, formatNumber } = useI18n();
  const [items, setItems] = useState<Worktree[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [selectedPaths, setSelectedPaths] = useState<Set<string>>(new Set());
  const [pending, setPending] = useState<Worktree[]>([]);
  const gate = useRef(false);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const removedFocus = useRef(false);

  async function load() {
    setLoading(true);
    setError('');
    try {
      const next = await listWorktrees();
      // Native scanning already filters these, but keep the renderer honest
      // if a stale or malformed bridge response contains an orphan entry.
      const verified = next.filter((item) => item.main || item.registration_id.trim().length > 0);
      setItems(verified);
      setSelectedPaths((current) => new Set([...current].filter((path) => verified.some((item) => item.path === path && item.can_remove))));
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { void load(); }, []);

  const removable = useMemo(() => items.filter((item) => item.can_remove), [items]);
  const selectedItems = useMemo(() => removable.filter((item) => selectedPaths.has(item.path)), [removable, selectedPaths]);
  const allSelected = removable.length > 0 && selectedItems.length === removable.length;

  function toggleSelected(path: string, checked: boolean) {
    setSelectedPaths((current) => {
      const next = new Set(current);
      if (checked) next.add(path); else next.delete(path);
      return next;
    });
  }

  function openDelete(worktrees: Worktree[]) {
    if (!worktrees.length) return;
    removedFocus.current = false;
    setPending(worktrees);
    setError('');
    setMessage('');
  }

  async function remove() {
    if (!pending.length || gate.current) return;
    gate.current = true;
    setBusy(true);
    setError('');
    setMessage('');
    try {
      const result = await removeWorktrees(pending, true);
      removedFocus.current = result.removed > 0;
      setPending([]);
      setSelectedPaths(new Set());
      await load();
      await onChanged();
      if (result.failed.length > 0) {
        setError(`${t('worktreeRemovalPartial', { removed: result.removed, failed: result.failed.length })} ${result.failed.join('; ')}`);
      } else {
        setMessage(result.removed === 1 ? t('worktreeRemoved') : t('worktreesRemoved', { count: result.removed }));
      }
    } catch (e) {
      setError(String(e));
    } finally {
      gate.current = false;
      setBusy(false);
    }
  }

  function changeSummary(item: Worktree) {
    if (item.dirty === null) return t('unknown');
    if (item.changed_files > 0) return t('worktreeChangedFiles', { count: formatNumber(item.changed_files) });
    if (item.ignored_files > 0) return t('worktreeIgnoredFiles', { count: formatNumber(item.ignored_files) });
    return t('worktreeClean');
  }

  return <div className="page-stack">
    <div className="section-heading-row worktree-heading">
      <div><h2 ref={headingRef} tabIndex={-1}>{t('worktreeTitle')}</h2><details className="worktree-rules cleanup-disclosure"><summary>{locale === 'zh' ? '清理说明' : 'Cleanup rules'}</summary><p>{t('worktreeSafetyCopy')}</p></details></div>
      <div className="worktree-heading-actions">
        <label className="worktree-select-all"><Checkbox checked={allSelected} indeterminate={selectedItems.length > 0 && !allSelected} disabled={!removable.length || loading || busy} onCheckedChange={(value) => setSelectedPaths(value ? new Set(removable.map((item) => item.path)) : new Set())} />{t('selectRemovableWorktrees')}</label>
        <div className="cleanup-row-actions">
          <Button variant="outline" size="sm" disabled={loading || busy} onClick={() => void load()}><RefreshCw />{locale === 'zh' ? '刷新工作树' : 'Refresh worktrees'}</Button>
          <Button variant="destructive" size="sm" disabled={!selectedItems.length || loading || busy} onClick={() => openDelete(selectedItems)}><Trash2 />{t('deleteSelectedWorktrees', { count: formatNumber(selectedItems.length) })}</Button>
        </div>
      </div>
    </div>
    {error && <Alert variant="destructive"><AlertTriangle /><AlertDescription>{error}</AlertDescription></Alert>}
    {message && <Alert role="status"><AlertDescription>{message}</AlertDescription></Alert>}
    <div className="worktree-list cleanup-surface">
      {loading ? <p className="cleanup-empty" role="status"><LoaderCircle className="spin" size={16} />{t('readingLocalData')}</p> : error && items.length === 0 ? <p className="cleanup-empty" role="status">{locale === 'zh' ? '无法读取工作树，请刷新重试。' : 'Unable to read worktrees. Refresh to retry.'}</p> : items.length === 0 ? <p className="cleanup-empty" role="status">{t('noWorktrees')}</p> : items.map((item) => {
        const selected = selectedPaths.has(item.path);
        return <section className={`worktree-row ${selected ? 'is-selected' : ''}`} key={item.path}>
          <label className="worktree-row-check"><Checkbox disabled={!item.can_remove || busy} checked={selected} onCheckedChange={(value) => toggleSelected(item.path, Boolean(value))} aria-label={`${t('selectRemovableWorktrees')}: ${item.path}`} /></label>
          <div className="worktree-main">
            <strong>{item.project || 'Worktree'}</strong>
            <code>{item.path}</code>
            <div className="worktree-facts">
              <span>{t('branch')}: {item.branch || t('unknown')}</span>
              <span className={item.changed_files > 0 ? 'worktree-warning' : ''}>{changeSummary(item)}</span>
              {item.ignored_files > 0 && item.changed_files > 0 && <span>{t('worktreeIgnoredFiles', { count: formatNumber(item.ignored_files) })}</span>}
              {item.created_at_ms !== null && <span>{t('worktreeCreatedAt', { time: formatDateTime(item.created_at_ms, locale) })}</span>}
              {item.updated_at_ms !== null && <span>{t('worktreeUpdatedAt', { time: formatDateTime(item.updated_at_ms, locale) })}</span>}
            </div>
            {!item.can_remove && <small>{localizeWorktreeReason(item.reason, locale)}</small>}
          </div>
          <div className="worktree-side"><strong>{formatBytes(item.bytes)}</strong><Button variant="outline" size="sm" disabled={busy || !item.can_remove} onClick={() => openDelete([item])}><Trash2 />{t('deleteWorktree')}</Button></div>
        </section>;
      })}
    </div>
    <Dialog open={pending.length > 0} onOpenChange={(open) => { if (!open && !busy) setPending([]); }}>
      <DialogContent className="worktree-dialog" showCloseButton={!busy} closeLabel={t('dialogClose')} finalFocus={() => removedFocus.current ? headingRef.current : true}>
        <DialogHeader><DialogTitle>{pending.length > 1 ? t('deleteWorktrees') : t('deleteWorktree')}</DialogTitle><DialogDescription>{t('worktreeDeleteCopy', { count: pending.length })}</DialogDescription></DialogHeader>
        <DialogBody className="page-stack worktree-dialog-body">
          <Alert variant="destructive"><AlertTriangle /><AlertDescription>{t('worktreeDeleteWarning')}</AlertDescription></Alert>
          <div className="worktree-delete-summary"><strong>{t('selectedWorktrees', { count: formatNumber(pending.length) })}</strong><span>{t('worktreeChangedFiles', { count: formatNumber(pending.reduce((sum, item) => sum + item.changed_files, 0)) })}</span><span>{t('worktreeIgnoredFiles', { count: formatNumber(pending.reduce((sum, item) => sum + item.ignored_files, 0)) })}</span></div>
          <ul className="worktree-delete-list">{pending.map((item) => <li key={item.path}><strong>{item.project || 'Worktree'}</strong><code>{item.path}</code><span>{item.branch || t('unknown')} · {formatBytes(item.bytes)}</span></li>)}</ul>
          {error && <Alert variant="destructive"><AlertDescription>{error}</AlertDescription></Alert>}
          {busy && <p role="status">{t('deletingWorktree')}</p>}
        </DialogBody>
        <DialogFooter><Button variant="outline" disabled={busy} onClick={() => setPending([])}>{t('close')}</Button><Button variant="destructive" disabled={busy} onClick={() => void remove()}>{busy ? <LoaderCircle className="spin" /> : <Trash2 />}{pending.length > 1 ? t('confirmDeleteWorktrees') : t('confirmDeleteWorktree')}</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  </div>;
}
