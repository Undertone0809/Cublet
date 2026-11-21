export type View = 'overview' | 'conversations' | 'worktrees' | 'activity' | 'settings';

export type Worktree = {
  path: string;
  project: string;
  branch: string;
  head: string;
  common_dir: string;
  registration_id: string;
  bytes: number;
  changed_files: number;
  ignored_files: number;
  created_at_ms: number | null;
  updated_at_ms: number | null;
  dirty: boolean | null;
  locked: boolean;
  main: boolean;
  can_remove: boolean;
  reason: string;
};

export type WorktreeRemovalResult = {
  removed: number;
  failed: string[];
};

export type Category = {
  id: string;
  label: string;
  bytes: number;
  file_count: number;
  reclaimable_bytes: number;
  safety: string;
};

export type Conversation = {
  id: string;
  title: string;
  cwd: string;
  status: 'current' | 'archived' | 'missing';
  updated_at_ms: number | null;
  bytes: number;
  missing_file: boolean;
  external: boolean;
  rollout_path: string;
};

export type StorageSnapshot = {
  scan_mode?: string;
  omitted_categories?: string[];
  scanned_at: string;
  codex_home: string;
  codex_running: boolean;
  total_bytes: number;
  file_count: number;
  reclaimable_bytes: number;
  categories: Category[];
  conversations: Conversation[];
  warnings: string[];
};

export type CodexStatus = {
  running: boolean;
  process_count: number;
  app_process_count: number;
  cli_process_count: number;
};

export type QuitCodexResult = {
  requested: boolean;
  still_running: boolean;
  message: string;
};

export type ManagedItem = {
  id: string;
  category_id: string;
  label: string;
  path: string;
  bytes: number;
  file_count: number;
  modified_ms: number | null;
  safety: string;
  can_remove: boolean;
};

export type ManagedCleanupPlan = {
  operation_id: string;
  codex_home: string;
  created_at: string;
  category_id: string;
  entries: ManagedItem[];
  estimated_bytes: number;
};

export type ManagedCleanupResult = {
  operation_id: string;
  moved_items: number;
  reclaimed_bytes: number;
  warnings: string[];
};

export type FileFingerprint = {
  path: string;
  size: number;
  modified_ms: number;
  inode: number | null;
};

export type CleanupEntry = {
  thread_id: string;
  title: string;
  rollout_path: string;
  fingerprint: FileFingerprint | null;
  bytes: number;
};

export type CleanupPlan = {
  selection_summary?: string | null;
  operation_id: string;
  codex_home: string;
  created_at: string;
  thread_ids: string[];
  entries: CleanupEntry[];
  database_fingerprints: FileFingerprint[];
  estimated_bytes: number;
};

export type CleanupResult = {
  operation_id: string;
  removed_threads: number;
  moved_files: number;
  reclaimed_bytes: number;
  warnings: string[];
};

export type CleanupOperationSummary = {
  selection_summary?: string | null;
  operation_id: string;
  operation_type: 'cleanup' | 'compaction' | 'managed' | 'temporary';
  created_at: string;
  status: string;
  thread_count: number;
  database_count: number;
  reclaimed_bytes: number;
};

export type SqliteEstimate = {
  path: string;
  page_size: number;
  page_count: number;
  freelist_count: number;
  reclaimable_bytes: number;
};

export type CompactionPlan = {
  operation_id: string;
  codex_home: string;
  created_at: string;
  databases: SqliteEstimate[];
  fingerprints: FileFingerprint[];
  estimated_bytes: number;
};
