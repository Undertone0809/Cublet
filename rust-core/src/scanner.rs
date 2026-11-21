use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

#[derive(Debug, Serialize, Clone)]
pub struct Category {
    pub id: String,
    pub label: String,
    pub bytes: u64,
    pub file_count: u64,
    pub reclaimable_bytes: u64,
    pub safety: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub status: String,
    pub updated_at_ms: Option<i64>,
    pub bytes: u64,
    pub missing_file: bool,
    pub external: bool,
    pub rollout_path: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct StorageSnapshot {
    pub scan_mode: String,
    pub omitted_categories: Vec<String>,
    pub scanned_at: String,
    pub codex_home: String,
    pub codex_running: bool,
    pub total_bytes: u64,
    pub file_count: u64,
    pub reclaimable_bytes: u64,
    pub categories: Vec<Category>,
    pub conversations: Vec<Conversation>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileFingerprint {
    pub path: String,
    pub size: u64,
    pub modified_ms: u64,
    pub inode: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupEntry {
    pub thread_id: String,
    pub title: String,
    pub rollout_path: String,
    pub fingerprint: Option<FileFingerprint>,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupPlan {
    #[serde(default)]
    pub selection_summary: Option<String>,
    pub operation_id: String,
    pub codex_home: String,
    pub created_at: String,
    pub thread_ids: Vec<String>,
    pub entries: Vec<CleanupEntry>,
    pub database_fingerprints: Vec<FileFingerprint>,
    pub estimated_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupResult {
    pub operation_id: String,
    pub removed_threads: usize,
    pub moved_files: usize,
    pub reclaimed_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CleanupOperationSummary {
    pub selection_summary: Option<String>,
    pub operation_id: String,
    pub operation_type: String,
    pub created_at: String,
    pub status: String,
    pub thread_count: usize,
    pub database_count: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SqliteEstimate {
    pub path: String,
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CompactionPlan {
    pub operation_id: String,
    pub codex_home: String,
    pub created_at: String,
    pub databases: Vec<SqliteEstimate>,
    pub fingerprints: Vec<FileFingerprint>,
    pub estimated_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ManagedItem {
    pub id: String,
    pub label: String,
    pub path: String,
    pub bytes: u64,
    pub file_count: u64,
    pub modified_ms: u64,
    pub safety: String,
    pub can_remove: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ManagedPlan {
    pub operation_id: String,
    pub codex_home: String,
    pub created_at: String,
    pub category_id: String,
    pub entries: Vec<ManagedItem>,
    pub estimated_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ManagedResult {
    pub operation_id: String,
    pub moved_items: usize,
    pub reclaimed_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CodexStatus {
    pub running: bool,
    pub process_count: usize,
    pub app_process_count: usize,
    pub cli_process_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct QuitResult {
    pub requested: bool,
    pub still_running: bool,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct Journal {
    #[serde(default)]
    operations: Vec<JournalOperation>,
    #[serde(default)]
    compactions: Vec<JournalCompaction>,
    #[serde(default)]
    managed_operations: Vec<JournalManagedOperation>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JournalOperation {
    operation_id: String,
    codex_home: String,
    quarantine_dir: String,
    plan: CleanupPlan,
    database_backups: Vec<String>,
    moved_files: Vec<(String, String)>,
    #[serde(default)]
    moved_file_count: usize,
    status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JournalCompaction {
    operation_id: String,
    codex_home: String,
    quarantine_dir: String,
    plan: CompactionPlan,
    databases: Vec<JournalCompactionDatabase>,
    status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JournalCompactionDatabase {
    path: String,
    temp_path: String,
    backup_path: String,
    sidecars: Vec<(String, String)>,
    stage: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JournalManagedOperation {
    operation_id: String,
    codex_home: String,
    quarantine_dir: String,
    plan: ManagedPlan,
    moved_items: Vec<(String, String)>,
    #[serde(default)]
    moved_item_count: usize,
    status: String,
}

struct CompactionReplacement {
    path: PathBuf,
    backup: PathBuf,
    sidecars: Vec<(PathBuf, PathBuf)>,
}

#[derive(Default)]
struct SizeTotals {
    logical_bytes: u64,
    allocated_bytes: u64,
    file_count: u64,
}

pub(crate) fn home_codex_path() -> Result<PathBuf, String> {
    if let Some(value) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(value));
    }
    dirs::home_dir()
        .map(|home| home.join(".codex"))
        .ok_or_else(|| "无法确定用户 Home 目录".to_string())
}

fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        metadata.len()
    }
}

#[derive(Hash, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Inode(u64, u64),
    #[cfg(not(unix))]
    Path(PathBuf),
}

fn file_identity(_path: &Path, metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity::Inode(metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        FileIdentity::Path(_path.canonicalize().unwrap_or_else(|_| _path.to_path_buf()))
    }
}

// Only inspect the indexed file. In particular, never traverse its project directory.
fn measurement_path(codex_home: &Path, value: &str) -> PathBuf {
    let path = resolve_codex_path(codex_home, value);
    if let Ok(relative) = path.strip_prefix(codex_home) {
        if let Ok(home) = codex_home.canonicalize() {
            return home.join(relative);
        }
    }
    // macOS exposes these system directories through trusted aliases. Other
    // symlinks, including directory symlinks inside user data, remain rejected.
    #[cfg(target_os = "macos")]
    for alias in ["/var", "/tmp", "/etc"] {
        if let Ok(relative) = path.strip_prefix(alias) {
            if let Ok(root) = Path::new(alias).canonicalize() {
                return root.join(relative);
            }
        }
    }
    path
}

fn measured_record_metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err("记录文件路径包含上级目录跳转".to_string());
        }
        cursor.push(component.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("不统计符号链接记录: {}", cursor.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("无法读取记录文件: {error}")),
        }
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("无法读取记录文件: {error}"))?;
    if !metadata.is_file() {
        return Err("记录路径不是普通文件".to_string());
    }
    Ok(Some(metadata))
}

fn category_for_path(root_id: &str, relative: &Path) -> String {
    if root_id == "electron-support" {
        return "electron-support".to_string();
    }
    if root_id == "electron-cache" {
        return "electron-cache".to_string();
    }
    let file_name = relative.file_name().and_then(|name| name.to_str());
    if file_name.is_some_and(|name| {
        name.ends_with(".sqlite")
            || name.ends_with(".sqlite-wal")
            || name.ends_with(".sqlite-shm")
            || name.ends_with(".db")
    }) {
        return "databases".to_string();
    }

    let first = relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or("other");

    match first {
        "sessions" | "archived_sessions" => "conversations".to_string(),
        "logs_2.sqlite" | "sqlite" | "thread_history_1.sqlite" | "state_5.sqlite" => {
            "databases".to_string()
        }
        "worktrees" => "worktrees".to_string(),
        "automations" => "automations".to_string(),
        "generated_images" | "computer-use" | "visualizations" | "attachments" => {
            "media".to_string()
        }
        "plugins" | "skills" | "vendor_imports" => "extensions".to_string(),
        "cache" | ".tmp" | "tmp" | "shell_snapshots" => "cache".to_string(),
        "backups" => "backups".to_string(),
        "history.jsonl" | "session_index.jsonl" => "conversations".to_string(),
        _ => "other".to_string(),
    }
}

fn scan_root(
    root_id: &str,
    root: &Path,
    totals: &mut BTreeMap<String, SizeTotals>,
    seen: &mut HashSet<FileIdentity>,
    managed_totals: &mut BTreeMap<String, (u64, bool)>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }

    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.map_err(|error| format!("遍历 {} 失败: {error}", root.display()))?;
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let components: Vec<_> = relative
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect();
        let managed_category = if root_id == "codex" {
            ["cache", "automations", "extensions"]
                .into_iter()
                .find(|category| {
                    managed_root_names(category).is_some_and(|roots| {
                        components
                            .first()
                            .is_some_and(|first| roots.contains(first))
                    })
                })
        } else {
            None
        };
        if let Some(category) = managed_category {
            let selected = components
                .get(1)
                .is_some_and(|name| managed_item_name_allowed(category, name));
            let invalid = entry.file_type().is_symlink()
                || (!entry.file_type().is_file() && !entry.file_type().is_dir());
            if invalid && (components.len() <= 2 || selected) {
                managed_totals.entry(category.into()).or_default().1 = true;
            }
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("读取 {} 失败: {error}", entry.path().display()))?;
        if let Some(category) = managed_category {
            if components
                .get(1)
                .is_some_and(|name| managed_item_name_allowed(category, name))
            {
                let item = managed_totals.entry(category.into()).or_default();
                item.0 = item.0.saturating_add(allocated_bytes(&metadata));
            }
        }
        if !seen.insert(file_identity(entry.path(), &metadata)) {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let category = category_for_path(root_id, relative);
        let item = totals.entry(category).or_default();
        item.logical_bytes = item.logical_bytes.saturating_add(metadata.len());
        item.allocated_bytes = item
            .allocated_bytes
            .saturating_add(allocated_bytes(&metadata));
        item.file_count = item.file_count.saturating_add(1);
    }
    Ok(())
}

#[cfg(test)]
fn bytes_for_path(path: &Path) -> Option<u64> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    Some(allocated_bytes(&metadata))
}

fn resolve_codex_path(codex_home: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        codex_home.join(candidate)
    }
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("读取 {} 的文件指纹失败: {error}", path.display()))?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.ino())
    };
    #[cfg(not(unix))]
    let inode = None;
    Ok(FileFingerprint {
        path: path.display().to_string(),
        size: metadata.len(),
        modified_ms,
        inode,
    })
}

fn fingerprint_matches(expected: &FileFingerprint) -> Result<bool, String> {
    let actual = file_fingerprint(Path::new(&expected.path))?;
    Ok(actual.size == expected.size
        && actual.modified_ms == expected.modified_ms
        && (expected.inode.is_none() || actual.inode == expected.inode))
}

fn validate_cleanup_plan_shape(plan: &CleanupPlan, codex_home: &Path) -> Result<(), String> {
    if plan
        .selection_summary
        .as_ref()
        .is_some_and(|summary| summary.len() > 4096)
    {
        return Err("清理计划摘要过长".to_string());
    }
    if !safe_operation_id(&plan.operation_id) {
        return Err("清理计划标识无效".to_string());
    }
    if Path::new(&plan.codex_home) != codex_home {
        return Err("清理计划对应的 Codex 目录已变化，请重新扫描".to_string());
    }
    let entry_ids: std::collections::HashSet<_> = plan
        .entries
        .iter()
        .map(|entry| entry.thread_id.as_str())
        .collect();
    let plan_ids: std::collections::HashSet<_> =
        plan.thread_ids.iter().map(String::as_str).collect();
    if entry_ids != plan_ids
        || entry_ids.len() != plan.entries.len()
        || plan_ids.len() != plan.thread_ids.len()
    {
        return Err("清理计划中的对话集合不一致，请重新生成计划".to_string());
    }
    for entry in &plan.entries {
        let safe_path = safe_rollout_path(codex_home, &entry.rollout_path)?;
        if safe_path.display().to_string() != entry.rollout_path {
            return Err(format!("清理计划路径未通过校验: {}", entry.rollout_path));
        }
        if let Some(fingerprint) = &entry.fingerprint {
            if fingerprint.path != entry.rollout_path {
                return Err("清理计划中的文件指纹与路径不匹配".to_string());
            }
        }
    }
    let allowed_databases: std::collections::HashSet<_> = cleanup_database_paths(codex_home)
        .into_iter()
        .map(|path| path.display().to_string())
        .collect();
    let fingerprint_paths: std::collections::HashSet<_> = plan
        .database_fingerprints
        .iter()
        .map(|fingerprint| fingerprint.path.as_str())
        .collect();
    let allowed_database_paths: std::collections::HashSet<_> =
        allowed_databases.iter().map(String::as_str).collect();
    if fingerprint_paths.len() != plan.database_fingerprints.len()
        || !fingerprint_paths.is_subset(&allowed_database_paths)
    {
        return Err("清理计划中的数据库指纹集合已变化，请重新生成计划".to_string());
    }
    for fingerprint in &plan.database_fingerprints {
        if !allowed_databases.contains(&fingerprint.path) {
            return Err(format!(
                "拒绝处理 Codex 目录外的数据库: {}",
                fingerprint.path
            ));
        }
    }
    Ok(())
}

fn database_paths(codex_home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut collect_sqlite = |directory: &Path| {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "sqlite" || extension == "db")
                && is_allowed_database_path(codex_home, &path)
            {
                paths.push(path);
            }
        }
    };
    collect_sqlite(codex_home);
    collect_sqlite(&codex_home.join("sqlite"));
    paths.sort();
    paths.dedup();
    paths
}

// Keep backup, deletion and restore predicates in one inventory. Ordering
// starts with owning records so restoration satisfies child constraints.
struct ConversationDatabase {
    file: &'static str,
    schema: &'static str,
    tables: &'static [(&'static str, &'static str)],
}
const CONVERSATION_DATABASES: &[ConversationDatabase] = &[
    ConversationDatabase {
        file: "state_5.sqlite",
        schema: "main",
        tables: &[
            ("threads", "id = ?1"),
            ("thread_dynamic_tools", "thread_id = ?1"),
            ("thread_artifacts", "thread_id = ?1"),
            (
                "thread_spawn_edges",
                "parent_thread_id = ?1 OR child_thread_id = ?1",
            ),
            ("thread_attachments", "thread_id = ?1"),
            ("thread_read_receipts", "thread_id = ?1"),
            ("guardian_review_feedback", "thread_id = ?1"),
        ],
    },
    ConversationDatabase {
        file: "thread_history_1.sqlite",
        schema: "history",
        tables: &[
            ("thread_turns", "thread_id = ?1"),
            ("thread_items", "thread_id = ?1"),
            ("thread_history_projection_state", "thread_id = ?1"),
            ("thread_realtime_items", "thread_id = ?1"),
        ],
    },
    ConversationDatabase {
        file: "logs_2.sqlite",
        schema: "logs",
        tables: &[("logs", "thread_id = ?1")],
    },
    ConversationDatabase {
        file: "queue_1.sqlite",
        schema: "queue",
        tables: &[("queued_items", "thread_id = ?1")],
    },
    ConversationDatabase {
        file: "goals_1.sqlite",
        schema: "goals",
        tables: &[
            ("thread_goals", "thread_id = ?1"),
            ("thread_goal_continuation_deferrals", "thread_id = ?1"),
        ],
    },
];

fn cleanup_database_paths(codex_home: &Path) -> Vec<PathBuf> {
    CONVERSATION_DATABASES
        .iter()
        .map(|database| codex_home.join(database.file))
        .filter(|path| {
            fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_file())
                .unwrap_or(false)
        })
        .collect()
}

fn validated_state_database_path(codex_home: &Path) -> Result<PathBuf, String> {
    let path = codex_home.join("state_5.sqlite");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(path),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "拒绝使用 symlink 作为 state_5.sqlite: {}",
            path.display()
        )),
        Ok(_) => Err(format!(
            "拒绝使用非普通文件作为 state_5.sqlite: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("未找到 state_5.sqlite，无法访问对话索引".to_string())
        }
        Err(error) => Err(format!("读取 state_5.sqlite 失败: {error}")),
    }
}

fn recovery_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("recovery").join("codex-storage-manager")
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(format!(
                    "拒绝使用非真实目录作为 {label}: {}",
                    path.display()
                ));
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("读取 {label} 失败: {error}")),
    }
    fs::create_dir(path).map_err(|error| format!("创建 {label} 失败: {error}"))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("读取新建 {label} 失败: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(format!("新建的 {label} 不是真实目录: {}", path.display()));
    }
    Ok(())
}

fn validate_real_directory_if_present(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(format!(
                    "拒绝使用非真实目录作为 {label}: {}",
                    path.display()
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("读取 {label} 失败: {error}")),
    }
}

fn ensure_recovery_dir(codex_home: &Path) -> Result<PathBuf, String> {
    let recovery_root = codex_home.join("recovery");
    ensure_real_directory(&recovery_root, "恢复根目录")?;
    let directory = recovery_dir(codex_home);
    ensure_real_directory(&directory, "恢复目录")?;
    Ok(directory)
}

fn validate_recovery_dir(codex_home: &Path) -> Result<PathBuf, String> {
    let recovery_root = codex_home.join("recovery");
    validate_real_directory_if_present(&recovery_root, "恢复根目录")?;
    let directory = recovery_dir(codex_home);
    validate_real_directory_if_present(&directory, "恢复目录")?;
    Ok(directory)
}

fn operation_owner_count(journal: &Journal, id: &str) -> usize {
    journal
        .operations
        .iter()
        .filter(|op| op.operation_id == id)
        .count()
        + journal
            .compactions
            .iter()
            .filter(|op| op.operation_id == id)
            .count()
        + journal
            .managed_operations
            .iter()
            .filter(|op| op.operation_id == id)
            .count()
}

fn require_unused_operation_id(home: &Path, id: &str) -> Result<(), String> {
    if !safe_operation_id(id)
        || operation_owner_count(&read_journal(home)?, id) != 0
        || fs::symlink_metadata(recovery_dir(home).join(id)).is_ok()
    {
        return Err(
            "Operation ID or recovery directory already exists; create a fresh plan".into(),
        );
    }
    Ok(())
}

fn require_unique_recovery_owner(journal: &Journal, id: &str) -> Result<(), String> {
    if operation_owner_count(journal, id) != 1 {
        return Err("Ambiguous recovery operation ID; no files changed".into());
    }
    Ok(())
}

fn ensure_operation_dir(codex_home: &Path, operation_id: &str) -> Result<PathBuf, String> {
    if !safe_operation_id(operation_id) {
        return Err("操作标识无效".to_string());
    }
    let directory = ensure_recovery_dir(codex_home)?.join(operation_id);
    ensure_real_directory(&directory, "操作隔离目录")?;
    Ok(directory)
}

fn ensure_directory_chain(root: &Path, directory: &Path, label: &str) -> Result<(), String> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| format!("{label}不在允许目录内: {}", directory.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!("{label}路径包含无效组件: {}", directory.display()));
        };
        current.push(name);
        ensure_real_directory(&current, label)?;
    }
    Ok(())
}

fn validate_directory_chain(root: &Path, directory: &Path, label: &str) -> Result<(), String> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| format!("{label}不在允许目录内: {}", directory.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(format!("{label}路径包含无效组件: {}", directory.display()));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(format!(
                        "拒绝使用非真实目录作为 {label}: {}",
                        current.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("读取 {label} 失败: {error}")),
        }
    }
    Ok(())
}

fn validate_operation_directory(path: &Path, label: &str) -> Result<(), String> {
    validate_real_directory_if_present(path, label)
}

fn validate_regular_file_if_present(path: &Path, label: &str) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("拒绝使用 symlink 作为 {label}: {}", path.display()))
        }
        Ok(metadata) if !metadata.file_type().is_file() => Err(format!(
            "拒绝使用非普通文件作为 {label}: {}",
            path.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("读取 {label} 失败: {error}")),
    }
}

fn journal_path(codex_home: &Path) -> PathBuf {
    recovery_dir(codex_home).join("journal.json")
}

fn read_journal(codex_home: &Path) -> Result<Journal, String> {
    validate_recovery_dir(codex_home)?;
    let path = journal_path(codex_home);
    if !validate_regular_file_if_present(&path, "恢复日志")? {
        return Ok(Journal::default());
    }
    let content =
        fs::read_to_string(&path).map_err(|error| format!("读取恢复日志失败: {error}"))?;
    serde_json::from_str(&content).map_err(|error| format!("解析恢复日志失败: {error}"))
}

fn write_journal(codex_home: &Path, journal: &Journal) -> Result<(), String> {
    let dir = ensure_recovery_dir(codex_home)?;
    let path = journal_path(codex_home);
    let temp = path.with_extension("json.tmp");
    validate_regular_file_if_present(&path, "恢复日志")?;
    validate_regular_file_if_present(&temp, "恢复日志临时文件")?;
    let payload = serde_json::to_vec_pretty(journal)
        .map_err(|error| format!("序列化恢复日志失败: {error}"))?;
    let mut file = fs::File::create(&temp).map_err(|error| format!("写入恢复日志失败: {error}"))?;
    file.write_all(&payload)
        .map_err(|error| format!("写入恢复日志失败: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("同步恢复日志失败: {error}"))?;
    fs::rename(&temp, &path).map_err(|error| format!("提交恢复日志失败: {error}"))?;
    #[cfg(unix)]
    fs::File::open(&dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("同步恢复目录失败: {error}"))?;
    Ok(())
}

fn operation_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    format!("{prefix}-{millis}-{}", std::process::id())
}

fn codex_process_count() -> usize {
    #[cfg(not(test))]
    {
        crate::processes::counts().map(|(a, c)| a + c).unwrap_or(1)
    }
    #[cfg(test)]
    {
        0
    }
}

fn writer_is_running() -> bool {
    codex_process_count() > 0
}

pub fn get_codex_status() -> Result<CodexStatus, String> {
    let (app_process_count, cli_process_count) = crate::processes::counts()?;
    let process_count = app_process_count + cli_process_count;
    Ok(CodexStatus {
        running: process_count > 0,
        process_count,
        app_process_count,
        cli_process_count,
    })
}

pub fn quit_codex() -> Result<QuitResult, String> {
    quit_codex_impl()
}

fn quit_codex_impl() -> Result<QuitResult, String> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let (app, cli) = crate::processes::counts()?;
        if app == 0 {
            return Ok(QuitResult {
                requested: false,
                still_running: cli > 0,
                message: if cli > 0 {
                    format!("Codex App 未运行；仍有 {cli} 个 CLI / IDE Codex 进程。请在终端退出 Codex 或停用 IDE 中的 Codex 后重新检查。不会退出 Cursor 或 ChatGPT。")
                } else {
                    "Codex 已停止运行".into()
                },
            });
        }
        let result = crate::processes::bounded_output(
            Command::new("/usr/bin/osascript").args([
                "-e",
                "if application \"Codex\" is running then tell application \"Codex\" to quit",
            ]),
            Duration::from_secs(8),
        )?;
        if !result.status.success() {
            return Err(format!(
                "Codex 未接受退出请求: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        let started = std::time::Instant::now();
        loop {
            let (app, cli) = crate::processes::counts()?;
            if app == 0 || started.elapsed() >= Duration::from_secs(5) {
                let still_running = app + cli > 0;
                return Ok(QuitResult {
                    requested: true,
                    still_running,
                    message: if still_running {
                        format!("仍在运行：App {app}，CLI / IDE {cli}。请手动退出后重新检查；未强制结束任何进程。")
                    } else {
                        "Codex 已退出".into()
                    },
                });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    #[cfg(any(not(target_os = "macos"), test))]
    {
        Err("Graceful quit is only available on macOS (disabled in tests)".into())
    }
}
pub(crate) fn ensure_writer_stopped() -> Result<(), String> {
    #[cfg(test)]
    {
        Ok(())
    }
    #[cfg(not(test))]
    {
        if writer_is_running() {
            return Err("检测到 Codex 仍在运行，请先退出 Codex 后再执行清理或压缩".to_string());
        }
        Ok(())
    }
}

fn safe_rollout_path(codex_home: &Path, value: &str) -> Result<PathBuf, String> {
    let path = resolve_codex_path(codex_home, value);
    let canonical_home = codex_home
        .canonicalize()
        .map_err(|error| format!("解析 Codex 目录失败: {error}"))?;
    let canonical = if path.exists() {
        path.canonicalize()
            .map_err(|error| format!("解析对话记录文件失败: {error}"))?
    } else {
        let mut cursor = path.as_path();
        let mut missing = Vec::new();
        while !cursor.exists() {
            let Some(name) = cursor.file_name() else {
                break;
            };
            missing.push(name.to_os_string());
            cursor = cursor
                .parent()
                .ok_or_else(|| format!("解析对话记录文件失败: {}", path.display()))?;
        }
        let mut canonical = cursor
            .canonicalize()
            .map_err(|error| format!("解析对话记录文件失败: {error}"))?;
        for name in missing.iter().rev() {
            canonical.push(name);
        }
        canonical
    };
    if !canonical.starts_with(&canonical_home) {
        return Err(format!("拒绝处理 Codex 目录外的路径: {}", path.display()));
    }
    let relative = canonical
        .strip_prefix(&canonical_home)
        .unwrap_or(&canonical);
    let allowed_root = relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .map(|value| value == "sessions" || value == "archived_sessions")
        .unwrap_or(false);
    if !allowed_root {
        return Err(format!("拒绝处理非对话目录中的文件: {}", path.display()));
    }
    Ok(path)
}

fn safe_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

fn managed_root_names(category_id: &str) -> Option<&'static [&'static str]> {
    match category_id {
        "cache" => Some(&["cache", ".tmp", "tmp", "shell_snapshots"]),
        "automations" => Some(&["automations"]),
        "extensions" => Some(&["plugins", "skills", "vendor_imports"]),
        _ => None,
    }
}

fn managed_root_path(codex_home: &Path, root_name: &str) -> PathBuf {
    codex_home.join(root_name)
}

fn managed_item_name_allowed(category_id: &str, name: &str) -> bool {
    match category_id {
        "cache" => true,
        // Keep automation definitions; only generated output buckets are movable.
        "automations" => matches!(
            name,
            "runs" | "artifacts" | "outputs" | "output" | "history" | "logs"
        ),
        // Plugin and skill definitions remain in place; package/cache buckets can be rebuilt.
        "extensions" => matches!(
            name,
            ".cache"
                | "cache"
                | "dist"
                | "build"
                | "node_modules"
                | "vendor"
                | "downloads"
                | "packages"
        ),
        _ => false,
    }
}

fn validate_managed_root(codex_home: &Path, root_name: &str) -> Result<Option<PathBuf>, String> {
    let path = managed_root_path(codex_home, root_name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "拒绝使用 symlink 作为 managed 根目录: {}",
            path.display()
        )),
        Ok(metadata) if !metadata.file_type().is_dir() => Err(format!(
            "拒绝使用非目录作为 managed 根目录: {}",
            path.display()
        )),
        Ok(_) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取 managed 根目录失败: {error}")),
    }
}

fn managed_path_from_id(
    codex_home: &Path,
    category_id: &str,
    item_id: &str,
) -> Result<PathBuf, String> {
    let roots =
        managed_root_names(category_id).ok_or_else(|| format!("类别不可管理: {category_id}"))?;
    let relative = Path::new(item_id);
    if relative.is_absolute() {
        return Err(format!("managed 项目路径必须是相对路径: {item_id}"));
    }
    let components: Vec<_> = relative.components().collect();
    if components.len() != 2 {
        return Err(format!("managed 项目必须是根目录下的直接项目: {item_id}"));
    }
    let std::path::Component::Normal(root_component) = components[0] else {
        return Err(format!("managed 项目根目录无效: {item_id}"));
    };
    let std::path::Component::Normal(item_component) = components[1] else {
        return Err(format!("managed 项目名称无效: {item_id}"));
    };
    let root_name = root_component
        .to_str()
        .ok_or_else(|| format!("managed 项目根目录不是有效文本: {item_id}"))?;
    if !roots.contains(&root_name) {
        return Err(format!("managed 项目不属于类别 {category_id}: {item_id}"));
    }
    if item_component.to_str().is_none() {
        return Err(format!("managed 项目名称不是有效文本: {item_id}"));
    }
    let item_name = item_component.to_str().unwrap_or_default();
    if !managed_item_name_allowed(category_id, item_name) {
        return Err(format!("managed 项目受保护，不能移除: {item_id}"));
    }
    let _ = validate_managed_root(codex_home, root_name)?;
    Ok(managed_root_path(codex_home, root_name).join(item_component))
}

fn managed_item_id(codex_home: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(codex_home)
        .map(|relative| relative.display().to_string())
        .map_err(|_| format!("managed 项目不在 Codex 目录内: {}", path.display()))
}

fn modified_ms_from_metadata(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn measure_managed_path(path: &Path) -> Result<(u64, u64, u64), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("读取 managed 项目失败 {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("拒绝处理 managed symlink: {}", path.display()));
    }
    if metadata.file_type().is_file() {
        return Ok((
            allocated_bytes(&metadata),
            1,
            modified_ms_from_metadata(&metadata),
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(format!("拒绝处理 managed 非普通文件: {}", path.display()));
    }
    let mut bytes = 0_u64;
    let mut file_count = 0_u64;
    for entry in WalkDir::new(path).follow_links(false).into_iter() {
        let entry = entry.map_err(|error| format!("遍历 managed 项目失败: {error}"))?;
        let entry_type = entry.file_type();
        if entry_type.is_symlink() {
            return Err(format!(
                "拒绝处理 managed symlink: {}",
                entry.path().display()
            ));
        }
        if entry_type.is_file() {
            let metadata = entry.metadata().map_err(|error| {
                format!("读取 managed 项目失败 {}: {error}", entry.path().display())
            })?;
            bytes = bytes.saturating_add(allocated_bytes(&metadata));
            file_count = file_count.saturating_add(1);
        } else if !entry_type.is_dir() {
            return Err(format!(
                "拒绝处理 managed 非普通文件: {}",
                entry.path().display()
            ));
        }
    }
    Ok((bytes, file_count, modified_ms_from_metadata(&metadata)))
}

fn validate_managed_path_if_present(path: &Path, label: &str) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "拒绝使用 managed symlink 作为 {label}: {}",
            path.display()
        )),
        Ok(metadata) if !metadata.file_type().is_file() && !metadata.file_type().is_dir() => {
            Err(format!(
                "拒绝使用 managed 非普通文件作为 {label}: {}",
                path.display()
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("读取 managed {label} 失败: {error}")),
    }
}

fn validate_managed_source_if_present(
    codex_home: &Path,
    path: &Path,
    label: &str,
) -> Result<bool, String> {
    let present = validate_managed_path_if_present(path, label)?;
    let canonical_home = codex_home
        .canonicalize()
        .map_err(|error| format!("解析 Codex 目录失败: {error}"))?;
    let canonical = if present {
        path.canonicalize()
            .map_err(|error| format!("解析 managed {label} 失败: {error}"))?
    } else {
        let mut cursor = path;
        let mut missing = Vec::new();
        let mut canonical = loop {
            match fs::symlink_metadata(cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "拒绝使用 managed symlink 作为 {label}: {}",
                        cursor.display()
                    ));
                }
                Ok(_) => {
                    break cursor
                        .canonicalize()
                        .map_err(|error| format!("解析 managed {label} 失败: {error}"))?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let name = cursor
                        .file_name()
                        .ok_or_else(|| format!("解析 managed {label} 失败: {}", path.display()))?;
                    missing.push(name.to_os_string());
                    cursor = cursor
                        .parent()
                        .ok_or_else(|| format!("解析 managed {label} 失败: {}", path.display()))?;
                }
                Err(error) => return Err(format!("读取 managed {label} 失败: {error}")),
            }
        };
        for name in missing.iter().rev() {
            canonical.push(name);
        }
        canonical
    };
    if !canonical.starts_with(&canonical_home) {
        return Err(format!(
            "拒绝处理 Codex 目录外的 managed {label}: {}",
            path.display()
        ));
    }
    Ok(present)
}

fn managed_item_from_path(
    codex_home: &Path,
    category_id: &str,
    path: &Path,
) -> Result<ManagedItem, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("读取 managed 项目失败 {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("拒绝处理 managed symlink: {}", path.display()));
    }
    let _ = validate_managed_source_if_present(codex_home, path, "项目")?;
    let id = managed_item_id(codex_home, path)?;
    let (bytes, file_count, modified_ms) = measure_managed_path(path)?;
    let label = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("managed 项目名称无效: {}", path.display()))?;
    if !managed_item_name_allowed(category_id, label) {
        return Err(format!("managed 项目受保护，不能移除: {}", path.display()));
    }
    Ok(ManagedItem {
        id,
        label: label.to_string(),
        path: path.display().to_string(),
        bytes,
        file_count,
        modified_ms,
        safety: "managed:可管理".to_string(),
        can_remove: true,
    })
}

fn managed_items(codex_home: &Path, category_id: &str) -> Result<Vec<ManagedItem>, String> {
    let roots =
        managed_root_names(category_id).ok_or_else(|| format!("类别不可管理: {category_id}"))?;
    let mut items = Vec::new();
    for root_name in roots {
        let Some(root) = validate_managed_root(codex_home, root_name)? else {
            continue;
        };
        for entry in fs::read_dir(&root)
            .map_err(|error| format!("读取 managed 根目录失败 {}: {error}", root.display()))?
        {
            let entry = entry.map_err(|error| format!("读取 managed 项目失败: {error}"))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                format!("读取 managed 项目类型失败 {}: {error}", path.display())
            })?;
            if file_type.is_symlink() {
                return Err(format!("拒绝处理 managed symlink: {}", path.display()));
            }
            if !file_type.is_file() && !file_type.is_dir() {
                return Err(format!("拒绝处理 managed 非普通文件: {}", path.display()));
            }
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                return Err(format!("managed 项目名称无效: {}", path.display()));
            };
            if !managed_item_name_allowed(category_id, name) {
                continue;
            }
            items.push(managed_item_from_path(codex_home, category_id, &path)?);
        }
    }
    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(items)
}

fn validate_managed_plan_shape(plan: &ManagedPlan, codex_home: &Path) -> Result<(), String> {
    if !safe_operation_id(&plan.operation_id) {
        return Err("managed 清理计划标识无效".to_string());
    }
    if Path::new(&plan.codex_home) != codex_home {
        return Err("managed 清理计划对应的 Codex 目录已变化，请重新扫描".to_string());
    }
    if plan.entries.is_empty() {
        return Err("至少选择一个 managed 项目".to_string());
    }
    let mut ids = std::collections::HashSet::new();
    let mut estimated = 0_u64;
    for item in &plan.entries {
        if !ids.insert(item.id.as_str()) {
            return Err(format!("managed 清理计划包含重复项目: {}", item.id));
        }
        let path = managed_path_from_id(codex_home, &plan.category_id, &item.id)?;
        if path.display().to_string() != item.path {
            return Err(format!("managed 清理计划路径无效: {}", item.path));
        }
        if item.safety != "managed:可管理" || !item.can_remove {
            return Err(format!("managed 项目不可操作: {}", item.id));
        }
        estimated = estimated.saturating_add(item.bytes);
    }
    if estimated != plan.estimated_bytes {
        return Err("managed 清理计划估算空间已变化，请重新生成计划".to_string());
    }
    Ok(())
}

fn validate_managed_plan(plan: &ManagedPlan, codex_home: &Path) -> Result<(), String> {
    validate_managed_plan_shape(plan, codex_home)?;
    for item in &plan.entries {
        let path = managed_path_from_id(codex_home, &plan.category_id, &item.id)?;
        let current = managed_item_from_path(codex_home, &plan.category_id, &path)?;
        if current.bytes != item.bytes
            || current.file_count != item.file_count
            || current.modified_ms != item.modified_ms
        {
            return Err(format!("managed 项目在计划创建后发生变化: {}", item.path));
        }
    }
    Ok(())
}

fn expected_managed_destination(
    quarantine: &Path,
    codex_home: &Path,
    source: &Path,
) -> Result<PathBuf, String> {
    let relative = source
        .strip_prefix(codex_home)
        .map_err(|_| format!("managed 项目不在 Codex 目录内: {}", source.display()))?;
    Ok(quarantine.join("managed").join(relative))
}

fn validate_managed_journal_operation(
    operation: &JournalManagedOperation,
    codex_home: &Path,
) -> Result<(), String> {
    validate_managed_plan_shape(&operation.plan, codex_home)?;
    if operation.operation_id != operation.plan.operation_id
        || !safe_operation_id(&operation.operation_id)
    {
        return Err("managed 恢复日志中的操作标识无效".to_string());
    }
    if Path::new(&operation.codex_home) != codex_home {
        return Err("managed 恢复日志对应的 Codex 目录已变化".to_string());
    }
    let expected_quarantine = recovery_dir(codex_home).join(&operation.operation_id);
    if Path::new(&operation.quarantine_dir) != expected_quarantine {
        return Err("managed 恢复日志中的隔离目录无效".to_string());
    }
    validate_operation_directory(&expected_quarantine, "managed 隔离目录")?;
    if operation.moved_item_count > operation.moved_items.len()
        || operation.moved_items.len() != operation.plan.entries.len()
    {
        return Err("managed 恢复日志中的项目集合不完整".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for (source, destination) in &operation.moved_items {
        let source_path = Path::new(source);
        let item = operation
            .plan
            .entries
            .iter()
            .find(|item| item.path == *source)
            .ok_or_else(|| format!("managed 恢复日志包含未知项目: {source}"))?;
        let expected_source =
            managed_path_from_id(codex_home, &operation.plan.category_id, &item.id)?;
        if expected_source != source_path {
            return Err(format!("managed 恢复日志中的源路径无效: {source}"));
        }
        let expected_destination =
            expected_managed_destination(&expected_quarantine, codex_home, source_path)?;
        if Path::new(destination) != expected_destination {
            return Err(format!("managed 恢复日志中的隔离路径无效: {destination}"));
        }
        let destination_parent = Path::new(destination)
            .parent()
            .ok_or_else(|| format!("managed 隔离路径无效: {destination}"))?;
        validate_directory_chain(&expected_quarantine, destination_parent, "managed 隔离目录")?;
        if validate_managed_source_if_present(codex_home, source_path, "源路径")? {
            let _ = measure_managed_path(source_path)?;
        }
        if validate_managed_path_if_present(Path::new(destination), "隔离路径")? {
            let _ = measure_managed_path(Path::new(destination))?;
        }
        if !seen.insert(source) {
            return Err(format!("managed 恢复日志包含重复项目: {source}"));
        }
    }
    if !matches!(
        operation.status.as_str(),
        "prepared" | "moving" | "completed" | "failed" | "restoring" | "restored"
    ) {
        return Err(format!(
            "managed 恢复日志中的状态无效: {}",
            operation.status
        ));
    }
    Ok(())
}

fn table_exists(connection: &Connection, schema: &str, table: &str) -> Result<bool, String> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name=?1)"
    );
    connection
        .query_row(&sql, params![table], |row| row.get::<_, i64>(0))
        .map(|value| value != 0)
        .map_err(|error| format!("检查表 {schema}.{table} 失败: {error}"))
}

fn sqlite_estimate(path: &Path) -> Result<SqliteEstimate, String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("读取数据库 {} 失败: {error}", path.display()))?;
    let page_size = connection
        .query_row("PRAGMA page_size", [], |row| row.get::<_, u64>(0))
        .map_err(|error| format!("读取数据库页大小失败: {error}"))?;
    let page_count = connection
        .query_row("PRAGMA page_count", [], |row| row.get::<_, u64>(0))
        .map_err(|error| format!("读取数据库页数失败: {error}"))?;
    let freelist_count = connection
        .query_row("PRAGMA freelist_count", [], |row| row.get::<_, u64>(0))
        .map_err(|error| format!("读取数据库空闲页失败: {error}"))?;
    Ok(SqliteEstimate {
        path: path.display().to_string(),
        page_size,
        page_count,
        freelist_count,
        reclaimable_bytes: page_size.saturating_mul(freelist_count),
    })
}

fn read_conversations(codex_home: &Path, warnings: &mut Vec<String>) -> Vec<Conversation> {
    let database_path = match validated_state_database_path(codex_home) {
        Ok(path) => path,
        Err(error) => {
            warnings.push(error.replace("无法访问对话索引", "无法建立对话索引"));
            return Vec::new();
        }
    };

    let connection =
        match Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(connection) => connection,
            Err(error) => {
                warnings.push(format!("读取对话索引失败: {error}"));
                return Vec::new();
            }
        };
    if let Err(error) = set_cleanup_busy_timeout(&connection) {
        warnings.push(format!("设置对话索引忙等待超时失败: {error}"));
        return Vec::new();
    }

    let query = "SELECT id, title, cwd, archived, updated_at_ms, rollout_path FROM threads ORDER BY recency_at_ms DESC, id DESC";
    let mut statement = match connection.prepare(query) {
        Ok(statement) => statement,
        Err(error) => {
            warnings.push(format!("当前 Codex 数据库 schema 不兼容: {error}"));
            return Vec::new();
        }
    };

    let rows = match statement.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let cwd: String = row.get(2)?;
        let archived: i64 = row.get(3)?;
        let updated_at_ms: Option<i64> = row.get(4)?;
        let rollout_path: String = row.get(5)?;
        Ok((id, title, cwd, archived, updated_at_ms, rollout_path))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            warnings.push(format!("读取对话索引失败: {error}"));
            return Vec::new();
        }
    };

    let mut conversations = Vec::new();
    let canonical_home = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    for (index, row) in rows.enumerate() {
        let (id, title, cwd, archived, updated_at_ms, rollout_path) = match row {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("跳过第 {} 条损坏的对话索引: {error}", index + 1));
                continue;
            }
        };
        conversations.push({
            let path = measurement_path(codex_home, &rollout_path);
            let external = !path.starts_with(&canonical_home);
            let measurement = if external {
                measured_record_metadata(&path)
            } else {
                safe_rollout_path(codex_home, &rollout_path)
                    .and_then(|_| measured_record_metadata(&path))
            };
            let metadata = match measurement {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!(
                        "对话 {} 的记录文件路径无效，已跳过占用统计: {error}",
                        id
                    ));
                    None
                }
            };
            let bytes = metadata.as_ref().map(allocated_bytes);
            let missing_file = bytes.is_none();
            Conversation {
                id,
                title: if title.trim().is_empty() {
                    "未命名对话".to_string()
                } else {
                    title
                },
                cwd,
                status: if missing_file {
                    "missing".to_string()
                } else if archived == 1 {
                    "archived".to_string()
                } else {
                    "current".to_string()
                },
                updated_at_ms,
                bytes: bytes.unwrap_or_default(),
                missing_file,
                external,
                rollout_path,
            }
        });
    }
    conversations
}

fn category_label(id: &str) -> (&'static str, &'static str, &'static str) {
    match id {
        "conversations" => ("对话", "可管理", "managed"),
        "external-conversations" => ("外部对话记录", "仅统计", "protected"),
        "databases" => ("数据库", "退出 Codex 后", "conditional"),
        "worktrees" => ("Worktrees", "仅检查", "protected"),
        "automations" => ("自动化产物", "可管理", "managed"),
        "media" => ("图片与附件", "仅检查", "protected"),
        "extensions" => ("插件与技能", "可管理", "managed"),
        "cache" => ("缓存", "可管理", "managed"),
        "electron-cache" => ("缓存", "后续版本", "conditional"),
        "electron-support" => ("Electron 数据", "谨慎操作", "conditional"),
        "backups" => ("备份", "需确认", "conditional"),
        _ => ("其他", "未知", "unknown"),
    }
}

pub fn scan_storage() -> Result<StorageSnapshot, String> {
    scan_storage_impl(false)
}

pub fn scan_storage_fast() -> Result<StorageSnapshot, String> {
    scan_storage_impl(true)
}

fn scan_storage_impl(fast: bool) -> Result<StorageSnapshot, String> {
    let codex_home = home_codex_path()?;
    if !codex_home.exists() {
        return Err(format!("Codex 数据目录不存在: {}", codex_home.display()));
    }

    let mut totals: BTreeMap<String, SizeTotals> = BTreeMap::new();
    let mut measured_managed = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut warnings = Vec::new();
    let mut omitted_categories = Vec::new();
    if fast {
        // A metadata preview never recursively measures project/runtime trees.
        // Only top-level files and indexed conversation files are measured.
        for entry in fs::read_dir(&codex_home).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            if seen.insert(file_identity(&entry.path(), &metadata)) {
                let item = totals
                    .entry(category_for_path("codex", Path::new(&entry.file_name())))
                    .or_default();
                item.logical_bytes += metadata.len();
                item.allocated_bytes += allocated_bytes(&metadata);
                item.file_count += 1;
            }
        }
        omitted_categories = [
            "worktrees",
            "automations",
            "media",
            "extensions",
            "cache",
            "electron-support",
            "electron-cache",
            "backups",
            "other",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        warnings.push("Quick preview: totals include top-level files and indexed conversation records only. Nested worktrees, runtimes, caches and attachments are not measured; run detailed scan for complete sizes.".into());
    } else {
        scan_root(
            "codex",
            &codex_home,
            &mut totals,
            &mut seen,
            &mut measured_managed,
        )?;
    }

    match read_journal(&codex_home) {
        Ok(journal) => {
            for operation in journal.compactions.iter().filter(|operation| {
                operation.status == "prepared"
                    || operation.status == "running"
                    || operation.status == "restoring"
            }) {
                if let Err(error) = validate_compaction_journal_operation(operation, &codex_home) {
                    warnings.push(format!(
                        "发现未完成的数据库压缩，但恢复日志校验失败 ({}): {error}",
                        operation.operation_id
                    ));
                } else {
                    warnings.push(format!(
                        "数据库压缩操作 {} 未完成，请在活动记录中执行恢复",
                        operation.operation_id
                    ));
                }
            }
            for operation in journal.managed_operations.iter().filter(|operation| {
                operation.status == "prepared"
                    || operation.status == "moving"
                    || operation.status == "restoring"
            }) {
                if let Err(error) = validate_managed_journal_operation(operation, &codex_home) {
                    warnings.push(format!(
                        "发现未完成的 managed 清理，但恢复日志校验失败 ({}): {error}",
                        operation.operation_id
                    ));
                } else {
                    warnings.push(format!(
                        "managed 清理操作 {} 未完成，请在活动记录中执行恢复",
                        operation.operation_id
                    ));
                }
            }
        }
        Err(error) => warnings.push(error),
    }

    if let Some(home) = dirs::home_dir().filter(|_| !fast) {
        let support = home.join("Library/Application Support/Codex");
        let cache = home.join("Library/Caches/Codex");
        scan_root(
            "electron-support",
            &support,
            &mut totals,
            &mut seen,
            &mut measured_managed,
        )?;
        scan_root(
            "electron-cache",
            &cache,
            &mut totals,
            &mut seen,
            &mut measured_managed,
        )?;
    }

    let conversations = read_conversations(&codex_home, &mut warnings);
    for conversation in conversations
        .iter()
        .filter(|item| (fast || item.external) && !item.missing_file)
    {
        let path = measurement_path(&codex_home, &conversation.rollout_path);
        if let Ok(Some(metadata)) = measured_record_metadata(&path) {
            if seen.insert(file_identity(&path, &metadata)) {
                let item = totals
                    .entry(
                        if conversation.external {
                            "external-conversations"
                        } else {
                            "conversations"
                        }
                        .to_string(),
                    )
                    .or_default();
                item.logical_bytes = item.logical_bytes.saturating_add(metadata.len());
                item.allocated_bytes = item
                    .allocated_bytes
                    .saturating_add(allocated_bytes(&metadata));
                item.file_count = item.file_count.saturating_add(1);
            }
        }
    }
    let total_bytes = totals.values().map(|item| item.allocated_bytes).sum();
    let conversation_cleanup_supported = match ensure_conversation_schema_supported(&codex_home) {
        Ok(()) => true,
        Err(error) => {
            warnings.push(error);
            false
        }
    };
    let conversation_bytes = conversations
        .iter()
        .filter(|_| conversation_cleanup_supported)
        .filter(|item| !item.missing_file && !item.external)
        .map(|item| item.bytes)
        .sum::<u64>();
    let mut database_reclaimable_bytes = 0_u64;
    for path in if fast {
        Vec::new()
    } else {
        database_paths(&codex_home)
    } {
        match sqlite_estimate(&path) {
            Ok(estimate) => {
                database_reclaimable_bytes =
                    database_reclaimable_bytes.saturating_add(estimate.reclaimable_bytes);
            }
            Err(error) => warnings.push(format!(
                "无法估算数据库 {} 的空闲页: {error}",
                path.display()
            )),
        }
    }
    let mut managed_reclaimable_bytes = BTreeMap::new();
    for (category, (bytes, invalid)) in &measured_managed {
        if *invalid {
            warnings.push(format!("Cannot validate managed category {category}; protected links or special files were retained"));
        } else {
            managed_reclaimable_bytes.insert(category.as_str(), *bytes);
        }
    }
    let mut categories = Vec::new();
    let empty_totals = SizeTotals::default();

    for id in [
        "conversations",
        "external-conversations",
        "databases",
        "worktrees",
        "automations",
        "media",
        "extensions",
        "cache",
        "electron-support",
        "electron-cache",
        "backups",
        "other",
    ] {
        let item = totals.get(id).unwrap_or(&empty_totals);
        let (label, safety_label, safety) = category_label(id);
        let reclaimable_bytes = if id == "conversations" {
            conversation_bytes
        } else if id == "databases" {
            database_reclaimable_bytes
        } else {
            managed_reclaimable_bytes
                .get(id)
                .copied()
                .unwrap_or_default()
        };
        categories.push(Category {
            id: id.to_string(),
            label: label.to_string(),
            bytes: item.allocated_bytes,
            file_count: item.file_count,
            reclaimable_bytes,
            safety: if id == "conversations" && !conversation_cleanup_supported {
                "protected:当前版本仅统计".into()
            } else {
                format!("{safety}:{safety_label}")
            },
        });
    }

    if conversations.iter().any(|item| item.missing_file) {
        warnings.push("部分对话索引指向的对话记录文件已不存在".to_string());
    }
    if total_bytes == 0 {
        warnings.push("扫描到了目录，但未发现可计量文件".to_string());
    }

    Ok(StorageSnapshot {
        scan_mode: if fast { "quick" } else { "detailed" }.into(),
        omitted_categories,
        scanned_at: Utc::now().to_rfc3339(),
        codex_home: codex_home.display().to_string(),
        codex_running: writer_is_running(),
        total_bytes,
        file_count: totals.values().map(|item| item.file_count).sum(),
        reclaimable_bytes: categories.iter().map(|item| item.reclaimable_bytes).sum(),
        categories,
        conversations,
        warnings,
    })
}

fn ensure_conversation_schema_supported(home: &Path) -> Result<(), String> {
    for entry in fs::read_dir(home).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let newer = [
            ("state_", 5),
            ("thread_history_", 1),
            ("queue_", 1),
            ("goals_", 1),
            ("logs_", 2),
        ]
        .into_iter()
        .any(|(prefix, supported)| {
            name.strip_prefix(prefix)
                .and_then(|name| name.strip_suffix(".sqlite"))
                .and_then(|version| version.parse::<u32>().ok())
                .is_some_and(|version| version > supported)
        });
        if newer {
            return Err(format!("Conversation cleanup is unavailable for newer Codex schema {name}. No conversation data was changed."));
        }
    }
    for database in CONVERSATION_DATABASES {
        let file = database.file;
        let Some(path) = validated_database_path_if_present(home, &home.join(file), file)? else {
            continue;
        };
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("{file}: {e}"))?;
        connection
            .busy_timeout(Duration::from_millis(100))
            .map_err(|e| e.to_string())?;
        let mut stmt = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .map_err(|e| format!("{file}: {e}"))?;
        let tables = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for table in tables {
            let table = table.map_err(|e| e.to_string())?;
            let quoted = table.replace('"', "\"\"");
            let mut columns_stmt = connection
                .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))
                .map_err(|e| e.to_string())?;
            let columns = columns_stmt
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let related = table.starts_with("thread_")
                || columns
                    .iter()
                    .any(|c| c == "thread_id" || c.ends_with("_thread_id"));
            let known = database.tables.iter().find(|(name, _)| *name == table);
            let valid = if let Some((_, predicate)) = known {
                predicate.split(" OR ").all(|part| {
                    columns
                        .iter()
                        .any(|name| name == part.split_whitespace().next().unwrap_or(""))
                }) && (table != "thread_read_receipts"
                    || columns.iter().any(|name| name == "revision"))
            } else {
                !related
                    || (file == "queue_1.sqlite" && table == "queued_thread_revisions")
                    || (file == "state_5.sqlite"
                        && matches!(
                            table.as_str(),
                            "thread_sections" | "rollout_migration_state"
                        ))
            };
            if !valid {
                return Err(format!("Conversation cleanup is unavailable for newer Codex schema {file}.{table}. No conversation data was changed."));
            }
        }
    }
    Ok(())
}

fn ensure_attached_children_selected(home: &Path, thread_ids: &[String]) -> Result<(), String> {
    let path = validated_state_database_path(home)?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    set_cleanup_busy_timeout(&connection)?;
    let has_attachment: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('threads') WHERE name='attached_thread_parent_id')",
        [], |row| row.get(0),
    ).map_err(|e| e.to_string())?;
    if !has_attachment {
        return Ok(());
    }
    let selected: std::collections::HashSet<_> = thread_ids.iter().collect();
    let mut children = connection
        .prepare("SELECT id FROM threads WHERE attached_thread_parent_id = ?1")
        .map_err(|e| e.to_string())?;
    for id in thread_ids {
        let rows = children
            .query_map(params![id], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for child in rows {
            if !selected.contains(&child.map_err(|e| e.to_string())?) {
                return Err(
                    "选中的对话包含未选中的关联对话。请同时选择关联对话，再创建删除计划。".into(),
                );
            }
        }
    }
    Ok(())
}

pub fn create_cleanup_plan(thread_ids: Vec<String>) -> Result<CleanupPlan, String> {
    if thread_ids.is_empty() {
        return Err("至少选择一个对话".to_string());
    }
    let codex_home = home_codex_path()?;
    ensure_conversation_schema_supported(&codex_home)?;
    let conversations = read_conversations(&codex_home, &mut Vec::new());
    let requested: std::collections::HashSet<_> = thread_ids.iter().cloned().collect();
    let mut entries = Vec::new();
    for conversation in conversations
        .into_iter()
        .filter(|item| requested.contains(&item.id))
    {
        if conversation.external {
            return Err("外部对话记录仅统计占用，暂不支持删除外部记录".to_string());
        }
        let path = safe_rollout_path(&codex_home, &conversation.rollout_path)?;
        let fingerprint = if path.exists() {
            Some(file_fingerprint(&path)?)
        } else {
            None
        };
        entries.push(CleanupEntry {
            thread_id: conversation.id,
            title: conversation.title,
            rollout_path: path.display().to_string(),
            fingerprint,
            bytes: conversation.bytes,
        });
    }
    if entries.len() != requested.len() {
        return Err("部分对话已不存在或不属于当前 Codex 数据目录，请重新扫描".to_string());
    }
    ensure_attached_children_selected(&codex_home, &thread_ids)?;
    let database_fingerprints = cleanup_database_paths(&codex_home)
        .iter()
        .map(|path| file_fingerprint(path))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CleanupPlan {
        selection_summary: None,
        operation_id: operation_id("cleanup"),
        codex_home: codex_home.display().to_string(),
        created_at: Utc::now().to_rfc3339(),
        thread_ids,
        estimated_bytes: entries.iter().map(|entry| entry.bytes).sum(),
        entries,
        database_fingerprints,
    })
}

pub fn create_managed_cleanup_plan(
    category_id: String,
    item_ids: Option<Vec<String>>,
) -> Result<ManagedPlan, String> {
    let codex_home = home_codex_path()?;
    let available = managed_items(&codex_home, &category_id)?;
    let selected = match item_ids {
        None => available,
        Some(item_ids) if item_ids.is_empty() => {
            return Err("至少选择一个 managed 项目".to_string());
        }
        Some(item_ids) => {
            let requested: std::collections::HashSet<_> =
                item_ids.iter().map(String::as_str).collect();
            if requested.len() != item_ids.len() {
                return Err("managed 清理计划包含重复项目".to_string());
            }
            let selected: Vec<_> = available
                .into_iter()
                .filter(|item| requested.contains(item.id.as_str()))
                .collect();
            if selected.len() != requested.len() {
                return Err("部分 managed 项目不存在或不属于指定类别，请重新扫描".to_string());
            }
            selected
        }
    };
    if selected.is_empty() {
        return Err("指定类别没有可管理项目".to_string());
    }
    Ok(ManagedPlan {
        operation_id: operation_id("managed"),
        codex_home: codex_home.display().to_string(),
        created_at: Utc::now().to_rfc3339(),
        category_id,
        estimated_bytes: selected.iter().map(|item| item.bytes).sum(),
        entries: selected,
    })
}

fn rollback_managed_moves(codex_home: &Path, moved: &[(String, String)]) {
    for (source, destination) in moved.iter().rev() {
        let source_path = Path::new(source);
        let destination_path = Path::new(destination);
        let destination_present = fs::symlink_metadata(destination_path).is_ok();
        let source_present = fs::symlink_metadata(source_path).is_ok();
        if destination_present && !source_present {
            if let Some(parent) = source_path.parent() {
                let _ = ensure_directory_chain(codex_home, parent, "managed 回滚目录");
            }
            let _ = fs::rename(destination_path, source_path);
        }
    }
}

pub fn execute_managed_cleanup_plan(plan: ManagedPlan) -> Result<ManagedResult, String> {
    let _mutation_guard = mutation_guard()?;
    ensure_writer_stopped()?;
    let codex_home = home_codex_path()?;
    validate_managed_plan(&plan, &codex_home)?;
    require_unused_operation_id(&codex_home, &plan.operation_id)?;
    let quarantine = ensure_operation_dir(&codex_home, &plan.operation_id)?;
    let mut journal = read_journal(&codex_home)?;
    if journal
        .operations
        .iter()
        .any(|item| item.operation_id == plan.operation_id)
        || journal
            .compactions
            .iter()
            .any(|item| item.operation_id == plan.operation_id)
        || journal
            .managed_operations
            .iter()
            .any(|item| item.operation_id == plan.operation_id)
    {
        return Err("managed 操作标识已存在，请重新生成计划".to_string());
    }
    let mut operation = JournalManagedOperation {
        operation_id: plan.operation_id.clone(),
        codex_home: codex_home.display().to_string(),
        quarantine_dir: quarantine.display().to_string(),
        plan: plan.clone(),
        moved_items: plan
            .entries
            .iter()
            .map(|item| {
                let source = managed_path_from_id(&codex_home, &plan.category_id, &item.id)?;
                let destination = expected_managed_destination(&quarantine, &codex_home, &source)?;
                Ok((
                    source.display().to_string(),
                    destination.display().to_string(),
                ))
            })
            .collect::<Result<Vec<_>, String>>()?,
        moved_item_count: 0,
        status: "prepared".to_string(),
    };
    let persist = |journal: &mut Journal, operation: &JournalManagedOperation| {
        if let Some(existing) = journal
            .managed_operations
            .iter_mut()
            .find(|item| item.operation_id == operation.operation_id)
        {
            *existing = operation.clone();
        } else {
            journal.managed_operations.push(operation.clone());
        }
        write_journal(&codex_home, journal)
    };
    persist(&mut journal, &operation)?;

    operation.status = "moving".to_string();
    persist(&mut journal, &operation)?;
    let move_result = (|| -> Result<(), String> {
        for (source, destination) in operation.moved_items.clone() {
            let source_path = PathBuf::from(&source);
            let destination_path = PathBuf::from(&destination);
            let destination_parent = destination_path
                .parent()
                .ok_or_else(|| format!("managed 隔离路径无效: {destination}"))?;
            ensure_directory_chain(&quarantine, destination_parent, "managed 隔离目录")?;
            if fs::symlink_metadata(&destination_path).is_ok() {
                return Err(format!(
                    "managed 隔离目标已存在，请重新生成计划: {destination}"
                ));
            }
            if !validate_managed_source_if_present(&codex_home, &source_path, "源路径")? {
                return Err(format!("managed 源路径已不存在，请重新生成计划: {source}"));
            }
            let _ = measure_managed_path(&source_path)?;
            fs::rename(&source_path, &destination_path)
                .map_err(|error| format!("隔离 managed 项目失败: {error}"))?;
            operation.moved_item_count += 1;
            persist(&mut journal, &operation)?;
        }
        operation.status = "completed".to_string();
        persist(&mut journal, &operation)
    })();
    if let Err(error) = move_result {
        rollback_managed_moves(&codex_home, &operation.moved_items);
        operation.status = "failed".to_string();
        let _ = persist(&mut journal, &operation);
        return Err(error);
    }
    Ok(ManagedResult {
        operation_id: plan.operation_id,
        moved_items: operation.moved_item_count,
        reclaimed_bytes: plan.estimated_bytes,
        warnings: Vec::new(),
    })
}

pub fn restore_managed_cleanup_operation(operation_id: String) -> Result<ManagedResult, String> {
    let _mutation_guard = mutation_guard()?;
    ensure_writer_stopped()?;
    let codex_home = home_codex_path()?;
    let mut journal = read_journal(&codex_home)?;
    require_unique_recovery_owner(&journal, &operation_id)?;
    if journal.operations.iter().any(|op| {
        op.operation_id == operation_id && matches!(op.status.as_str(), "purging" | "purged")
    }) {
        return Err("Recovery was permanently cleared or is being cleared".into());
    }
    let mut operation = journal
        .managed_operations
        .iter()
        .find(|item| item.operation_id == operation_id)
        .cloned()
        .ok_or_else(|| "找不到可恢复的 managed 清理操作".to_string())?;
    if operation.status == "restored" {
        return Err("managed 清理操作已经恢复".to_string());
    }
    validate_managed_journal_operation(&operation, &codex_home)?;
    operation.status = "restoring".to_string();
    if let Some(existing) = journal
        .managed_operations
        .iter_mut()
        .find(|item| item.operation_id == operation_id)
    {
        *existing = operation.clone();
    }
    write_journal(&codex_home, &journal)?;

    let mut restored = 0;
    for (source, destination) in &operation.moved_items {
        let source_path = Path::new(source);
        let destination_path = Path::new(destination);
        let source_present =
            validate_managed_source_if_present(&codex_home, source_path, "源路径")?;
        let destination_present = validate_managed_path_if_present(destination_path, "隔离路径")?;
        if source_present && destination_present {
            return Err(format!("managed 恢复目标已存在且隔离副本仍在: {source}"));
        }
        if !source_present && !destination_present {
            return Err(format!("managed 隔离项目缺失，无法恢复: {destination}"));
        }
        if source_present {
            continue;
        }
        let parent = source_path
            .parent()
            .ok_or_else(|| format!("managed 恢复目录无效: {source}"))?;
        ensure_directory_chain(&codex_home, parent, "managed 恢复目录")?;
        fs::rename(destination_path, source_path)
            .map_err(|error| format!("恢复 managed 项目失败: {error}"))?;
        restored += 1;
        operation.moved_item_count = restored;
        if let Some(existing) = journal
            .managed_operations
            .iter_mut()
            .find(|item| item.operation_id == operation_id)
        {
            *existing = operation.clone();
        }
        write_journal(&codex_home, &journal)?;
    }
    operation.status = "restored".to_string();
    if let Some(existing) = journal
        .managed_operations
        .iter_mut()
        .find(|item| item.operation_id == operation_id)
    {
        *existing = operation;
    }
    write_journal(&codex_home, &journal)?;
    Ok(ManagedResult {
        operation_id,
        moved_items: restored,
        reclaimed_bytes: 0,
        warnings: Vec::new(),
    })
}

fn verify_cleanup_plan(plan: &CleanupPlan, codex_home: &Path) -> Result<(), String> {
    validate_cleanup_plan_shape(plan, codex_home)?;
    if plan.database_fingerprints.len() != cleanup_database_paths(codex_home).len() {
        return Err("清理计划中的数据库指纹集合已变化，请重新生成计划".into());
    }
    let conversations = read_conversations(codex_home, &mut Vec::new());
    for entry in &plan.entries {
        let current = conversations
            .iter()
            .find(|item| item.id == entry.thread_id)
            .ok_or_else(|| format!("对话索引已变化: {}", entry.thread_id))?;
        let current_path = safe_rollout_path(codex_home, &current.rollout_path)?;
        let safe_path = safe_rollout_path(codex_home, &entry.rollout_path)?;
        if safe_path.display().to_string() != entry.rollout_path {
            return Err(format!("清理计划路径未通过校验: {}", entry.rollout_path));
        }
        if current_path != safe_path {
            return Err(format!(
                "对话 {} 的记录文件路径已变化，请重新生成计划",
                entry.thread_id
            ));
        }
        if current.missing_file != entry.fingerprint.is_none() {
            return Err(format!("对话 {} 的记录文件状态已变化", entry.thread_id));
        }
        if let Some(fingerprint) = &entry.fingerprint {
            if !fingerprint_matches(fingerprint)? {
                return Err(format!("文件在计划创建后发生变化: {}", fingerprint.path));
            }
        }
    }
    let allowed_databases: std::collections::HashSet<_> = cleanup_database_paths(codex_home)
        .into_iter()
        .map(|path| path.display().to_string())
        .collect();
    for fingerprint in &plan.database_fingerprints {
        if !allowed_databases.contains(&fingerprint.path) {
            return Err(format!(
                "拒绝处理 Codex 目录外的数据库: {}",
                fingerprint.path
            ));
        }
        if !fingerprint_matches(fingerprint)? {
            return Err(format!("数据库在计划创建后发生变化: {}", fingerprint.path));
        }
    }
    Ok(())
}

fn checkpoint_wal(path: &Path) -> Result<(), String> {
    let mut has_sidecar = false;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if validate_regular_file_if_present(&sidecar, "数据库附属文件")? {
            has_sidecar = true;
        }
    }
    if !has_sidecar {
        return Ok(());
    }
    let connection =
        Connection::open(path).map_err(|error| format!("打开数据库以同步 WAL 失败: {error}"))?;
    let busy = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("同步数据库 WAL 失败: {error}"))?;
    if busy != 0 {
        return Err(format!("数据库仍有 {busy} 个 WAL 检查点任务未完成"));
    }
    Ok(())
}

fn set_cleanup_busy_timeout(connection: &Connection) -> Result<(), String> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("设置数据库忙等待超时失败: {error}"))
}

fn backup_thread_database(
    path: &Path,
    destination: &Path,
    thread_ids: &[String],
    tables: &[(&str, &str)],
) -> Result<(), String> {
    let source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| format!("打开日志数据库失败: {error}"))?;
    set_cleanup_busy_timeout(&source)?;
    let backup =
        Connection::open(destination).map_err(|error| format!("创建日志备份失败: {error}"))?;
    set_cleanup_busy_timeout(&backup)?;
    backup
        .execute(
            "ATTACH DATABASE ?1 AS source",
            params![path.display().to_string()],
        )
        .map_err(|error| format!("打开日志源数据库失败: {error}"))?;
    for (table, _) in tables {
        if table_exists(&source, "main", table)? {
            let create =
                format!("CREATE TABLE main.{table} AS SELECT * FROM source.{table} WHERE 0");
            backup
                .execute_batch(&create)
                .map_err(|error| format!("创建数据库备份表 {table} 失败: {error}"))?;
        }
    }
    let transaction = backup
        .unchecked_transaction()
        .map_err(|error| format!("开始数据库行备份事务失败: {error}"))?;
    for (table, predicate) in tables {
        if !table_exists(&source, "main", table)? {
            continue;
        }
        let insert =
            format!("INSERT INTO main.{table} SELECT * FROM source.{table} WHERE {predicate}");
        for thread_id in thread_ids {
            transaction
                .execute(&insert, params![thread_id])
                .map_err(|error| format!("备份 {table} 对话行失败: {error}"))?;
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("提交数据库行备份失败: {error}"))?;
    Ok(())
}

fn backup_database(
    path: &Path,
    quarantine: &Path,
    thread_ids: &[String],
) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("无法备份数据库 {}", path.display()))?;
    let destination = quarantine.join(format!("backup-{file_name}"));
    if validate_regular_file_if_present(&destination, "数据库备份目标")? {
        return Err(format!(
            "数据库备份目标已存在，请重新生成计划: {}",
            destination.display()
        ));
    }
    let database = CONVERSATION_DATABASES
        .iter()
        .find(|database| database.file == file_name)
        .ok_or("Unrecognized conversation database")?;
    backup_thread_database(path, &destination, thread_ids, database.tables)?;
    Ok(Some(destination.display().to_string()))
}

fn rollback_moves(codex_home: &Path, moved: &[(String, String)]) {
    for (source, destination) in moved.iter().rev() {
        let source_path = Path::new(source);
        let destination_path = Path::new(destination);
        if destination_path.exists() && !source_path.exists() {
            if let Some(parent) = source_path.parent() {
                let _ = ensure_directory_chain(codex_home, parent, "对话回滚目录");
            }
            let _ = fs::rename(destination_path, source_path);
        }
    }
}

fn validate_journal_operation(
    operation: &JournalOperation,
    codex_home: &Path,
) -> Result<(), String> {
    validate_cleanup_plan_shape(&operation.plan, codex_home)?;
    if operation.operation_id != operation.plan.operation_id
        || !safe_operation_id(&operation.operation_id)
    {
        return Err("恢复日志中的操作标识无效".to_string());
    }
    if Path::new(&operation.codex_home) != codex_home {
        return Err("恢复日志对应的 Codex 目录已变化".to_string());
    }
    let expected_quarantine = recovery_dir(codex_home).join(&operation.operation_id);
    if Path::new(&operation.quarantine_dir) != expected_quarantine {
        return Err("恢复日志中的隔离目录无效".to_string());
    }
    validate_operation_directory(&expected_quarantine, "清理隔离目录")?;
    if operation.moved_file_count > operation.moved_files.len() {
        return Err("恢复日志中的已移动文件数量无效".to_string());
    }
    if !matches!(
        operation.status.as_str(),
        "prepared"
            | "backing_up"
            | "moving"
            | "files_moved"
            | "updating_database"
            | "purging"
            | "purged"
            | "completed"
            | "failed"
            | "restoring"
            | "restored"
    ) {
        return Err(format!("恢复日志中的状态无效: {}", operation.status));
    }
    for backup in &operation.database_backups {
        let path = Path::new(backup);
        if path.parent() != Some(expected_quarantine.as_path())
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("backup-"))
        {
            return Err(format!("恢复日志中的数据库备份路径无效: {backup}"));
        }
        validate_regular_file_if_present(path, "数据库备份")?;
    }
    for (source, destination) in &operation.moved_files {
        let source_path = safe_rollout_path(codex_home, source)?;
        if source_path.display().to_string() != *source {
            return Err(format!("恢复日志中的源文件路径无效: {source}"));
        }
        let relative = source_path
            .strip_prefix(codex_home)
            .map_err(|_| format!("恢复日志中的源文件不在 Codex 目录内: {source}"))?;
        let expected_destination = expected_quarantine.join("files").join(relative);
        if Path::new(destination) != expected_destination {
            return Err(format!("恢复日志中的隔离文件路径无效: {destination}"));
        }
        let destination_parent = Path::new(destination)
            .parent()
            .ok_or_else(|| format!("恢复日志中的隔离文件路径无效: {destination}"))?;
        validate_directory_chain(&expected_quarantine, destination_parent, "清理隔离文件目录")?;
        validate_regular_file_if_present(Path::new(destination), "清理隔离文件")?;
    }
    Ok(())
}

fn is_allowed_database_path(codex_home: &Path, path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let parent_allowed = parent == codex_home || parent == codex_home.join("sqlite");
    if !parent_allowed
        || !path
            .extension()
            .is_some_and(|extension| extension == "sqlite" || extension == "db")
    {
        return false;
    }
    if parent == codex_home {
        let Ok(parent_metadata) = fs::metadata(parent) else {
            return false;
        };
        if !parent_metadata.is_dir() {
            return false;
        }
    } else {
        let Ok(parent_metadata) = fs::symlink_metadata(parent) else {
            return false;
        };
        if !parent_metadata.file_type().is_dir() {
            return false;
        }
    }
    let Ok(canonical_home) = codex_home.canonicalize() else {
        return false;
    };
    let Ok(canonical_parent) = parent.canonicalize() else {
        return false;
    };
    if !canonical_parent.starts_with(&canonical_home) {
        return false;
    }
    if !path.exists() {
        return true;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    let Ok(canonical_path) = path.canonicalize() else {
        return false;
    };
    canonical_path.starts_with(canonical_home)
}

fn validated_database_path_if_present(
    codex_home: &Path,
    path: &Path,
    label: &str,
) -> Result<Option<PathBuf>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("拒绝使用 symlink 作为 {label}: {}", path.display()))
        }
        Ok(metadata) if !metadata.file_type().is_file() => Err(format!(
            "拒绝使用非普通文件作为 {label}: {}",
            path.display()
        )),
        Ok(_) if !is_allowed_database_path(codex_home, path) => {
            Err(format!("拒绝使用不允许的 {label}: {}", path.display()))
        }
        Ok(_) => Ok(Some(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取 {label} 失败: {error}")),
    }
}

fn validate_compaction_journal_operation(
    operation: &JournalCompaction,
    codex_home: &Path,
) -> Result<(), String> {
    if operation.operation_id != operation.plan.operation_id
        || !safe_operation_id(&operation.operation_id)
    {
        return Err("压缩恢复日志中的操作标识无效".to_string());
    }
    if Path::new(&operation.codex_home) != codex_home
        || Path::new(&operation.plan.codex_home) != codex_home
    {
        return Err("压缩恢复日志对应的 Codex 目录已变化".to_string());
    }
    let expected_quarantine = recovery_dir(codex_home).join(&operation.operation_id);
    if Path::new(&operation.quarantine_dir) != expected_quarantine {
        return Err("压缩恢复日志中的隔离目录无效".to_string());
    }
    validate_operation_directory(&expected_quarantine, "压缩隔离目录")?;
    if operation.databases.len() > operation.plan.databases.len()
        || matches!(operation.status.as_str(), "completed" | "restored")
            && operation.databases.len() != operation.plan.databases.len()
    {
        return Err("压缩恢复日志中的数据库集合不完整".to_string());
    }

    let plan_paths: std::collections::HashSet<_> = operation
        .plan
        .databases
        .iter()
        .map(|database| database.path.as_str())
        .collect();
    let mut seen = std::collections::HashSet::new();
    for entry in &operation.databases {
        let path = Path::new(&entry.path);
        let Some(_database) = operation
            .plan
            .databases
            .iter()
            .find(|database| database.path == entry.path)
        else {
            return Err(format!("压缩恢复日志包含未知数据库: {}", entry.path));
        };
        if !is_allowed_database_path(codex_home, path) {
            return Err(format!("压缩恢复日志中的数据库路径无效: {}", entry.path));
        };
        if !seen.insert(entry.path.as_str()) {
            return Err(format!("压缩恢复日志包含重复数据库: {}", entry.path));
        }
        let (expected_temp, expected_backup) =
            expected_compaction_artifact_paths(&expected_quarantine, codex_home, path)?;
        if Path::new(&entry.temp_path) != expected_temp
            || Path::new(&entry.backup_path) != expected_backup
        {
            return Err(format!(
                "压缩恢复日志中的数据库备份路径无效: {}",
                entry.path
            ));
        }
        let artifact_parent = expected_temp
            .parent()
            .ok_or_else(|| format!("压缩恢复目录无效: {}", expected_temp.display()))?;
        validate_directory_chain(&expected_quarantine, artifact_parent, "数据库恢复目录")?;
        validate_regular_file_if_present(&expected_temp, "数据库压缩临时文件")?;
        validate_regular_file_if_present(&expected_backup, "数据库原始备份")?;
        if !matches!(
            entry.stage.as_str(),
            "prepared"
                | "vacuuming"
                | "vacuumed"
                | "verified"
                | "database_backed_up"
                | "sidecars_backing_up"
                | "sidecars_backed_up"
                | "replaced"
                | "restored"
        ) {
            return Err(format!("压缩恢复日志中的阶段无效: {}", entry.stage));
        }
        for (original, saved) in &entry.sidecars {
            let valid_suffix = ["-wal", "-shm"].iter().any(|suffix| {
                original == &format!("{}{}", entry.path, suffix)
                    && saved == &format!("{}{}", entry.backup_path, suffix)
            });
            if !valid_suffix {
                return Err(format!("压缩恢复日志中的 sidecar 路径无效: {}", original));
            }
            validate_regular_file_if_present(Path::new(saved), "数据库附属文件备份")?;
            validate_regular_file_if_present(Path::new(original), "数据库附属文件")?;
        }
    }
    if seen.iter().any(|path| !plan_paths.contains(path)) {
        return Err("压缩恢复日志包含未知数据库".to_string());
    }
    if !matches!(
        operation.status.as_str(),
        "prepared" | "running" | "restoring" | "completed" | "restored" | "failed"
    ) {
        return Err(format!("压缩恢复日志中的状态无效: {}", operation.status));
    }
    Ok(())
}

fn replace_compaction_journal_entry(journal: &mut Journal, operation: &JournalCompaction) {
    if let Some(existing) = journal
        .compactions
        .iter_mut()
        .find(|item| item.operation_id == operation.operation_id)
    {
        *existing = operation.clone();
    } else {
        journal.compactions.push(operation.clone());
    }
}

fn replace_cleanup_journal_entry(journal: &mut Journal, operation: &JournalOperation) {
    if let Some(existing) = journal
        .operations
        .iter_mut()
        .find(|item| item.operation_id == operation.operation_id)
    {
        *existing = operation.clone();
    } else {
        journal.operations.push(operation.clone());
    }
}

fn rollback_compaction_stage(
    path: &Path,
    backup: &Path,
    sidecars: &[(PathBuf, PathBuf)],
    temp: &Path,
) {
    if path.exists() {
        let failed = backup.with_extension("failed");
        let _ = fs::rename(path, failed);
    }
    if backup.exists() && !path.exists() {
        let _ = fs::rename(backup, path);
    }
    for (original, saved) in sidecars.iter().rev() {
        if saved.exists() && !original.exists() {
            let _ = fs::rename(saved, original);
        }
    }
    let _ = fs::remove_file(temp);
}

fn expected_compaction_artifact_paths(
    quarantine: &Path,
    codex_home: &Path,
    path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let relative = path
        .strip_prefix(codex_home)
        .map_err(|_| format!("数据库不在 Codex 目录内: {}", path.display()))?;
    let file_name = relative
        .file_name()
        .ok_or_else(|| format!("数据库路径无效: {}", path.display()))?;
    let artifact_dir = quarantine
        .join("databases")
        .join(relative.parent().unwrap_or_else(|| Path::new("")));
    Ok((
        artifact_dir.join(format!("{}.vacuum", file_name.to_string_lossy())),
        artifact_dir.join(file_name),
    ))
}

fn compaction_artifact_paths(
    quarantine: &Path,
    codex_home: &Path,
    path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let (temp, backup) = expected_compaction_artifact_paths(quarantine, codex_home, path)?;
    let artifact_parent = temp
        .parent()
        .ok_or_else(|| format!("数据库恢复目录无效: {}", temp.display()))?;
    ensure_directory_chain(codex_home, artifact_parent, "数据库恢复目录")?;
    Ok((temp, backup))
}

fn attach_conversation_database(
    connection: &Connection,
    path: &Path,
    schema: &str,
) -> Result<(), String> {
    connection
        .execute(
            &format!("ATTACH DATABASE ?1 AS {schema}"),
            params![path.display().to_string()],
        )
        .map_err(|error| format!("打开会话数据库 {schema} 失败: {error}"))?;
    Ok(())
}

fn apply_conversation_deletions(
    connection: &Connection,
    database: &ConversationDatabase,
    thread_ids: &[String],
) -> Result<(), String> {
    for (table, predicate) in database.tables.iter().rev() {
        if !table_exists(connection, database.schema, table)? {
            continue;
        }
        let sql = format!("DELETE FROM {}.{table} WHERE {predicate}", database.schema);
        for id in thread_ids {
            connection
                .execute(&sql, params![id])
                .map_err(|error| format!("清理 {}.{table} 失败: {error}", database.file))?;
        }
    }
    Ok(())
}

pub fn execute_cleanup_plan(plan: CleanupPlan) -> Result<CleanupResult, String> {
    let _mutation_guard = mutation_guard()?;
    ensure_writer_stopped()?;
    let codex_home = home_codex_path()?;
    ensure_conversation_schema_supported(&codex_home)?;
    validated_state_database_path(&codex_home)?;
    verify_cleanup_plan(&plan, &codex_home)?;
    ensure_attached_children_selected(&codex_home, &plan.thread_ids)?;
    require_unused_operation_id(&codex_home, &plan.operation_id)?;
    let quarantine = ensure_operation_dir(&codex_home, &plan.operation_id)?;
    let mut operation = JournalOperation {
        operation_id: plan.operation_id.clone(),
        codex_home: codex_home.display().to_string(),
        quarantine_dir: quarantine.display().to_string(),
        plan: plan.clone(),
        database_backups: Vec::new(),
        moved_files: Vec::new(),
        moved_file_count: 0,
        status: "prepared".to_string(),
    };
    let mut journal = read_journal(&codex_home)?;
    if journal
        .operations
        .iter()
        .any(|item| item.operation_id == operation.operation_id)
    {
        return Err("清理操作标识已存在，请重新生成计划".to_string());
    }

    let persist = |journal: &mut Journal, operation: &JournalOperation| {
        replace_cleanup_journal_entry(journal, operation);
        write_journal(&codex_home, journal)
    };
    persist(&mut journal, &operation)?;

    operation.status = "backing_up".to_string();
    persist(&mut journal, &operation)?;
    let backup_result = (|| -> Result<(), String> {
        for path in cleanup_database_paths(&codex_home) {
            match backup_database(&path, &quarantine, &plan.thread_ids) {
                Ok(Some(backup)) => {
                    operation.database_backups.push(backup);
                    persist(&mut journal, &operation)?;
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })();
    if let Err(error) = backup_result {
        operation.status = "failed".to_string();
        let _ = persist(&mut journal, &operation);
        return Err(error);
    }

    let transaction_result = (|| -> Result<(), String> {
        ensure_writer_stopped()?;
        verify_cleanup_plan(&plan, &codex_home)?;
        ensure_attached_children_selected(&codex_home, &plan.thread_ids)?;
        let state_path = validated_state_database_path(&codex_home)?;
        let mut state = Connection::open(&state_path)
            .map_err(|error| format!("打开 state_5.sqlite 失败: {error}"))?;
        set_cleanup_busy_timeout(&state)?;
        state
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|error| format!("初始化数据库约束失败: {error}"))?;
        let mut attached = Vec::new();
        for database in CONVERSATION_DATABASES {
            if let Some(path) = validated_database_path_if_present(
                &codex_home,
                &codex_home.join(database.file),
                database.file,
            )? {
                if database.schema != "main" {
                    attach_conversation_database(&state, &path, database.schema)?;
                }
                attached.push(database);
            }
        }
        let transaction = state
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| format!("开始清理事务失败: {error}"))?;
        ensure_writer_stopped()?;
        let mut planned_moves = Vec::new();
        for entry in &plan.entries {
            let source = safe_rollout_path(&codex_home, &entry.rollout_path)?;
            if !source.exists() {
                continue;
            }
            let relative = source
                .strip_prefix(&codex_home)
                .map_err(|_| "清理文件不在 Codex 目录内".to_string())?;
            let destination = quarantine.join("files").join(relative);
            planned_moves.push((
                source.display().to_string(),
                destination.display().to_string(),
            ));
        }
        operation.moved_files = planned_moves;
        operation.status = "moving".to_string();
        persist(&mut journal, &operation)?;

        for (source, destination) in operation.moved_files.clone() {
            let source_path = PathBuf::from(&source);
            let destination_path = PathBuf::from(&destination);
            let destination_parent = destination_path
                .parent()
                .ok_or_else(|| format!("清理隔离文件路径无效: {destination}"))?;
            ensure_directory_chain(&quarantine, destination_parent, "清理隔离文件目录")?;
            match fs::symlink_metadata(&destination_path) {
                Ok(_) => {
                    return Err(format!(
                        "清理隔离文件目标已存在，请重新生成计划: {destination}"
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("检查清理隔离文件失败: {error}")),
            }
            if !source_path.exists() {
                continue;
            }
            fs::rename(&source_path, &destination_path)
                .map_err(|error| format!("隔离对话记录文件失败: {error}"))?;
            operation.moved_file_count += 1;
            persist(&mut journal, &operation)?;
        }
        operation.status = "files_moved".to_string();
        persist(&mut journal, &operation)?;
        operation.status = "updating_database".to_string();
        persist(&mut journal, &operation)?;
        for database in attached {
            apply_conversation_deletions(&transaction, database, &plan.thread_ids)?;
        }
        transaction
            .commit()
            .map_err(|error| format!("提交清理事务失败: {error}"))
    })();
    if let Err(error) = transaction_result {
        rollback_moves(&codex_home, &operation.moved_files);
        operation.status = "failed".to_string();
        let _ = persist(&mut journal, &operation);
        return Err(error);
    }
    operation.status = "completed".to_string();
    persist(&mut journal, &operation)?;
    Ok(CleanupResult {
        operation_id: plan.operation_id,
        removed_threads: plan.thread_ids.len(),
        moved_files: operation.moved_file_count,
        reclaimed_bytes: plan.estimated_bytes,
        warnings: Vec::new(),
    })
}

fn conversation_columns(
    connection: &Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA {schema}.table_info({table})"))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get(1))
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn restore_from_backup(
    state: &mut Connection,
    operation: &JournalOperation,
    codex_home: &Path,
) -> Result<(), String> {
    let mut attached = Vec::new();
    for database in CONVERSATION_DATABASES {
        let backup_name = format!("backup-{}", database.file);
        let backup = operation
            .database_backups
            .iter()
            .map(Path::new)
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name == backup_name.as_str())
            });
        let Some(backup) = backup else {
            if database.schema == "main" {
                return Err("恢复日志缺少 state_5.sqlite 备份".into());
            }
            continue;
        };
        if !validate_regular_file_if_present(backup, "数据库备份")? {
            return Err(format!("恢复日志中的 {} 备份不存在", database.file));
        }
        let path = validated_database_path_if_present(
            codex_home,
            &codex_home.join(database.file),
            database.file,
        )?
        .ok_or_else(|| format!("恢复目标数据库 {} 不存在", database.file))?;
        if database.schema != "main" {
            attach_conversation_database(state, &path, database.schema)?;
        }
        let backup_schema = format!("backup_{}", database.schema);
        attach_conversation_database(state, backup, &backup_schema)?;
        attached.push((database, backup_schema));
    }
    let transaction = state
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("开始恢复事务失败: {error}"))?;
    for (database, backup_schema) in attached {
        let schema = database.schema;
        for (table, predicate) in database.tables {
            if !table_exists(&transaction, &backup_schema, table)? {
                continue;
            }
            if !table_exists(&transaction, schema, table)? {
                return Err(format!("恢复目标缺少 {}.{table}", database.file));
            }
            let live_columns = conversation_columns(&transaction, schema, table)?;
            let backup_columns = conversation_columns(&transaction, &backup_schema, table)?;
            let columns = live_columns
                .iter()
                .filter(|name| backup_columns.contains(name))
                .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ");
            if columns.is_empty() {
                return Err(format!("恢复目标 {}.{table} 缺少备份列", database.file));
            }
            let sql = format!("INSERT INTO {schema}.{table} ({columns}) SELECT {columns} FROM {backup_schema}.{table} WHERE {predicate} ON CONFLICT DO NOTHING");
            for id in &operation.plan.thread_ids {
                transaction
                    .execute(&sql, params![id])
                    .map_err(|error| format!("恢复 {}.{table} 失败: {error}", database.file))?;
            }
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("提交恢复事务失败: {error}"))
}

pub fn restore_cleanup_operation(operation_id: String) -> Result<CleanupResult, String> {
    let _mutation_guard = mutation_guard()?;
    ensure_writer_stopped()?;
    let codex_home = home_codex_path()?;
    ensure_conversation_schema_supported(&codex_home)?;
    validated_state_database_path(&codex_home)?;
    let mut journal = read_journal(&codex_home)?;
    require_unique_recovery_owner(&journal, &operation_id)?;
    if journal.operations.iter().any(|op| {
        op.operation_id == operation_id && matches!(op.status.as_str(), "purging" | "purged")
    }) {
        return Err("Recovery was permanently cleared or is being cleared".into());
    }
    let operation = journal
        .operations
        .iter()
        .find(|item| {
            item.operation_id == operation_id
                && !matches!(item.status.as_str(), "restored" | "purging" | "purged")
        })
        .cloned()
        .ok_or_else(|| "找不到可恢复的清理操作".to_string())?;
    validate_journal_operation(&operation, &codex_home)?;
    for (source, destination) in &operation.moved_files {
        let source_path = Path::new(source);
        let destination_path = Path::new(destination);
        let source_present = validate_regular_file_if_present(source_path, "恢复源文件")?;
        let destination_present =
            validate_regular_file_if_present(destination_path, "清理隔离文件")?;
        if source_present && destination_present {
            return Err(format!(
                "恢复目标已存在且隔离副本仍在，请先处理: {}",
                source_path.display()
            ));
        }
        if !source_present && !destination_present {
            return Err(format!(
                "隔离文件缺失，无法恢复: {}",
                destination_path.display()
            ));
        }
    }
    let state_path = validated_state_database_path(&codex_home)?;
    let mut state = Connection::open(&state_path)
        .map_err(|error| format!("打开 state_5.sqlite 失败: {error}"))?;
    set_cleanup_busy_timeout(&state)?;
    if !matches!(operation.status.as_str(), "prepared" | "backing_up")
        && (operation.status != "failed" || !operation.moved_files.is_empty())
    {
        state
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|error| error.to_string())?;
        restore_from_backup(&mut state, &operation, &codex_home)?;
    }
    let mut restored_files = 0;
    for (source, destination) in &operation.moved_files {
        let source_path = Path::new(source);
        let destination_path = Path::new(destination);
        if validate_regular_file_if_present(source_path, "恢复源文件")? {
            continue;
        }
        if !validate_regular_file_if_present(destination_path, "清理隔离文件")? {
            continue;
        }
        if let Some(parent) = source_path.parent() {
            ensure_directory_chain(&codex_home, parent, "对话恢复目录")?;
        }
        fs::rename(destination_path, source_path)
            .map_err(|error| format!("恢复对话记录文件失败: {error}"))?;
        restored_files += 1;
    }
    if let Some(item) = journal
        .operations
        .iter_mut()
        .find(|item| item.operation_id == operation_id)
    {
        item.status = "restored".to_string();
    }
    write_journal(&codex_home, &journal)?;
    Ok(CleanupResult {
        operation_id,
        removed_threads: operation.plan.thread_ids.len(),
        moved_files: restored_files,
        reclaimed_bytes: 0,
        warnings: Vec::new(),
    })
}

static MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn mutation_guard() -> Result<(std::sync::MutexGuard<'static, ()>, fs::File), String> {
    let guard = MUTATION_LOCK
        .try_lock()
        .map_err(|_| "Another recovery operation is in progress")?;
    let home = home_codex_path()?;
    let directory = ensure_recovery_dir(&home)?;
    let path = directory.join("mutation.lock");
    validate_regular_file_if_present(&path, "mutation lock")?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100).mode(0o600);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000).mode(0o600);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    file.try_lock()
        .map_err(|_| "Another application instance has a recovery operation in progress")?;
    Ok((guard, file))
}

pub fn purge_recovery(operation_id: String, confirmation: String) -> Result<u64, String> {
    purge_cleanup_operation(operation_id, confirmation)
}

fn purge_cleanup_operation(operation_id: String, confirmation: String) -> Result<u64, String> {
    let _guard = mutation_guard()?;
    ensure_writer_stopped()?;
    if !safe_operation_id(&operation_id) || confirmation != operation_id {
        return Err("Type the exact operation ID to permanently delete its recovery copies".into());
    }
    let home = home_codex_path()?;
    let mut journal = read_journal(&home)?;
    require_unique_recovery_owner(&journal, &operation_id)?;
    let index = journal
        .operations
        .iter()
        .position(|op| op.operation_id == operation_id)
        .ok_or("Conversation recovery operation not found")?;
    let operation = &journal.operations[index];
    validate_journal_operation(operation, &home)?;
    if !matches!(
        operation.status.as_str(),
        "completed" | "restored" | "purging"
    ) {
        return Err(
            "Only completed or restored conversation recovery can be permanently cleared".into(),
        );
    }
    let target = recovery_dir(&home).join(&operation_id);
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut bytes = 0;
    if target.exists() {
        for entry in WalkDir::new(&target).follow_links(false) {
            let entry = entry.map_err(|e| e.to_string())?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                return Err("Recovery contains symlink or special file; nothing purged".into());
            }
            if !entry
                .path()
                .canonicalize()
                .map_err(|e| e.to_string())?
                .starts_with(target.canonicalize().map_err(|e| e.to_string())?)
            {
                return Err("Recovery path escaped operation directory".into());
            }
            if metadata.is_dir() {
                dirs.push(entry.path().to_path_buf());
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    bytes += metadata.blocks() * 512;
                }
                #[cfg(not(unix))]
                {
                    bytes += metadata.len();
                }
                files.push(entry.path().to_path_buf());
            }
        }
    }
    // Durable irreversible intent first. A partial purge cannot later be restored as if complete.
    journal.operations[index].status = "purging".into();
    write_journal(&home, &journal)?;
    for file in files {
        validate_recovery_dir(&home)?;
        validate_directory_chain(
            &recovery_dir(&home),
            file.parent().ok_or("Invalid recovery path")?,
            "purge",
        )?;
        if validate_regular_file_if_present(&file, "purge")? {
            fs::remove_file(file).map_err(|e| format!("Partial purge; retry from history: {e}"))?;
        }
    }
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs {
        fs::remove_dir(dir).map_err(|e| format!("Partial purge; retry from history: {e}"))?;
    }
    if target.exists() {
        return Err("Recovery directory remains; retry".into());
    }
    journal.operations[index].status = "purged".into();
    write_journal(&home, &journal)?;
    Ok(bytes)
}

pub fn list_cleanup_operations() -> Result<Vec<CleanupOperationSummary>, String> {
    let codex_home = home_codex_path()?;
    let journal = read_journal(&codex_home)?;
    let mut summaries = Vec::with_capacity(journal.operations.len() + journal.compactions.len());
    for operation in journal.operations {
        validate_journal_operation(&operation, &codex_home)?;
        summaries.push(CleanupOperationSummary {
            selection_summary: operation.plan.selection_summary,
            operation_id: operation.operation_id,
            operation_type: "cleanup".to_string(),
            created_at: operation.plan.created_at,
            status: operation.status,
            thread_count: operation.plan.thread_ids.len(),
            database_count: 0,
            reclaimed_bytes: operation.plan.estimated_bytes,
        });
    }
    for operation in journal.compactions {
        validate_compaction_journal_operation(&operation, &codex_home)?;
        summaries.push(CleanupOperationSummary {
            selection_summary: None,
            operation_id: operation.operation_id,
            operation_type: "compaction".to_string(),
            created_at: operation.plan.created_at,
            status: operation.status,
            thread_count: 0,
            database_count: operation.plan.databases.len(),
            reclaimed_bytes: operation.plan.estimated_bytes,
        });
    }
    for operation in journal.managed_operations {
        validate_managed_journal_operation(&operation, &codex_home)?;
        summaries.push(CleanupOperationSummary {
            selection_summary: None,
            operation_id: operation.operation_id,
            operation_type: "managed".to_string(),
            created_at: operation.plan.created_at,
            status: operation.status,
            thread_count: operation.plan.entries.len(),
            database_count: 0,
            reclaimed_bytes: operation.plan.estimated_bytes,
        });
    }
    Ok(summaries)
}

pub fn create_compaction_plan() -> Result<CompactionPlan, String> {
    let codex_home = home_codex_path()?;
    let databases = database_paths(&codex_home)
        .iter()
        .map(|path| sqlite_estimate(path))
        .collect::<Result<Vec<_>, _>>()?;
    let fingerprints = databases
        .iter()
        .map(|item| file_fingerprint(Path::new(&item.path)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompactionPlan {
        operation_id: operation_id("compact"),
        codex_home: codex_home.display().to_string(),
        created_at: Utc::now().to_rfc3339(),
        estimated_bytes: databases.iter().map(|item| item.reclaimable_bytes).sum(),
        databases,
        fingerprints,
    })
}

pub fn execute_compaction_plan(plan: CompactionPlan) -> Result<CompactionPlan, String> {
    let _mutation_guard = mutation_guard()?;
    ensure_writer_stopped()?;
    let codex_home = home_codex_path()?;
    if !safe_operation_id(&plan.operation_id) {
        return Err("压缩计划标识无效".to_string());
    }
    if Path::new(&plan.codex_home) != codex_home {
        return Err("压缩计划对应的 Codex 目录已变化，请重新扫描".to_string());
    }
    let allowed_databases: std::collections::HashSet<_> = database_paths(&codex_home)
        .into_iter()
        .map(|path| path.display().to_string())
        .collect();
    if plan
        .databases
        .iter()
        .any(|database| !allowed_databases.contains(&database.path))
    {
        return Err("压缩计划包含非 Codex 数据库路径".to_string());
    }
    let database_set: std::collections::HashSet<_> = plan
        .databases
        .iter()
        .map(|database| database.path.as_str())
        .collect();
    let fingerprint_set: std::collections::HashSet<_> = plan
        .fingerprints
        .iter()
        .map(|fingerprint| fingerprint.path.as_str())
        .collect();
    if database_set.len() != plan.databases.len()
        || fingerprint_set.len() != plan.fingerprints.len()
        || database_set != fingerprint_set
    {
        return Err("压缩计划中的数据库与文件指纹集合不一致，请重新生成计划".to_string());
    }
    for fingerprint in &plan.fingerprints {
        if !allowed_databases.contains(&fingerprint.path) {
            return Err(format!(
                "拒绝处理 Codex 目录外的数据库: {}",
                fingerprint.path
            ));
        }
        if !fingerprint_matches(fingerprint)? {
            return Err(format!("数据库在计划创建后发生变化: {}", fingerprint.path));
        }
    }
    require_unused_operation_id(&codex_home, &plan.operation_id)?;
    let quarantine = ensure_operation_dir(&codex_home, &plan.operation_id)?;
    let mut journal = read_journal(&codex_home)?;
    if journal
        .compactions
        .iter()
        .any(|operation| operation.operation_id == plan.operation_id)
    {
        return Err("压缩操作标识已存在，请重新生成计划".to_string());
    }
    let mut operation = JournalCompaction {
        operation_id: plan.operation_id.clone(),
        codex_home: codex_home.display().to_string(),
        quarantine_dir: quarantine.display().to_string(),
        plan: plan.clone(),
        databases: Vec::new(),
        status: "prepared".to_string(),
    };
    replace_compaction_journal_entry(&mut journal, &operation);
    write_journal(&codex_home, &journal)?;

    let persist = |journal: &mut Journal, operation: &JournalCompaction| {
        replace_compaction_journal_entry(journal, operation);
        write_journal(&codex_home, journal)
    };
    let mut replaced = Vec::new();
    let compaction_result = (|| -> Result<(), String> {
        for database in &plan.databases {
            let path = PathBuf::from(&database.path);
            let (temp, backup) = compaction_artifact_paths(&quarantine, &codex_home, &path)?;
            let temp_present = validate_regular_file_if_present(&temp, "数据库压缩临时文件")?;
            let backup_present = validate_regular_file_if_present(&backup, "数据库原始备份")?;
            if temp_present || backup_present {
                return Err(format!(
                    "压缩恢复目录已存在，请重新生成计划: {}",
                    path.display()
                ));
            }
            let database_index = operation.databases.len();
            operation.databases.push(JournalCompactionDatabase {
                path: path.display().to_string(),
                temp_path: temp.display().to_string(),
                backup_path: backup.display().to_string(),
                sidecars: Vec::new(),
                stage: "prepared".to_string(),
            });
            persist(&mut journal, &operation)?;
            operation.status = "running".to_string();
            operation.databases[database_index].stage = "vacuuming".to_string();
            persist(&mut journal, &operation)?;
            checkpoint_wal(&path)?;
            let connection =
                Connection::open(&path).map_err(|error| format!("打开数据库失败: {error}"))?;
            connection
                .execute("VACUUM INTO ?1", params![temp.display().to_string()])
                .map_err(|error| format!("压缩 {} 失败: {error}", path.display()))?;
            operation.databases[database_index].stage = "vacuumed".to_string();
            persist(&mut journal, &operation)?;
            let integrity = Connection::open(&temp)
                .map_err(|error| format!("打开压缩结果失败: {error}"))?
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .map_err(|error| format!("校验压缩结果失败: {error}"))?;
            if integrity != "ok" {
                return Err(format!("压缩结果校验失败: {integrity}"));
            }
            operation.databases[database_index].stage = "verified".to_string();
            persist(&mut journal, &operation)?;
            fs::rename(&path, &backup).map_err(|error| format!("保存原数据库失败: {error}"))?;
            let mut sidecars = Vec::new();
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
                if sidecar.exists() {
                    let sidecar_backup = PathBuf::from(format!("{}{}", backup.display(), suffix));
                    sidecars.push((sidecar, sidecar_backup));
                }
            }
            operation.databases[database_index].sidecars = sidecars
                .iter()
                .map(|(original, saved)| {
                    (original.display().to_string(), saved.display().to_string())
                })
                .collect();
            operation.databases[database_index].stage = "sidecars_backing_up".to_string();
            if let Err(error) = persist(&mut journal, &operation) {
                rollback_compaction_stage(&path, &backup, &sidecars, &temp);
                return Err(error);
            }
            for (sidecar, sidecar_backup) in &sidecars {
                if let Err(error) = fs::rename(sidecar, sidecar_backup) {
                    rollback_compaction_stage(&path, &backup, &sidecars, &temp);
                    return Err(format!("保存数据库附属文件失败: {error}"));
                }
            }
            operation.databases[database_index].stage = "sidecars_backed_up".to_string();
            if let Err(error) = persist(&mut journal, &operation) {
                rollback_compaction_stage(&path, &backup, &sidecars, &temp);
                return Err(error);
            }
            if let Err(error) = fs::rename(&temp, &path) {
                rollback_compaction_stage(&path, &backup, &sidecars, &temp);
                return Err(format!("替换压缩数据库失败: {error}"));
            }
            replaced.push(CompactionReplacement {
                path,
                backup,
                sidecars,
            });
            operation.databases[database_index].stage = "replaced".to_string();
            persist(&mut journal, &operation)?;
        }
        operation.status = "completed".to_string();
        persist(&mut journal, &operation)?;
        Ok(())
    })();
    if let Err(error) = compaction_result {
        for replacement in replaced.iter().rev() {
            let failed = replacement.backup.with_extension("failed");
            if replacement.path.exists() {
                let _ = fs::rename(&replacement.path, failed);
            }
            let _ = fs::rename(&replacement.backup, &replacement.path);
            for (original, saved) in replacement.sidecars.iter().rev() {
                let _ = fs::rename(saved, original);
            }
        }
        operation.status = "failed".to_string();
        let _ = persist(&mut journal, &operation);
        return Err(error);
    }
    create_compaction_plan()
}

pub fn restore_compaction_operation(operation_id: String) -> Result<CleanupResult, String> {
    let _mutation_guard = mutation_guard()?;
    ensure_writer_stopped()?;
    let codex_home = home_codex_path()?;
    let mut journal = read_journal(&codex_home)?;
    require_unique_recovery_owner(&journal, &operation_id)?;
    if journal.operations.iter().any(|op| {
        op.operation_id == operation_id && matches!(op.status.as_str(), "purging" | "purged")
    }) {
        return Err("Recovery was permanently cleared or is being cleared".into());
    }
    let mut operation = journal
        .compactions
        .iter()
        .find(|item| item.operation_id == operation_id)
        .cloned()
        .ok_or_else(|| "找不到可恢复的数据库压缩操作".to_string())?;
    if operation.status == "restored" {
        return Err("数据库压缩操作已经恢复".to_string());
    }
    validate_compaction_journal_operation(&operation, &codex_home)?;
    operation.status = "restoring".to_string();
    replace_compaction_journal_entry(&mut journal, &operation);
    write_journal(&codex_home, &journal)?;

    let mut restored_databases = 0;
    let mut warnings = Vec::new();
    for index in 0..operation.databases.len() {
        let entry = &mut operation.databases[index];
        if entry.stage == "restored" {
            continue;
        }
        let path = PathBuf::from(&entry.path);
        let temp = PathBuf::from(&entry.temp_path);
        let backup = PathBuf::from(&entry.backup_path);
        let temp_present = validate_regular_file_if_present(&temp, "数据库压缩临时文件")?;
        let path_present =
            validated_database_path_if_present(&codex_home, &path, "数据库")?.is_some();
        let backup_present = validate_regular_file_if_present(&backup, "数据库原始备份")?;
        if temp_present {
            fs::remove_file(&temp)
                .map_err(|error| format!("清理未完成的压缩临时文件失败: {error}"))?;
        }
        if path_present && backup_present {
            let displaced = backup.with_extension("compacted");
            if validate_regular_file_if_present(&displaced, "压缩数据库副本")? {
                return Err(format!(
                    "恢复目标已有隔离副本，请先处理: {}",
                    displaced.display()
                ));
            }
            fs::rename(&path, &displaced)
                .map_err(|error| format!("隔离压缩数据库失败: {error}"))?;
            warnings.push(format!("压缩结果已保留在 {}", displaced.display()));
        }
        if backup_present {
            fs::rename(&backup, &path).map_err(|error| format!("恢复原数据库失败: {error}"))?;
            restored_databases += 1;
        } else if !path_present {
            return Err(format!("原数据库和压缩备份均不存在: {}", path.display()));
        }
        for (original, saved) in &entry.sidecars {
            let original_path = Path::new(original);
            let saved_path = Path::new(saved);
            let original_present =
                validate_regular_file_if_present(original_path, "数据库附属文件")?;
            let saved_present = validate_regular_file_if_present(saved_path, "数据库附属文件备份")?;
            if original_present && saved_present {
                return Err(format!(
                    "sidecar 恢复目标已存在且隔离副本仍在: {}",
                    original_path.display()
                ));
            }
            if !original_present && saved_present {
                fs::rename(saved_path, original_path)
                    .map_err(|error| format!("恢复数据库附属文件失败: {error}"))?;
            }
        }
        entry.stage = "restored".to_string();
        replace_compaction_journal_entry(&mut journal, &operation);
        write_journal(&codex_home, &journal)?;
    }
    operation.status = "restored".to_string();
    replace_compaction_journal_entry(&mut journal, &operation);
    write_journal(&codex_home, &journal)?;
    Ok(CleanupResult {
        operation_id,
        removed_threads: 0,
        moved_files: restored_databases,
        reclaimed_bytes: 0,
        warnings,
    })
}

// Live cleanup deliberately has a separate allowlist from offline managed cleanup.
// No conversation, database, skill, plugin, shell snapshot or runtime path is eligible.
const LIVE_MIN_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LIVE_MAX_ENTRIES: usize = 20_000;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveCleanupItem {
    pub id: String,
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    pub file_count: u64,
    pub can_remove: bool,
    pub blocking_reason: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveCleanupPreview {
    pub scanned_at: String,
    pub codex_home: String,
    pub codex_running: bool,
    pub minimum_age_days: u32,
    pub eligible_bytes: u64,
    pub items: Vec<LiveCleanupItem>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveCleanupPlan {
    pub plan_id: String,
    pub codex_home: String,
    pub created_at: String,
    pub entries: Vec<LiveCleanupItem>,
    pub estimated_bytes: u64,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveCleanupResult {
    pub operation_id: String,
    pub moved_items: usize,
    pub moved_bytes: u64,
    pub freed_bytes: u64,
    pub warnings: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LiveCleanupOperationSummary {
    pub operation_id: String,
    pub created_at: String,
    pub status: String,
    pub item_count: usize,
    pub moved_bytes: u64,
}
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct LiveStamp {
    relative: String,
    bytes: u64,
    modified_ns: u128,
    inode: u64,
    device: u64,
    is_dir: bool,
}
#[derive(Debug, Clone)]
struct LiveServerPlan {
    plan: LiveCleanupPlan,
    stamps: BTreeMap<String, Vec<LiveStamp>>,
    created: std::time::Instant,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
struct LiveOperation {
    plan: LiveCleanupPlan,
    stamps: BTreeMap<String, Vec<LiveStamp>>,
    status: String,
    moved: Vec<String>,
}
static LIVE_PLANS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, LiveServerPlan>>> =
    std::sync::OnceLock::new();
static LIVE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn live_plans() -> &'static std::sync::Mutex<BTreeMap<String, LiveServerPlan>> {
    LIVE_PLANS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}
fn live_id_allowed(id: &str) -> bool {
    let parts: Vec<_> = id.split('/').collect();
    match parts.as_slice() {
        [".tmp", name] => name.strip_prefix("git-").is_some_and(|suffix| {
            suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        }),
        ["log", name] => name.strip_prefix("codex-tui.log.").is_some_and(|suffix| {
            let digits = suffix.strip_suffix(".gz").unwrap_or(suffix);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        }),
        _ => false,
    }
}
fn live_home() -> Result<PathBuf, String> {
    let home = home_codex_path()?;
    let canonical = home.canonicalize().map_err(|e| e.to_string())?;
    // Trusted macOS /tmp -> /private/tmp ancestor alias is resolved by canonicalizing
    // the configured root, but links inside the configured root are never followed.
    let metadata = fs::symlink_metadata(&home).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Codex home must be a real directory".into());
    }
    Ok(canonical)
}
fn live_source(home: &Path, id: &str) -> Result<PathBuf, String> {
    if !live_id_allowed(id) {
        return Err("Live cleanup path is not allowlisted".into());
    }
    let path = home.join(id);
    let parent = path.parent().ok_or("Missing live cleanup parent")?;
    let meta = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink()
        || !meta.is_dir()
        || parent.canonicalize().map_err(|e| e.to_string())? != parent
    {
        return Err("Live cleanup parent contains a symlink".into());
    }
    Ok(path)
}
fn live_stamps(
    path: &Path,
    id: &str,
    require_old: bool,
) -> Result<(Vec<LiveStamp>, u64, u64), String> {
    live_measure(path, id, require_old, true)
}
fn live_measure(
    path: &Path,
    id: &str,
    require_old: bool,
    require_head: bool,
) -> Result<(Vec<LiveStamp>, u64, u64), String> {
    let start = std::time::Instant::now();
    let mut stamps = Vec::new();
    let mut bytes = 0_u64;
    let mut count = 0_u64;
    let temporary_git = id.starts_with(".tmp/");
    let root_meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let root_device = {
        use std::os::unix::fs::MetadataExt;
        root_meta.dev()
    };
    if temporary_git != root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return Err("Expected regular rotated log or temporary bare Git directory".into());
    }
    // The installed Codex creates these throwaway bare object-store directories.
    // Any additional root content (config, hooks, worktree, source files) blocks it.
    if temporary_git {
        let mut has_head = false;
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            if !matches!(name.to_str(), Some("HEAD" | "objects" | "refs")) {
                return Err("Temporary directory has unrecognized content".into());
            }
            has_head |= name == "HEAD";
        }
        if require_head && !has_head {
            return Err("Incomplete temporary Git directory; retained".into());
        }
    }
    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = entry.map_err(|e| e.to_string())?;
        if stamps.len() >= LIVE_MAX_ENTRIES || start.elapsed() > Duration::from_millis(250) {
            return Err("Candidate exceeds bounded inspection; retained".into());
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err("Symlink or special file; retained".into());
        }
        let modified = metadata.modified().map_err(|e| e.to_string())?;
        if require_old
            && SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                < LIVE_MIN_AGE
        {
            return Err("Changed within the last 7 days; retained".into());
        }
        #[cfg(unix)]
        let (inode, device, links) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.ino(), metadata.dev(), metadata.nlink())
        };
        #[cfg(not(unix))]
        let (inode, device, links) = (0, 0, 1);
        #[cfg(unix)]
        if device != root_device {
            return Err("Nested mount; retained".into());
        }
        if metadata.is_file() && links != 1 {
            return Err("Hard-linked file; retained".into());
        }
        if metadata.is_file() {
            bytes = bytes.saturating_add(allocated_bytes(&metadata));
            count += 1;
        }
        stamps.push(LiveStamp {
            relative: entry
                .path()
                .strip_prefix(path)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("Non-UTF8 cleanup path; retained")?
                .to_string(),
            bytes: metadata.len(),
            modified_ns: modified
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos(),
            inode,
            device,
            is_dir: metadata.is_dir(),
        });
    }
    stamps.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok((stamps, bytes, count))
}

// Drain the process-wide stream without storing unrelated private paths.
// Retain only candidates/recovery; all byte/time/incompleteness limits fail closed.
fn read_live_open_paths(
    mut reader: impl std::io::Read,
    home: &Path,
    byte_limit: usize,
) -> Result<Vec<PathBuf>, String> {
    let roots = [home.join(".tmp"), home.join("log"), recovery_dir(home)];
    let mut buffer = [0_u8; 64 * 1024];
    let mut field = Vec::new();
    let mut paths = HashSet::new();
    let mut total_bytes = 0_usize;
    let mut retained_bytes = 0_usize;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|e| format!("Open-file output read failed: {e}; retained"))?;
        if count == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(count);
        if total_bytes > byte_limit {
            return Err(format!(
                "Open-file inspection exceeded {byte_limit} output bytes; retained"
            ));
        }
        for byte in &buffer[..count] {
            if *byte == 0 || *byte == b'\n' {
                if let Some(name) = field.strip_prefix(b"n") {
                    if let Ok(name) = std::str::from_utf8(name) {
                        let path = PathBuf::from(name);
                        if roots.iter().any(|root| path.starts_with(root)) && paths.insert(path) {
                            retained_bytes = retained_bytes.saturating_add(name.len());
                            if retained_bytes > 8 * 1024 * 1024 || paths.len() > 100_000 {
                                return Err(
                                    "Open-file matching-path limit exceeded; retained".into()
                                );
                            }
                        }
                    }
                }
                field.clear();
            } else {
                if field.len() >= 64 * 1024 {
                    return Err(
                        "Open-file inspection returned an oversized record; retained".into(),
                    );
                }
                field.push(*byte);
            }
        }
    }
    if !field.is_empty() {
        return Err("Open-file output ended with an incomplete record; retained".into());
    }
    Ok(paths.into_iter().collect())
}

fn live_open_paths() -> Result<Vec<PathBuf>, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let home = live_home()?;
    let mut child = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-F0n"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Open-file inspection unavailable: {e}"))?;
    let stdout = child.stdout.take().ok_or("Missing lsof output")?;
    let stderr = child.stderr.take().ok_or("Missing lsof errors")?;
    let output = std::thread::spawn(move || read_live_open_paths(stdout, &home, 64 * 1024 * 1024));
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.take(64 * 1024).read_to_end(&mut bytes);
        (result, bytes)
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let paths = output
        .join()
        .map_err(|_| "Open-file inspection reader failed; retained")??;
    let (error_read, errors) = errors
        .join()
        .map_err(|_| "Open-file inspection error reader failed; retained")?;
    if status.is_none() {
        return Err("Open-file inspection timed out after 5 seconds; retained".into());
    }
    if !status.as_ref().is_some_and(|s| s.success()) {
        return Err(format!(
            "Open-file inspection exited unsuccessfully (code {:?}); retained",
            status.and_then(|s| s.code())
        ));
    }
    if error_read.is_err() {
        return Err("Open-file inspection error stream could not be read; retained".into());
    }
    if !errors.is_empty() {
        return Err("Open-file inspection reported warnings or permission errors; retained".into());
    }
    Ok(paths)
}

fn live_is_open(path: &Path, open: &[PathBuf]) -> bool {
    open.iter().any(|p| p == path || p.starts_with(path))
}
type LiveInventory = (Vec<LiveCleanupItem>, BTreeMap<String, Vec<LiveStamp>>);

fn live_scan(home: &Path, open: &Result<Vec<PathBuf>, String>) -> Result<LiveInventory, String> {
    let inspection_started = std::time::Instant::now();
    let mut items = Vec::new();
    let mut all_stamps = BTreeMap::new();
    for root in ["log", ".tmp"] {
        let parent = home.join(root);
        if !parent.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(&parent).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Live cleanup root is not a real directory".into());
        }
        for entry in fs::read_dir(parent).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let id = format!("{root}/{}", entry.file_name().to_string_lossy());
            if !live_id_allowed(&id) {
                continue;
            }
            let path = live_source(home, &id)?;
            let measured = if inspection_started.elapsed() < Duration::from_secs(8) {
                live_stamps(&path, &id, true)
            } else {
                Err("Live preview inspection budget reached; retained".into())
            };
            let (bytes, file_count, mut blocking_reason) = match measured {
                Ok((stamps, bytes, count)) => {
                    all_stamps.insert(id.clone(), stamps);
                    (bytes, count, None)
                }
                Err(reason) => (0, 0, Some(reason)),
            };
            if blocking_reason.is_none() {
                blocking_reason = match open {
                    Ok(open) if live_is_open(&path, open) => {
                        Some("An active process has this item open; retained".into())
                    }
                    Err(reason) => Some(reason.clone()),
                    _ => None,
                };
            }
            items.push(LiveCleanupItem {
                id,
                path: path.to_string_lossy().into_owned(),
                kind: if root == "log" {
                    "rotated-log"
                } else {
                    "stale-git-temporary"
                }
                .into(),
                bytes,
                file_count,
                can_remove: blocking_reason.is_none(),
                blocking_reason,
            });
        }
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((items, all_stamps))
}
pub fn scan_live_cleanup() -> Result<LiveCleanupPreview, String> {
    let home = live_home()?;
    let open = live_open_paths();
    let (items, _) = live_scan(&home, &open)?;
    Ok(LiveCleanupPreview { scanned_at: Utc::now().to_rfc3339(), codex_home: home.display().to_string(), codex_running: writer_is_running(), minimum_age_days: 7, eligible_bytes: items.iter().filter(|i| i.can_remove).map(|i| i.bytes).sum(), items, warnings: vec!["Only old rotated logs and verified temporary bare Git stores are eligible. Moving to recovery frees 0 bytes until recovery copies are explicitly purged.".into()] })
}
pub fn create_live_cleanup_plan(item_ids: Vec<String>) -> Result<LiveCleanupPlan, String> {
    let home = live_home()?;
    let requested: HashSet<_> = item_ids.iter().collect();
    if requested.is_empty() || requested.len() != item_ids.len() {
        return Err("Select unique live cleanup items".into());
    }
    // Rebuild only the requested allowlisted candidates. Unrelated temporary
    // stores and the preview's global inspection budget cannot affect selection.
    let sources = item_ids
        .iter()
        .map(|id| live_source(&home, id).map(|path| (id.clone(), path)))
        .collect::<Result<Vec<_>, _>>()?;
    let open =
        live_open_paths().map_err(|reason| format!("Cannot create live cleanup plan: {reason}"))?;
    let mut entries = Vec::with_capacity(sources.len());
    let mut selected_stamps = BTreeMap::new();
    for (id, path) in sources {
        let (stamps, bytes, file_count) = live_stamps(&path, &id, true)
            .map_err(|reason| format!("Cannot clean {id}: {reason}"))?;
        if live_is_open(&path, &open) {
            return Err(format!(
                "Cannot clean {id}: an active process has this item open; retained"
            ));
        }
        selected_stamps.insert(id.clone(), stamps);
        entries.push(LiveCleanupItem {
            kind: if id.starts_with("log/") {
                "rotated-log"
            } else {
                "stale-git-temporary"
            }
            .into(),
            id,
            path: path.display().to_string(),
            bytes,
            file_count,
            can_remove: true,
            blocking_reason: None,
        });
    }
    let sequence = LIVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let plan = LiveCleanupPlan {
        plan_id: format!("{}-{sequence}", operation_id("live")),
        codex_home: home.display().to_string(),
        created_at: Utc::now().to_rfc3339(),
        estimated_bytes: entries.iter().map(|i| i.bytes).sum(),
        entries,
    };
    let mut plans = live_plans().lock().map_err(|_| "Plan cache unavailable")?;
    plans.retain(|_, p| p.created.elapsed() < Duration::from_secs(600));
    if plans.len() >= 16 {
        return Err("Too many pending previews; wait or restart the app".into());
    }
    plans.insert(
        plan.plan_id.clone(),
        LiveServerPlan {
            plan: plan.clone(),
            stamps: selected_stamps,
            created: std::time::Instant::now(),
        },
    );
    Ok(plan)
}
fn live_operation_path(home: &Path, id: &str) -> Result<PathBuf, String> {
    if !safe_operation_id(id) || !id.starts_with("live-") {
        return Err("Invalid live operation ID".into());
    }
    let dir = validate_recovery_dir(home)?.join(id);
    validate_real_directory_if_present(&dir, "live recovery")?;
    let path = dir.join("live-operation.json");
    validate_regular_file_if_present(&path, "live recovery journal")?;
    Ok(path)
}
fn live_write(home: &Path, op: &LiveOperation) -> Result<(), String> {
    let path = live_operation_path(home, &op.plan.plan_id)?;
    let sequence = LIVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("json.tmp-{}-{sequence}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(op).map_err(|e| e.to_string())?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
    fs::File::open(path.parent().ok_or("Missing journal parent")?)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}
fn live_read(home: &Path, id: &str) -> Result<LiveOperation, String> {
    let path = live_operation_path(home, id)?;
    let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
    if metadata.len() > 16 * 1024 * 1024 {
        return Err("Live recovery journal too large".into());
    }
    let op: LiveOperation = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if op.plan.plan_id != id
        || Path::new(&op.plan.codex_home) != home
        || op.plan.entries.is_empty()
        || !matches!(
            op.status.as_str(),
            "prepared"
                | "moving"
                | "completed"
                | "failed"
                | "restoring"
                | "restored"
                | "purging"
                | "purged"
        )
    {
        return Err("Invalid live recovery journal".into());
    }
    let mut seen = HashSet::new();
    for item in &op.plan.entries {
        if !seen.insert(&item.id)
            || !live_id_allowed(&item.id)
            || Path::new(&item.path) != home.join(&item.id)
            || !op.stamps.contains_key(&item.id)
        {
            return Err("Invalid live recovery entry".into());
        }
    }
    if op.moved.iter().any(|id| !seen.contains(id)) {
        return Err("Invalid live recovery progress".into());
    }
    Ok(op)
}
pub fn execute_live_cleanup_plan(plan: LiveCleanupPlan) -> Result<LiveCleanupResult, String> {
    let _guard = mutation_guard()?;
    let home = live_home()?;
    let server = live_plans()
        .lock()
        .map_err(|_| "Plan cache unavailable")?
        .remove(&plan.plan_id)
        .ok_or("Preview expired; create a new plan")?;
    if server.created.elapsed() > Duration::from_secs(600)
        || server.plan.codex_home != home.display().to_string()
        || serde_json::to_value(&server.plan).map_err(|e| e.to_string())?
            != serde_json::to_value(&plan).map_err(|e| e.to_string())?
    {
        return Err("Live plan was altered or expired; create a new plan".into());
    }
    require_unused_operation_id(&home, &plan.plan_id)?;
    let quarantine = ensure_operation_dir(&home, &plan.plan_id)?;
    let mut op = LiveOperation {
        plan: server.plan,
        stamps: server.stamps,
        status: "prepared".into(),
        moved: Vec::new(),
    };
    live_write(&home, &op)?;
    let result = (|| -> Result<(), String> {
        op.status = "moving".into();
        live_write(&home, &op)?;
        // One complete open-file snapshot immediately before the move batch;
        // age and complete subtree fingerprints are revalidated per item.
        let open = live_open_paths()?;
        for item in op.plan.entries.clone() {
            let source = live_source(&home, &item.id)?;
            let destination = quarantine.join("live").join(&item.id);
            ensure_directory_chain(
                &quarantine,
                destination.parent().ok_or("Missing destination parent")?,
                "live recovery item",
            )?;
            if fs::symlink_metadata(&destination).is_ok() {
                return Err("Recovery target already exists".into());
            }
            let (stamps, _, _) = live_stamps(&source, &item.id, true)?;
            if stamps != op.stamps[&item.id] || live_is_open(&source, &open) {
                return Err("Live candidate changed or is open; no further files moved".into());
            }
            fs::rename(&source, &destination).map_err(|e| e.to_string())?;
            op.moved.push(item.id);
            live_write(&home, &op)?;
        }
        op.status = "completed".into();
        live_write(&home, &op)
    })();
    if let Err(error) = result {
        op.status = "failed".into();
        let _ = live_write(&home, &op);
        return Err(format!(
            "{error}. Any moved items remain recoverable in operation {}",
            op.plan.plan_id
        ));
    }
    Ok(LiveCleanupResult { operation_id: op.plan.plan_id, moved_items: op.moved.len(), moved_bytes: plan.estimated_bytes, freed_bytes: 0, warnings: vec!["Moved to recovery; physical disk space has not been freed. Purging recovery requires separate explicit authorization.".into()] })
}
pub fn restore_live_cleanup_operation(operation_id: String) -> Result<LiveCleanupResult, String> {
    let _guard = mutation_guard()?;
    let home = live_home()?;
    let mut op = live_read(&home, &operation_id)?;
    if matches!(op.status.as_str(), "restored" | "purging" | "purged") {
        return Err(
            "Live cleanup is already restored or its recovery was permanently purged".into(),
        );
    }
    let quarantine = recovery_dir(&home).join(&operation_id);
    let open = live_open_paths()?;
    // A failed batch may leave later source-only items changed or open.
    // Restore only actual recovery copies, including rename-before-journal crashes.
    for item in &op.plan.entries {
        let source = live_source(&home, &item.id)?;
        let saved = quarantine.join("live").join(&item.id);
        validate_directory_chain(
            &quarantine,
            saved.parent().ok_or("Missing recovery parent")?,
            "live recovery",
        )?;
        let source_present = fs::symlink_metadata(&source).is_ok();
        let saved_present = fs::symlink_metadata(&saved).is_ok();
        if !saved_present {
            if !source_present && op.moved.contains(&item.id) {
                return Err("Live recovery item is missing".into());
            }
            continue;
        }
        if source_present {
            return Err("Restore destination already exists; no files overwritten".into());
        }
        let (stamps, _, _) = live_stamps(&saved, &item.id, false)?;
        if stamps != op.stamps[&item.id] || live_is_open(&saved, &open) {
            return Err("Live recovery item changed or is open".into());
        }
    }
    op.status = "restoring".into();
    live_write(&home, &op)?;
    let mut restored = 0;
    for item in &op.plan.entries {
        let source = live_source(&home, &item.id)?;
        let saved = quarantine.join("live").join(&item.id);
        if fs::symlink_metadata(&saved).is_ok() {
            if fs::symlink_metadata(&source).is_ok() {
                return Err("Restore target appeared; no files overwritten".into());
            }
            validate_directory_chain(
                &quarantine,
                saved.parent().ok_or("Missing recovery parent")?,
                "live restore",
            )?;
            if live_stamps(&saved, &item.id, false)?.0 != op.stamps[&item.id] {
                return Err("Recovery item changed".into());
            }
            fs::rename(saved, source).map_err(|e| e.to_string())?;
            restored += 1;
        }
    }
    op.status = "restored".into();
    live_write(&home, &op)?;
    Ok(LiveCleanupResult {
        operation_id,
        moved_items: restored,
        moved_bytes: 0,
        freed_bytes: 0,
        warnings: Vec::new(),
    })
}
/// Permanent deletion is a separate explicit action; never part of quarantine.
/// Allocated bytes removed are reported in warnings, not as physical free-space gain.
pub fn purge_live_cleanup_operation(
    operation_id: String,
    confirmation: String,
) -> Result<LiveCleanupResult, String> {
    let _guard = mutation_guard()?;
    if confirmation != operation_id {
        return Err("Type the exact operation ID to permanently delete recovery copies".into());
    }
    let home = live_home()?;
    let mut op = live_read(&home, &operation_id)?;
    if !matches!(op.status.as_str(), "completed" | "failed" | "purging") {
        return Err("Only completed or interrupted cleanup recovery can be purged".into());
    }
    let quarantine = recovery_dir(&home).join(&operation_id);
    let open = live_open_paths()?;
    let mut targets = Vec::new();
    // Infer completed renames from filesystem state, including interrupted journals.
    for item in &op.plan.entries {
        let saved = quarantine.join("live").join(&item.id);
        validate_directory_chain(
            &quarantine,
            saved.parent().ok_or("Missing recovery parent")?,
            "live purge",
        )?;
        match fs::symlink_metadata(&saved) {
            Ok(_) => {
                let resuming = op.status == "purging";
                let (stamps, bytes, _) = live_measure(&saved, &item.id, false, !resuming)?;
                let matches = if resuming {
                    stamps.iter().all(|actual| {
                        op.stamps[&item.id].iter().any(|expected| {
                            if actual.is_dir {
                                actual.relative == expected.relative
                                    && expected.is_dir
                                    && actual.inode == expected.inode
                                    && actual.device == expected.device
                            } else {
                                actual == expected
                            }
                        })
                    })
                } else {
                    stamps == op.stamps[&item.id]
                };
                if !matches || live_is_open(&saved, &open) {
                    return Err("Recovery content changed or is open; purge refused".into());
                }
                targets.push((saved, stamps, bytes));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    op.status = "purging".into();
    live_write(&home, &op)?;
    let mut deleted_allocated = 0_u64;
    for (root, stamps, bytes) in targets {
        // Remove only the exact validated entries. New/symlink entries cannot be
        // followed and unknown descendants make remove_dir fail closed.
        for expected in stamps.iter().rev() {
            let path = if expected.relative.is_empty() {
                root.clone()
            } else {
                root.join(&expected.relative)
            };
            validate_directory_chain(
                &quarantine,
                path.parent().ok_or("Missing purge parent")?,
                "live purge",
            )?;
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() || metadata.is_dir() != expected.is_dir {
                return Err("Recovery item changed during purge".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.ino() != expected.inode || metadata.dev() != expected.device {
                    return Err("Recovery identity changed during purge".into());
                }
            }
            if expected.is_dir {
                fs::remove_dir(&path).map_err(|e| e.to_string())?;
            } else {
                if metadata.len() != expected.bytes
                    || metadata
                        .modified()
                        .map_err(|e| e.to_string())?
                        .duration_since(UNIX_EPOCH)
                        .map_err(|e| e.to_string())?
                        .as_nanos()
                        != expected.modified_ns
                {
                    return Err("Recovery file changed during purge".into());
                }
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
        }
        deleted_allocated = deleted_allocated.saturating_add(bytes);
    }
    op.status = "purged".into();
    live_write(&home, &op)?;
    Ok(LiveCleanupResult { operation_id, moved_items: 0, moved_bytes: 0, freed_bytes: 0, warnings: vec![format!("Permanently deleted {deleted_allocated} allocated bytes from recovery. Physical free-space change is not measured; APFS snapshots or clones may retain blocks.")] })
}

pub fn list_live_cleanup_operations() -> Result<Vec<LiveCleanupOperationSummary>, String> {
    match fs::symlink_metadata(home_codex_path()?) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    let home = live_home()?;
    let root = validate_recovery_dir(&home)?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !id.starts_with("live-") {
            continue;
        }
        let op = live_read(&home, &id)?;
        result.push(LiveCleanupOperationSummary {
            operation_id: id,
            created_at: op.plan.created_at,
            status: op.status,
            item_count: op.plan.entries.len(),
            moved_bytes: op
                .plan
                .entries
                .iter()
                .filter(|i| op.moved.contains(&i.id))
                .map(|i| i.bytes)
                .sum(),
        });
    }
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn live_fixture(root: &Path, name: &str) -> PathBuf {
        let item = root.join(".tmp").join(name);
        fs::create_dir_all(item.join("objects")).unwrap();
        fs::create_dir_all(item.join("refs")).unwrap();
        fs::write(item.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let old = SystemTime::now() - LIVE_MIN_AGE - Duration::from_secs(60);
        let paths: Vec<_> = WalkDir::new(&item)
            .into_iter()
            .map(|e| e.unwrap().into_path())
            .collect();
        for path in paths.into_iter().rev() {
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        item
    }
    fn fixture_live_plan(home: &Path, id: &str) -> LiveCleanupPlan {
        let home = home.canonicalize().unwrap();
        let (items, stamps) = live_scan(&home, &Ok(Vec::new())).unwrap();
        let item = items.into_iter().find(|i| i.id == id).unwrap();
        assert!(item.can_remove, "{:?}", item.blocking_reason);
        let plan = LiveCleanupPlan {
            plan_id: format!(
                "{}-{}",
                operation_id("live"),
                LIVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            codex_home: home.display().to_string(),
            created_at: Utc::now().to_rfc3339(),
            estimated_bytes: item.bytes,
            entries: vec![item],
        };
        live_plans().lock().unwrap().insert(
            plan.plan_id.clone(),
            LiveServerPlan {
                plan: plan.clone(),
                stamps,
                created: std::time::Instant::now(),
            },
        );
        plan
    }
    #[test]
    fn live_open_inspection_streams_large_inventory_and_keeps_only_eligible_roots() {
        use std::io::Read;
        let home = Path::new("/Users/fixture/.codex");
        let records = b"n/Users/fixture/.codex/.tmp/git-ABC123/HEAD\0n/Users/fixture/.codex/log/codex-tui.log.1\0n/Users/fixture/.codex/recovery/codex-storage-manager/live-fixture/live/.tmp/git-ABC123/HEAD\0n/Users/fixture/.codex/sessions/valuable.jsonl\0n/Users/unrelated/private-file\0";
        let large_stream = std::io::repeat(b'\n')
            .take(9 * 1024 * 1024)
            .chain(std::io::Cursor::new(records));
        let paths = read_live_open_paths(large_stream, home, 64 * 1024 * 1024).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths
            .iter()
            .all(|p| !p.to_string_lossy().contains("valuable")
                && !p.to_string_lossy().contains("private-file")));
        assert!(
            read_live_open_paths(std::io::repeat(b'\n').take(2048), home, 1024)
                .unwrap_err()
                .contains("exceeded")
        );
        assert!(
            read_live_open_paths(std::io::Cursor::new(b"n/incomplete"), home, 1024)
                .unwrap_err()
                .contains("incomplete")
        );
    }
    #[test]
    fn live_allowlist_retains_active_and_valuable_content() {
        let root = fixture_root("live-allowlist");
        let item = live_fixture(&root, "git-ABC123");
        for id in [
            "../sessions",
            "sessions/old.jsonl",
            "state_5.sqlite",
            "log/codex-tui.log",
            ".tmp/git-ABC123/HEAD",
            "plugins/cache",
            "tmp/arg0",
            ".tmp/git-../",
        ] {
            assert!(!live_id_allowed(id), "{id}");
        }
        let home = root.canonicalize().unwrap();
        let opened = vec![item.canonicalize().unwrap().join("HEAD")];
        let (items, _) = live_scan(&home, &Ok(opened)).unwrap();
        assert!(!items[0].can_remove);
        assert!(items[0]
            .blocking_reason
            .as_ref()
            .unwrap()
            .contains("active process"));
        fs::write(item.join("config"), b"unexpected").unwrap();
        assert!(live_stamps(&item, ".tmp/git-ABC123", true).is_err());
        fs::remove_file(item.join("config")).unwrap();
        fs::write(item.join("HEAD"), b"recent").unwrap();
        assert!(live_stamps(&item, ".tmp/git-ABC123", true)
            .unwrap_err()
            .contains("7 days"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn live_cleanup_rejects_links_and_renderer_path_tampering() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("live-link-guards");
        let item = live_fixture(&root, "git-ABC123");
        let outside = root.join("valuable.txt");
        fs::write(&outside, b"valuable").unwrap();
        std::os::unix::fs::symlink(&outside, item.join("objects/linked")).unwrap();
        assert!(live_stamps(&item, ".tmp/git-ABC123", true).is_err());
        fs::remove_file(item.join("objects/linked")).unwrap();
        fs::remove_dir_all(&item).unwrap();
        live_fixture(&root, "git-ABC123");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let mut plan = fixture_live_plan(&root, ".tmp/git-ABC123");
        plan.entries[0].path = outside.display().to_string();
        assert!(execute_live_cleanup_plan(plan)
            .unwrap_err()
            .contains("altered"));
        assert_eq!(fs::read(&outside).unwrap(), b"valuable");
        assert!(item.exists());
        let root_real = root.join("real-tmp");
        fs::rename(root.join(".tmp"), &root_real).unwrap();
        std::os::unix::fs::symlink(&root_real, root.join(".tmp")).unwrap();
        assert!(live_source(&root.canonicalize().unwrap(), ".tmp/git-ABC123").is_err());
        match previous {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn live_plan_inspects_only_selected_item_and_rejects_tampering() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("live-direct-selection");
        let item = live_fixture(&root, "git-ABC123");
        // This unrelated protected root would make a full inventory fail.
        // A selected temporary item must remain independently reviewable.
        std::os::unix::fs::symlink(root.join("sessions"), root.join("log")).unwrap();
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        assert!(live_scan(&root.canonicalize().unwrap(), &Ok(Vec::new())).is_err());
        let plan = create_live_cleanup_plan(vec![".tmp/git-ABC123".into()]).unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].id, ".tmp/git-ABC123");
        assert_eq!(
            plan.estimated_bytes,
            allocated_bytes(&fs::metadata(item.join("HEAD")).unwrap())
        );
        let mut altered = plan.clone();
        altered.estimated_bytes += 1;
        assert!(execute_live_cleanup_plan(altered)
            .unwrap_err()
            .contains("altered"));
        assert!(item.is_dir());
        fs::write(item.join("HEAD"), b"recent change").unwrap();
        assert!(create_live_cleanup_plan(vec![".tmp/git-ABC123".into()])
            .unwrap_err()
            .contains("7 days"));
        assert!(create_live_cleanup_plan(vec!["../sessions".into()])
            .unwrap_err()
            .contains("allowlisted"));
        match previous {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn live_partial_batch_restores_saved_item_while_later_source_changed() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("live-partial-batch");
        let a = live_fixture(&root, "git-AAA111");
        let b = live_fixture(&root, "git-BBB222");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let home = root.canonicalize().unwrap();
        let (items, stamps) = live_scan(&home, &Ok(Vec::new())).unwrap();
        let plan = LiveCleanupPlan {
            plan_id: format!(
                "{}-{}",
                operation_id("live"),
                LIVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            codex_home: home.display().to_string(),
            created_at: Utc::now().to_rfc3339(),
            estimated_bytes: items.iter().map(|i| i.bytes).sum(),
            entries: items,
        };
        let quarantine = ensure_operation_dir(&home, &plan.plan_id).unwrap();
        let saved = quarantine.join("live/.tmp/git-AAA111");
        ensure_directory_chain(&quarantine, saved.parent().unwrap(), "fixture").unwrap();
        fs::rename(&a, &saved).unwrap();
        let op = LiveOperation {
            plan: plan.clone(),
            stamps,
            status: "failed".into(),
            moved: vec![".tmp/git-AAA111".into()],
        };
        live_write(&home, &op).unwrap();
        fs::write(b.join("HEAD"), b"changed after preview").unwrap();
        restore_live_cleanup_operation(plan.plan_id).unwrap();
        assert!(a.join("HEAD").is_file());
        assert_eq!(fs::read(b.join("HEAD")).unwrap(), b"changed after preview");
        match previous {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn live_cleanup_round_trip_and_interrupted_purge_fixture() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("live-roundtrip");
        let item = live_fixture(&root, "git-ABC123");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let plan = fixture_live_plan(&root, ".tmp/git-ABC123");
        let result = execute_live_cleanup_plan(plan.clone()).unwrap();
        assert_eq!(result.freed_bytes, 0);
        assert_eq!(result.moved_items, 1);
        assert!(!item.exists());
        // A crash-leaked old atomic-write temp must not block recovery.
        let quarantine = recovery_dir(&root).join(&plan.plan_id);
        fs::write(
            quarantine.join("live-operation.json.tmp"),
            b"interrupted write",
        )
        .unwrap();
        restore_live_cleanup_operation(plan.plan_id.clone()).unwrap();
        assert_eq!(
            fs::read(item.join("HEAD")).unwrap(),
            b"ref: refs/heads/main\n"
        );
        let plan = fixture_live_plan(&root, ".tmp/git-ABC123");
        execute_live_cleanup_plan(plan.clone()).unwrap();
        assert!(purge_live_cleanup_operation(plan.plan_id.clone(), "wrong".into()).is_err());
        let home = root.canonicalize().unwrap();
        let mut op = live_read(&home, &plan.plan_id).unwrap();
        op.status = "purging".into();
        live_write(&home, &op).unwrap();
        let saved = recovery_dir(&root)
            .join(&plan.plan_id)
            .join("live/.tmp/git-ABC123");
        fs::remove_file(saved.join("HEAD")).unwrap(); // interruption after first unlink
        let purged =
            purge_live_cleanup_operation(plan.plan_id.clone(), plan.plan_id.clone()).unwrap();
        assert_eq!(purged.freed_bytes, 0);
        assert!(!saved.exists());
        assert_eq!(live_read(&home, &plan.plan_id).unwrap().status, "purged");
        assert!(restore_live_cleanup_operation(plan.plan_id).is_err());
        match previous {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn known_current_codex_schema_supports_recoverable_cleanup_and_keeps_queue_revision() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("current-schema");
        create_fixture_databases(&root);
        let state = root.join("state_5.sqlite");
        let history = root.join("thread_history_1.sqlite");
        let queue = root.join("queue_1.sqlite");
        let goals = root.join("goals_1.sqlite");
        let db = |path: &Path| Connection::open(path).unwrap();
        db(&state).execute_batch("
            ALTER TABLE threads ADD COLUMN attached_thread_parent_id TEXT;
            CREATE TABLE thread_sections(id TEXT PRIMARY KEY);
            CREATE TABLE thread_read_receipts(thread_id TEXT PRIMARY KEY, revision TEXT);
            INSERT INTO thread_read_receipts VALUES('thread-1', 'r1');
            CREATE TABLE thread_attachments(id TEXT PRIMARY KEY, thread_id TEXT REFERENCES threads(id) ON DELETE CASCADE);
            INSERT INTO thread_attachments VALUES('a1', 'thread-1');
            CREATE TABLE guardian_review_feedback(id TEXT PRIMARY KEY, thread_id TEXT REFERENCES threads(id) ON DELETE CASCADE);
            INSERT INTO guardian_review_feedback VALUES('r1', 'thread-1');
        ").unwrap();
        db(&history)
            .execute_batch(
                "
            CREATE TABLE thread_realtime_items(thread_id TEXT PRIMARY KEY, item_json TEXT);
            INSERT INTO thread_realtime_items VALUES('thread-1', '{}');
        ",
            )
            .unwrap();
        db(&queue).execute_batch("
            CREATE TABLE queued_items(id TEXT PRIMARY KEY, thread_id TEXT);
            CREATE TABLE queued_thread_revisions(thread_id TEXT PRIMARY KEY, revision INTEGER);
            INSERT INTO queued_items VALUES('q1', 'thread-1');
            INSERT INTO queued_thread_revisions VALUES('thread-1', 3);
            CREATE TRIGGER queue_delete AFTER DELETE ON queued_items BEGIN
                UPDATE queued_thread_revisions SET revision = revision + 1 WHERE thread_id = OLD.thread_id;
            END;
        ").unwrap();
        db(&goals).execute_batch("
            CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY);
            CREATE TABLE thread_goal_continuation_deferrals(thread_id TEXT PRIMARY KEY REFERENCES thread_goals(thread_id) ON DELETE CASCADE);
            INSERT INTO thread_goals VALUES('thread-1');
            INSERT INTO thread_goal_continuation_deferrals VALUES('thread-1');
        ").unwrap();
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let plan = create_cleanup_plan(vec!["thread-1".into()]).unwrap();
        let count = |path: &Path, table: &str| {
            db(path)
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap()
        };
        let tables = [
            (&state, "threads"),
            (&state, "thread_attachments"),
            (&state, "thread_read_receipts"),
            (&history, "thread_realtime_items"),
            (&queue, "queued_items"),
            (&goals, "thread_goal_continuation_deferrals"),
        ];
        assert!(tables.iter().all(|(path, table)| count(path, table) == 1));
        let result = execute_cleanup_plan(plan).unwrap();
        assert!(tables.iter().all(|(path, table)| count(path, table) == 0));
        assert_eq!(count(&queue, "queued_thread_revisions"), 1);
        db(&state)
            .execute_batch(
                "ALTER TABLE threads ADD COLUMN later TEXT NOT NULL DEFAULT 'new-default'",
            )
            .unwrap();
        restore_cleanup_operation(result.operation_id).unwrap();
        assert!(tables.iter().all(|(path, table)| count(path, table) == 1));
        let revision: u64 = db(&queue)
            .query_row("SELECT revision FROM queued_thread_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revision, 4);
        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_conversation_schema_fails_closed_without_mutation() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("unsupported-schema");
        create_fixture_databases(&root);
        let connection = Connection::open(root.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch("CREATE TABLE thread_read_receipts(thread_id TEXT);")
            .unwrap();
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        assert!(create_cleanup_plan(vec!["thread-1".into()])
            .unwrap_err()
            .contains("newer Codex schema"));
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        connection
            .execute_batch("DROP TABLE thread_read_receipts;")
            .unwrap();
        fs::write(root.join("queue_1.sqlite"), b"queue fixture").unwrap();
        assert!(create_cleanup_plan(vec!["thread-1".into()])
            .unwrap_err()
            .contains("queue_1.sqlite"));
        match previous {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn quick_scan_marks_nested_measurements_incomplete() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("quick-scan");
        create_fixture_databases(&root);
        fs::create_dir_all(root.join("worktrees/project/deep/node_modules")).unwrap();
        fs::write(
            root.join("worktrees/project/deep/node_modules/fixture"),
            [7; 4096],
        )
        .unwrap();
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let quick = scan_storage_fast().unwrap();
        assert_eq!(quick.scan_mode, "quick");
        assert!(quick.omitted_categories.contains(&"worktrees".into()));
        assert_eq!(
            quick
                .categories
                .iter()
                .find(|c| c.id == "worktrees")
                .unwrap()
                .bytes,
            0
        );
        assert!(quick.warnings.iter().any(|s| s.contains("Quick preview")));
        match previous {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn purge_only_confirmed_completed_fixture_recovery() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("purge");
        create_fixture_databases(&root);
        fs::write(root.join("sessions/2026/08/thread-1.jsonl"), b"rollout").unwrap();
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let plan = create_cleanup_plan(vec!["thread-1".into()]).unwrap();
        execute_cleanup_plan(plan.clone()).unwrap();
        assert!(purge_cleanup_operation(plan.operation_id.clone(), "wrong".into()).is_err());
        assert!(purge_cleanup_operation("../sessions".into(), "../sessions".into()).is_err());
        let quarantine = recovery_dir(&root).join(&plan.operation_id);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("state_5.sqlite"), quarantine.join("evil-link"))
                .unwrap();
            assert!(
                purge_cleanup_operation(plan.operation_id.clone(), plan.operation_id.clone())
                    .is_err()
            );
            fs::remove_file(quarantine.join("evil-link")).unwrap();
        }
        assert!(
            purge_cleanup_operation(plan.operation_id.clone(), plan.operation_id.clone()).unwrap()
                > 0
        );
        assert!(!quarantine.exists());
        assert!(root.join("state_5.sqlite").exists());
        assert_eq!(list_cleanup_operations().unwrap()[0].status, "purged");
        assert!(restore_cleanup_operation(plan.operation_id.clone()).is_err());
        if let Some(previous) = previous {
            env::set_var("CODEX_HOME", previous);
        } else {
            env::remove_var("CODEX_HOME");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn all_journal_mutations_reject_held_lock() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("mutation-lock");
        create_fixture_databases(&root);
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let plan = create_cleanup_plan(vec!["thread-1".into()]).unwrap();
        let lock = MUTATION_LOCK.lock().unwrap();
        assert!(execute_cleanup_plan(plan)
            .unwrap_err()
            .contains("in progress"));
        assert!(restore_cleanup_operation("missing".into())
            .unwrap_err()
            .contains("in progress"));
        assert!(restore_compaction_operation("missing".into())
            .unwrap_err()
            .contains("in progress"));
        assert!(restore_managed_cleanup_operation("missing".into())
            .unwrap_err()
            .contains("in progress"));
        drop(lock);
        if let Some(previous) = previous {
            env::set_var("CODEX_HOME", previous);
        } else {
            env::remove_var("CODEX_HOME");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_ids_are_global_and_legacy_aliases_cannot_purge_or_restore() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("global-operation-ids");
        create_fixture_databases(&root);
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::write(root.join("cache/item"), b"precious cache").unwrap();
        fs::write(root.join("sessions/2026/08/thread-1.jsonl"), b"rollout").unwrap();
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let managed =
            create_managed_cleanup_plan("cache".into(), Some(vec!["cache/item".into()])).unwrap();
        execute_managed_cleanup_plan(managed.clone()).unwrap();
        let mut cleanup = create_cleanup_plan(vec!["thread-1".into()]).unwrap();
        cleanup.operation_id = managed.operation_id.clone();
        assert!(
            execute_cleanup_plan(cleanup.clone()).is_err(),
            "managed->cleanup alias must fail"
        );
        assert!(root.join("sessions/2026/08/thread-1.jsonl").exists());
        cleanup.operation_id = "cleanup-unique-fixture".into();
        execute_cleanup_plan(cleanup.clone()).unwrap();
        let mut compact = create_compaction_plan().unwrap();
        compact.operation_id = cleanup.operation_id.clone();
        assert!(
            execute_compaction_plan(compact).is_err(),
            "cleanup->compaction alias must fail"
        );
        let mut journal = read_journal(&root).unwrap();
        let mut alias = journal.operations[0].clone();
        alias.operation_id = managed.operation_id.clone();
        alias.plan.operation_id = managed.operation_id.clone();
        alias.quarantine_dir = recovery_dir(&root)
            .join(&managed.operation_id)
            .display()
            .to_string();
        alias.database_backups.clear();
        alias.moved_files.clear();
        alias.moved_file_count = 0;
        journal.operations.push(alias.clone());
        write_journal(&root, &journal).unwrap();
        assert!(purge_cleanup_operation(
            managed.operation_id.clone(),
            managed.operation_id.clone()
        )
        .is_err());
        assert!(recovery_dir(&root)
            .join(&managed.operation_id)
            .join("managed/cache/item")
            .exists());
        journal.operations.last_mut().unwrap().status = "purged".into();
        write_journal(&root, &journal).unwrap();
        assert!(restore_managed_cleanup_operation(managed.operation_id.clone()).is_err());
        journal.operations.pop();
        write_journal(&root, &journal).unwrap();
        restore_managed_cleanup_operation(managed.operation_id).unwrap();
        assert_eq!(
            fs::read(root.join("cache/item")).unwrap(),
            b"precious cache"
        );
        if let Some(previous) = previous {
            env::set_var("CODEX_HOME", previous);
        } else {
            env::remove_var("CODEX_HOME");
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture_root(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "codex-storage-manager-{name}-{}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove stale fixture");
        }
        fs::create_dir_all(root.join("sessions/2026/08")).expect("create fixture dirs");
        root
    }

    fn create_fixture_databases(root: &Path) {
        let state = Connection::open(root.join("state_5.sqlite")).expect("open state fixture");
        state.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL, title TEXT NOT NULL, archived INTEGER NOT NULL, updated_at_ms INTEGER, recency_at_ms INTEGER);
             CREATE TABLE thread_artifacts (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, artifact_type TEXT NOT NULL, identity_key TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE thread_dynamic_tools (thread_id TEXT NOT NULL, position INTEGER NOT NULL, name TEXT NOT NULL, description TEXT NOT NULL, input_schema TEXT NOT NULL, PRIMARY KEY(thread_id, position));
             CREATE TABLE thread_spawn_edges (parent_thread_id TEXT NOT NULL, child_thread_id TEXT NOT NULL PRIMARY KEY, status TEXT NOT NULL);
             INSERT INTO threads VALUES ('thread-1', 'sessions/2026/08/thread-1.jsonl', '/tmp/project', 'Fixture conversation', 1, 100, 100);
             INSERT INTO thread_artifacts VALUES ('artifact-1', 'thread-1', 'file', 'one', '{}', 100);
             INSERT INTO thread_dynamic_tools VALUES ('thread-1', 0, 'tool', 'fixture', '{}');
             INSERT INTO thread_spawn_edges VALUES ('thread-1', 'child-1', 'completed');",
        ).expect("create state schema");

        let history =
            Connection::open(root.join("thread_history_1.sqlite")).expect("open history fixture");
        history.execute_batch(
            "CREATE TABLE thread_items (thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, item_id TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, item_json TEXT NOT NULL, PRIMARY KEY(thread_id, turn_id, item_id));
             CREATE TABLE thread_turns (thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL, status TEXT NOT NULL, PRIMARY KEY(thread_id, turn_id));
             CREATE TABLE thread_history_projection_state (thread_id TEXT PRIMARY KEY, next_rollout_byte_offset INTEGER NOT NULL, next_rollout_ordinal INTEGER NOT NULL);
             INSERT INTO thread_items VALUES ('thread-1', 'turn-1', 'item-1', 0, 100, '{}');
             INSERT INTO thread_turns VALUES ('thread-1', 'turn-1', 0, 'completed');
             INSERT INTO thread_history_projection_state VALUES ('thread-1', 0, 0);",
        ).expect("create history schema");

        let logs = Connection::open(root.join("logs_2.sqlite")).expect("open logs fixture");
        logs.execute_batch(
            "CREATE TABLE logs (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, ts_nanos INTEGER NOT NULL, level TEXT NOT NULL, target TEXT NOT NULL, feedback_log_body TEXT, module_path TEXT, file TEXT, line INTEGER, thread_id TEXT, process_uuid TEXT, estimated_bytes INTEGER NOT NULL DEFAULT 0);
             INSERT INTO logs (id, ts, ts_nanos, level, target, thread_id) VALUES (1, 100, 0, 'INFO', 'fixture', 'thread-1');",
        ).expect("create logs schema");
    }

    #[test]
    fn cleanup_round_trip_restores_fixture() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("round-trip");
        create_fixture_databases(&root);
        fs::write(
            root.join("sessions/2026/08/thread-1.jsonl"),
            b"fixture rollout\n",
        )
        .expect("write rollout");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let mut plan =
            create_cleanup_plan(vec!["thread-1".to_string()]).expect("create cleanup plan");
        let mut legacy = serde_json::to_value(&plan).expect("serialize legacy plan");
        legacy.as_object_mut().unwrap().remove("selection_summary");
        assert!(serde_json::from_value::<CleanupPlan>(legacy)
            .unwrap()
            .selection_summary
            .is_none());
        plan.selection_summary = Some("2026-08 · Asia/Shanghai".to_string());
        assert_eq!(plan.entries.len(), 1);
        let result = execute_cleanup_plan(plan.clone()).expect("execute cleanup plan");
        assert_eq!(
            list_cleanup_operations().unwrap()[0].selection_summary,
            plan.selection_summary
        );
        assert_eq!(result.removed_threads, 1);
        assert!(!root.join("sessions/2026/08/thread-1.jsonl").exists());
        let state =
            Connection::open(root.join("state_5.sqlite")).expect("open state after cleanup");
        assert_eq!(
            state
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            state
                .query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );

        restore_cleanup_operation(result.operation_id).expect("restore cleanup");
        assert!(root.join("sessions/2026/08/thread-1.jsonl").exists());
        let restored = Connection::open(root.join("state_5.sqlite")).expect("open restored state");
        assert_eq!(
            restored
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            restored
                .query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            restored
                .query_row("SELECT COUNT(*) FROM thread_artifacts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let restored_logs =
            Connection::open(root.join("logs_2.sqlite")).expect("open restored logs");
        assert_eq!(
            restored_logs
                .query_row(
                    "SELECT COUNT(*) FROM logs WHERE thread_id = 'thread-1'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn cleanup_current_conversation_round_trip() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("current-round-trip");
        create_fixture_databases(&root);
        fs::write(
            root.join("sessions/2026/08/thread-1.jsonl"),
            b"current fixture rollout\n",
        )
        .expect("write current rollout");
        Connection::open(root.join("state_5.sqlite"))
            .expect("open state fixture")
            .execute("UPDATE threads SET archived = 0 WHERE id = 'thread-1'", [])
            .expect("mark current thread");

        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let plan = create_cleanup_plan(vec!["thread-1".to_string()])
            .expect("create cleanup plan for current conversation");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].thread_id, "thread-1");
        let result =
            execute_cleanup_plan(plan).expect("execute cleanup plan for current conversation");
        assert_eq!(result.removed_threads, 1);
        assert!(!root.join("sessions/2026/08/thread-1.jsonl").exists());

        restore_cleanup_operation(result.operation_id).expect("restore current conversation");
        assert!(root.join("sessions/2026/08/thread-1.jsonl").exists());
        let restored = Connection::open(root.join("state_5.sqlite")).expect("open restored state");
        assert_eq!(
            restored
                .query_row(
                    "SELECT archived FROM threads WHERE id = 'thread-1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn sqlite_estimate_reports_free_pages() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("estimate");
        let path = root.join("state_5.sqlite");
        let connection = Connection::open(&path).expect("open estimate fixture");
        connection.execute_batch("CREATE TABLE payload (value TEXT); BEGIN; INSERT INTO payload VALUES ('x'); COMMIT; DELETE FROM payload;").expect("create free page");
        let estimate = sqlite_estimate(&path).expect("estimate sqlite");
        assert!(estimate.page_size > 0);
        assert!(estimate.page_count > 0);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn cleanup_plan_rejects_tampered_rollout_path() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("tamper");
        create_fixture_databases(&root);
        fs::write(
            root.join("sessions/2026/08/thread-1.jsonl"),
            b"fixture rollout\n",
        )
        .expect("write rollout");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let mut plan = create_cleanup_plan(vec!["thread-1".to_string()]).expect("create plan");
        plan.entries[0].rollout_path = root.join("state_5.sqlite").display().to_string();
        assert!(execute_cleanup_plan(plan).is_err());
        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn cleanup_plan_rejects_another_valid_rollout_path() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("tamper-valid-rollout");
        create_fixture_databases(&root);
        let original = root.join("sessions/2026/08/thread-1.jsonl");
        let other = root.join("sessions/2026/08/other.jsonl");
        fs::write(&original, b"fixture rollout\n").expect("write rollout");
        fs::write(&other, b"other rollout\n").expect("write other rollout");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let mut plan = create_cleanup_plan(vec!["thread-1".to_string()]).expect("create plan");
        plan.entries[0].rollout_path = other.display().to_string();
        plan.entries[0].fingerprint = Some(file_fingerprint(&other).expect("fingerprint other"));
        assert!(execute_cleanup_plan(plan).is_err());
        assert!(original.exists());
        assert!(other.exists());

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn compaction_plan_requires_matching_fingerprints() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("compaction-fingerprint");
        create_fixture_databases(&root);
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let mut plan = create_compaction_plan().expect("create compaction plan");
        plan.fingerprints.clear();
        assert!(execute_compaction_plan(plan).is_err());

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn compaction_artifacts_preserve_duplicate_basenames() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("duplicate-basename");
        let quarantine = root.join("recovery/codex-storage-manager/compact-test");
        let nested = root.join("sqlite/state_5.sqlite");
        fs::create_dir_all(nested.parent().unwrap()).expect("create sqlite dir");
        fs::write(&nested, b"fixture").expect("write nested database");
        let (_, root_backup) =
            compaction_artifact_paths(&quarantine, &root, &root.join("state_5.sqlite"))
                .expect("root artifact");
        let (_, nested_backup) =
            compaction_artifact_paths(&quarantine, &root, &nested).expect("nested artifact");
        assert_ne!(root_backup, nested_backup);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn compaction_round_trip_persists_and_restores_journal() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("compaction-round-trip");
        create_fixture_databases(&root);
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let plan = create_compaction_plan().expect("create compaction plan");
        let operation_id = plan.operation_id.clone();
        execute_compaction_plan(plan).expect("execute compaction plan");
        let journal = read_journal(&root).expect("read compaction journal");
        let operation = journal
            .compactions
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("find completed compaction");
        assert_eq!(operation.status, "completed");
        assert!(operation
            .databases
            .iter()
            .all(|database| database.stage == "replaced"));
        let summaries = list_cleanup_operations().expect("list compaction activity");
        assert!(summaries.iter().any(|summary| {
            summary.operation_id == operation_id
                && summary.operation_type == "compaction"
                && summary.database_count == 3
        }));

        let result =
            restore_compaction_operation(operation_id.clone()).expect("restore compaction");
        assert_eq!(result.moved_files, 3);
        let restored = read_journal(&root).expect("read restored journal");
        assert_eq!(
            restored
                .compactions
                .iter()
                .find(|operation| operation.operation_id == operation_id)
                .expect("find restored compaction")
                .status,
            "restored"
        );
        for database in ["state_5.sqlite", "thread_history_1.sqlite", "logs_2.sqlite"] {
            assert!(root.join(database).is_file());
        }

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn interrupted_compaction_restores_database_and_sidecar_from_journal() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("compaction-crash-recovery");
        create_fixture_databases(&root);
        let database_path = root.join("state_5.sqlite");
        let original_sidecar = PathBuf::from(format!("{}-wal", database_path.display()));
        fs::write(&original_sidecar, b"wal-before").expect("write sidecar");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let quarantine = recovery_dir(&root).join("compact-crash-test");
        let (temp, backup) = compaction_artifact_paths(&quarantine, &root, &database_path)
            .expect("get compaction artifacts");
        fs::rename(&database_path, &backup).expect("move original database");
        let saved_sidecar = PathBuf::from(format!("{}-wal", backup.display()));
        fs::rename(&original_sidecar, &saved_sidecar).expect("move sidecar");
        fs::write(&database_path, b"compacted-placeholder").expect("write compacted database");

        let estimate = sqlite_estimate(&backup).expect("estimate backup");
        let fingerprint = file_fingerprint(&backup).expect("fingerprint backup");
        let plan = CompactionPlan {
            operation_id: "compact-crash-test".to_string(),
            codex_home: root.display().to_string(),
            created_at: Utc::now().to_rfc3339(),
            databases: vec![SqliteEstimate {
                path: database_path.display().to_string(),
                ..estimate
            }],
            fingerprints: vec![FileFingerprint {
                path: database_path.display().to_string(),
                ..fingerprint
            }],
            estimated_bytes: 0,
        };
        let mut journal = Journal::default();
        journal.compactions.push(JournalCompaction {
            operation_id: plan.operation_id.clone(),
            codex_home: root.display().to_string(),
            quarantine_dir: quarantine.display().to_string(),
            plan,
            databases: vec![JournalCompactionDatabase {
                path: database_path.display().to_string(),
                temp_path: temp.display().to_string(),
                backup_path: backup.display().to_string(),
                sidecars: vec![(
                    original_sidecar.display().to_string(),
                    saved_sidecar.display().to_string(),
                )],
                stage: "replaced".to_string(),
            }],
            status: "running".to_string(),
        });
        write_journal(&root, &journal).expect("write interrupted journal");
        let snapshot = scan_storage().expect("scan interrupted compaction");
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.contains("compact-crash-test")));

        let result = restore_compaction_operation("compact-crash-test".to_string())
            .expect("restore interrupted compaction");
        assert_eq!(result.moved_files, 1);
        assert!(Connection::open(&database_path).is_ok());
        assert_eq!(
            fs::read(&original_sidecar).expect("read restored sidecar"),
            b"wal-before"
        );
        assert!(backup.with_extension("compacted").is_file());
        assert!(!temp.exists());

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn interrupted_cleanup_restores_all_predeclared_moves() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("cleanup-crash-recovery");
        create_fixture_databases(&root);
        Connection::open(root.join("state_5.sqlite"))
            .expect("open state fixture")
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, 1, 200, 200)",
                params![
                    "thread-2",
                    "sessions/2026/08/thread-2.jsonl",
                    "/tmp/project",
                    "Second fixture conversation"
                ],
            )
            .expect("insert second thread");
        let first = root.join("sessions/2026/08/thread-1.jsonl");
        let second = root.join("sessions/2026/08/thread-2.jsonl");
        fs::write(&first, b"first rollout\n").expect("write first rollout");
        fs::write(&second, b"second rollout\n").expect("write second rollout");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let plan = create_cleanup_plan(vec!["thread-1".to_string(), "thread-2".to_string()])
            .expect("create cleanup plan");
        let operation_id = plan.operation_id.clone();
        let quarantine = ensure_operation_dir(&root, &operation_id).expect("create quarantine");
        let mut database_backups = Vec::new();
        for path in cleanup_database_paths(&root) {
            if let Some(backup) =
                backup_database(&path, &quarantine, &plan.thread_ids).expect("backup database")
            {
                database_backups.push(backup);
            }
        }
        let first_destination = quarantine
            .join("files")
            .join("sessions/2026/08/thread-1.jsonl");
        ensure_directory_chain(
            &quarantine,
            first_destination.parent().unwrap(),
            "清理隔离文件目录",
        )
        .expect("create cleanup quarantine path");
        fs::rename(&first, &first_destination).expect("move first rollout");
        let second_destination = quarantine
            .join("files")
            .join("sessions/2026/08/thread-2.jsonl");
        let mut journal = read_journal(&root).expect("read empty journal");
        journal.operations.push(JournalOperation {
            operation_id: operation_id.clone(),
            codex_home: root.display().to_string(),
            quarantine_dir: quarantine.display().to_string(),
            plan,
            database_backups,
            moved_files: vec![
                (
                    first.display().to_string(),
                    first_destination.display().to_string(),
                ),
                (
                    second.display().to_string(),
                    second_destination.display().to_string(),
                ),
            ],
            moved_file_count: 1,
            status: "moving".to_string(),
        });
        write_journal(&root, &journal).expect("write interrupted cleanup journal");

        let persisted = read_journal(&root).expect("read interrupted cleanup journal");
        assert_eq!(persisted.operations[0].moved_files.len(), 2);
        let result =
            restore_cleanup_operation(operation_id.clone()).expect("restore interrupted cleanup");
        assert_eq!(result.moved_files, 1);
        assert!(first.is_file());
        assert!(second.is_file());
        assert!(!first_destination.exists());
        assert!(!second_destination.exists());
        assert_eq!(
            read_journal(&root)
                .expect("read restored cleanup journal")
                .operations[0]
                .status,
            "restored"
        );

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn database_discovery_includes_db_and_nested_duplicate_names() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("database-discovery");
        create_fixture_databases(&root);
        let nested = root.join("sqlite/state_5.sqlite");
        fs::create_dir_all(nested.parent().unwrap()).expect("create nested sqlite dir");
        Connection::open(&nested)
            .expect("create nested sqlite database")
            .execute_batch("CREATE TABLE nested (value TEXT);")
            .expect("create nested schema");
        let db = root.join("sqlite/plugin.db");
        Connection::open(&db)
            .expect("create db database")
            .execute_batch("CREATE TABLE plugin (value TEXT);")
            .expect("create db schema");

        let paths = database_paths(&root);
        assert!(paths.contains(&root.join("state_5.sqlite")));
        assert!(paths.contains(&nested));
        assert!(paths.contains(&db));
        assert_eq!(
            paths
                .iter()
                .filter(|path| path
                    .file_name()
                    .is_some_and(|name| name == "state_5.sqlite"))
                .count(),
            2
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn database_operations_reject_symlinks() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("database-symlink");
        create_fixture_databases(&root);
        let outside = root.with_file_name(format!(
            "codex-storage-manager-database-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_file(&outside).expect("remove stale outside database");
        }
        Connection::open(&outside)
            .expect("create outside database")
            .execute_batch("CREATE TABLE external (value TEXT);")
            .expect("create outside schema");
        let symlink = root.join("sqlite/external.db");
        fs::create_dir_all(symlink.parent().unwrap()).expect("create sqlite directory");
        std::os::unix::fs::symlink(&outside, &symlink).expect("create nested database symlink");

        let state = root.join("state_5.sqlite");
        let state_real = root.join("state-real.sqlite");
        fs::rename(&state, &state_real).expect("move state fixture");
        std::os::unix::fs::symlink(&outside, &state).expect("create cleanup database symlink");

        assert!(!database_paths(&root).contains(&symlink));
        assert!(!database_paths(&root).contains(&state));
        assert!(!cleanup_database_paths(&root).contains(&state));
        assert!(!is_allowed_database_path(&root, &symlink));
        assert!(!is_allowed_database_path(&root, &state));
        let mut warnings = Vec::new();
        assert!(read_conversations(&root, &mut warnings).is_empty());
        assert!(warnings
            .iter()
            .any(|warning| { warning.contains("symlink") && warning.contains("state_5.sqlite") }));

        let real_sqlite_dir = root.join("sqlite-real");
        fs::rename(root.join("sqlite"), &real_sqlite_dir).expect("move sqlite fixture directory");
        let outside_dir = root.with_file_name(format!(
            "codex-storage-manager-database-symlink-dir-outside-{}",
            std::process::id()
        ));
        if outside_dir.exists() {
            fs::remove_dir_all(&outside_dir).expect("remove stale outside database directory");
        }
        fs::create_dir_all(&outside_dir).expect("create outside database directory");
        Connection::open(outside_dir.join("linked.db"))
            .expect("create linked outside database")
            .execute_batch("CREATE TABLE external (value TEXT);")
            .expect("create linked outside schema");
        let linked_sqlite_dir = root.join("sqlite");
        std::os::unix::fs::symlink(&outside_dir, &linked_sqlite_dir)
            .expect("create sqlite directory symlink");
        let linked_database = linked_sqlite_dir.join("linked.db");
        assert!(!database_paths(&root).contains(&linked_database));
        assert!(!is_allowed_database_path(&root, &linked_database));

        fs::remove_dir_all(&root).expect("remove fixture");
        fs::remove_file(outside).expect("remove outside database");
        fs::remove_dir_all(outside_dir).expect("remove outside database directory");
    }

    #[cfg(unix)]
    #[test]
    fn conversation_scan_does_not_follow_rollout_symlinks() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("rollout-symlink-scan");
        create_fixture_databases(&root);
        let outside = root.with_file_name(format!(
            "codex-storage-manager-rollout-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_file(&outside).expect("remove stale outside rollout");
        }
        fs::write(&outside, vec![b'x'; 4096]).expect("write outside rollout");
        let linked = root.join("sessions/2026/08/thread-1.jsonl");
        std::os::unix::fs::symlink(&outside, &linked).expect("create rollout symlink");

        let mut warnings = Vec::new();
        let conversations = read_conversations(&root, &mut warnings);
        assert_eq!(conversations.len(), 1);
        assert!(conversations[0].missing_file);
        assert_eq!(conversations[0].bytes, 0);
        assert_eq!(bytes_for_path(&linked), None);

        fs::remove_dir_all(root).expect("remove fixture");
        fs::remove_file(outside).expect("remove outside rollout");
    }

    #[test]
    fn conversation_scan_counts_external_rollout_but_rejects_deletion() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("external-rollout-scan");
        create_fixture_databases(&root);
        let outside = root.with_file_name(format!(
            "codex-storage-manager-external-rollout-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_file(&outside).expect("remove stale outside rollout");
        }
        fs::write(&outside, vec![b'x'; 4096]).expect("write outside rollout");
        Connection::open(root.join("state_5.sqlite"))
            .expect("open state fixture")
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = 'thread-1'",
                params![outside.display().to_string()],
            )
            .expect("set external rollout path");

        let mut warnings = Vec::new();
        let conversations = read_conversations(&root, &mut warnings);
        assert_eq!(conversations.len(), 1);
        assert!(!conversations[0].missing_file);
        assert!(conversations[0].external);
        assert_eq!(conversations[0].status, "archived");
        assert_eq!(conversations[0].bytes, bytes_for_path(&outside).unwrap());
        assert!(warnings.is_empty());
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        assert!(create_cleanup_plan(vec!["thread-1".to_string()])
            .expect_err("external records remain read-only")
            .contains("暂不支持删除外部记录"));
        let snapshot = scan_storage().expect("scan external records");
        let category = snapshot
            .categories
            .iter()
            .find(|item| item.id == "external-conversations")
            .unwrap();
        assert_eq!(category.bytes, bytes_for_path(&outside).unwrap());
        assert_eq!(category.file_count, 1);
        assert_eq!(category.reclaimable_bytes, 0);
        assert_eq!(
            snapshot.total_bytes,
            snapshot
                .categories
                .iter()
                .map(|item| item.bytes)
                .sum::<u64>()
        );
        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }

        fs::remove_dir_all(root).expect("remove fixture");
        fs::remove_file(outside).expect("remove outside rollout");
    }

    #[test]
    fn external_records_distinguish_missing_and_deduplicate_file_usage() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("external-dedup");
        create_fixture_databases(&root);
        let external_root = fixture_root("external-dedup-records");
        let outside = external_root.join("record.jsonl");
        let hardlink = external_root.join("hardlink.jsonl");
        let missing = external_root.join("missing.jsonl");
        fs::write(&outside, vec![b'x'; 8192]).unwrap();
        fs::hard_link(&outside, &hardlink).unwrap();
        let connection = Connection::open(root.join("state_5.sqlite")).unwrap();
        connection
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = 'thread-1'",
                params![outside.display().to_string()],
            )
            .unwrap();
        for (id, path) in [
            ("duplicate", &outside),
            ("hardlink", &hardlink),
            ("missing", &missing),
        ] {
            connection.execute("INSERT INTO threads VALUES (?1, ?2, '/tmp/project', 'External fixture', 0, 100, 100)", params![id, path.display().to_string()]).unwrap();
        }
        drop(connection);
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let snapshot = scan_storage().unwrap();
        let category = snapshot
            .categories
            .iter()
            .find(|item| item.id == "external-conversations")
            .unwrap();
        assert_eq!(category.bytes, bytes_for_path(&outside).unwrap());
        assert_eq!(category.file_count, 1);
        assert_eq!(category.reclaimable_bytes, 0);
        let missing = snapshot
            .conversations
            .iter()
            .find(|item| item.id == "missing")
            .unwrap();
        assert!(missing.external && missing.missing_file);
        assert_eq!(missing.bytes, 0);
        assert_eq!(
            snapshot
                .conversations
                .iter()
                .filter(|item| !item.missing_file && item.external)
                .count(),
            3
        );
        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(external_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_record_measurement_rejects_symlinks_and_parent_traversal() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("external-measurement-links")
            .canonicalize()
            .unwrap();
        let record = root.join("sessions/record.jsonl");
        fs::write(&record, vec![b'x'; 4096]).unwrap();
        std::os::unix::fs::symlink(&record, root.join("linked.jsonl")).unwrap();
        std::os::unix::fs::symlink(root.join("sessions"), root.join("linked-dir")).unwrap();
        assert!(measured_record_metadata(&root.join("linked.jsonl")).is_err());
        assert!(measured_record_metadata(&root.join("linked-dir/record.jsonl")).is_err());
        assert!(measured_record_metadata(&root.join("sessions/../sessions/record.jsonl")).is_err());
        assert!(measured_record_metadata(&root.join("sessions")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_reclaimable_conversations_includes_current_threads() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("reclaimable-current");
        create_fixture_databases(&root);
        let archived = root.join("sessions/2026/08/thread-1.jsonl");
        let current = root.join("sessions/2026/08/thread-2.jsonl");
        fs::write(&archived, b"archived rollout\n").expect("write archived rollout");
        fs::write(&current, vec![b'x'; 4096]).expect("write current rollout");
        Connection::open(root.join("state_5.sqlite"))
            .expect("open state fixture")
            .execute(
                "INSERT INTO threads VALUES ('thread-2', ?1, '/tmp/project', 'Current fixture conversation', 0, 200, 200)",
                params!["sessions/2026/08/thread-2.jsonl"],
            )
            .expect("insert current thread");

        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let snapshot = scan_storage().expect("scan reclaimable fixture");
        let conversations = snapshot
            .categories
            .iter()
            .find(|category| category.id == "conversations")
            .expect("conversation category");
        let archived_bytes = bytes_for_path(&archived).expect("archived rollout bytes");
        let current_bytes = bytes_for_path(&current).expect("current rollout bytes");
        assert_eq!(
            conversations.reclaimable_bytes,
            archived_bytes + current_bytes
        );
        assert_eq!(conversations.bytes, archived_bytes + current_bytes);

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn snapshot_marks_managed_cache_as_reclaimable() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("cache-not-reclaimable");
        create_fixture_databases(&root);
        let cache_file = root.join("cache/cache-entry.bin");
        fs::create_dir_all(cache_file.parent().expect("cache parent"))
            .expect("create cache directory");
        fs::write(&cache_file, vec![b'x'; 4096]).expect("write cache fixture");
        fs::write(
            root.join("sessions/2026/08/thread-1.jsonl"),
            b"archived rollout\n",
        )
        .expect("write rollout");

        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let snapshot = scan_storage().expect("scan cache fixture");
        let cache = snapshot
            .categories
            .iter()
            .find(|category| category.id == "cache")
            .expect("cache category");
        assert!(cache.bytes >= bytes_for_path(&cache_file).expect("cache bytes"));
        assert!(cache.reclaimable_bytes >= bytes_for_path(&cache_file).expect("cache bytes"));
        assert_eq!(cache.safety, "managed:可管理");

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn managed_cleanup_round_trip_restores_cache_item() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("managed-round-trip");
        create_fixture_databases(&root);
        let item = root.join("cache/cache-entry.bin");
        fs::create_dir_all(item.parent().unwrap()).expect("create cache root");
        fs::write(&item, vec![b'x'; 8192]).expect("write cache item");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);

        let plan = create_managed_cleanup_plan(
            "cache".to_string(),
            Some(vec!["cache/cache-entry.bin".to_string()]),
        )
        .expect("create managed plan");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].file_count, 1);
        assert!(plan.estimated_bytes > 0);
        let result = execute_managed_cleanup_plan(plan.clone()).expect("execute managed plan");
        assert_eq!(result.moved_items, 1);
        assert!(!item.exists());
        let quarantine = recovery_dir(&root)
            .join(&plan.operation_id)
            .join("managed/cache/cache-entry.bin");
        assert!(quarantine.is_file());
        let summaries = list_cleanup_operations().expect("list managed operations");
        assert!(summaries.iter().any(|summary| {
            summary.operation_id == plan.operation_id
                && summary.operation_type == "managed"
                && summary.thread_count == 1
                && summary.database_count == 0
        }));

        let restored =
            restore_managed_cleanup_operation(result.operation_id).expect("restore managed plan");
        assert_eq!(restored.moved_items, 1);
        assert_eq!(
            fs::read(&item).expect("read restored cache item").len(),
            8192
        );
        assert!(!quarantine.exists());

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn managed_cleanup_rejects_path_escape() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("managed-path-escape");
        create_fixture_databases(&root);
        let item = root.join("cache/cache-entry.bin");
        fs::create_dir_all(item.parent().unwrap()).expect("create cache root");
        fs::write(&item, b"fixture").expect("write cache item");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let mut plan =
            create_managed_cleanup_plan("cache".to_string(), None).expect("create managed plan");
        plan.entries[0].id = "cache/../state_5.sqlite".to_string();
        assert!(execute_managed_cleanup_plan(plan).is_err());
        assert!(item.is_file());
        assert!(root.join("state_5.sqlite").is_file());
        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn managed_cleanup_rejects_symlink_items() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("managed-symlink");
        create_fixture_databases(&root);
        let outside = root.with_file_name(format!(
            "codex-storage-manager-managed-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_file(&outside).expect("remove stale outside item");
        }
        fs::write(&outside, b"outside").expect("write outside item");
        let cache_root = root.join("cache");
        fs::create_dir_all(&cache_root).expect("create cache root");
        std::os::unix::fs::symlink(&outside, cache_root.join("linked-item"))
            .expect("create managed symlink");
        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let error = create_managed_cleanup_plan("cache".to_string(), None)
            .expect_err("reject managed symlink");
        assert!(error.contains("symlink"));
        assert!(outside.is_file());
        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
        fs::remove_file(outside).expect("remove outside item");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_and_restore_reject_state_database_symlinks() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("cleanup-state-symlink");
        create_fixture_databases(&root);
        fs::write(
            root.join("sessions/2026/08/thread-1.jsonl"),
            b"fixture rollout\n",
        )
        .expect("write rollout");
        let outside = root.with_file_name(format!(
            "codex-storage-manager-cleanup-state-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_file(&outside).expect("remove stale outside database");
        }
        fs::write(&outside, b"outside database").expect("write outside database");

        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let plan = create_cleanup_plan(vec!["thread-1".to_string()]).expect("create plan");
        let state = root.join("state_5.sqlite");
        let state_real = root.join("state-real.sqlite");
        fs::rename(&state, &state_real).expect("move state fixture");
        std::os::unix::fs::symlink(&outside, &state).expect("create cleanup state symlink");

        let execute_error = execute_cleanup_plan(plan.clone()).expect_err("reject state symlink");
        assert!(execute_error.contains("symlink"));
        assert!(root.join("sessions/2026/08/thread-1.jsonl").is_file());

        fs::remove_file(&state).expect("remove cleanup state symlink");
        fs::rename(&state_real, &state).expect("restore state fixture");
        let result = execute_cleanup_plan(plan).expect("execute cleanup plan");

        fs::rename(&state, &state_real).expect("move cleaned state fixture");
        std::os::unix::fs::symlink(&outside, &state).expect("create restore state symlink");
        let restore_error =
            restore_cleanup_operation(result.operation_id).expect_err("reject restore symlink");
        assert!(restore_error.contains("symlink"));

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
        fs::remove_file(outside).expect("remove outside database");
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_history_and_logs_database_symlinks() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("restore-database-symlink");
        create_fixture_databases(&root);
        fs::write(
            root.join("sessions/2026/08/thread-1.jsonl"),
            b"fixture rollout\n",
        )
        .expect("write rollout");
        fs::remove_file(root.join("thread_history_1.sqlite")).expect("remove history fixture");
        fs::remove_file(root.join("logs_2.sqlite")).expect("remove logs fixture");

        let previous = env::var_os("CODEX_HOME");
        env::set_var("CODEX_HOME", &root);
        let plan = create_cleanup_plan(vec!["thread-1".to_string()]).expect("create plan");
        let result = execute_cleanup_plan(plan).expect("execute cleanup plan");

        let outside_history = root.with_file_name(format!(
            "codex-storage-manager-restore-history-symlink-outside-{}",
            std::process::id()
        ));
        if outside_history.exists() {
            fs::remove_file(&outside_history).expect("remove stale history database");
        }
        Connection::open(&outside_history).expect("create outside history database");
        let history = root.join("thread_history_1.sqlite");
        std::os::unix::fs::symlink(&outside_history, &history)
            .expect("create history database symlink");
        let history_error = restore_cleanup_operation(result.operation_id.clone())
            .expect_err("reject history database symlink");
        assert!(history_error.contains("thread_history_1.sqlite"));
        assert!(history_error.contains("symlink"));

        fs::remove_file(&history).expect("remove history database symlink");
        let outside_logs = root.with_file_name(format!(
            "codex-storage-manager-restore-logs-symlink-outside-{}",
            std::process::id()
        ));
        if outside_logs.exists() {
            fs::remove_file(&outside_logs).expect("remove stale logs database");
        }
        Connection::open(&outside_logs).expect("create outside logs database");
        let logs = root.join("logs_2.sqlite");
        std::os::unix::fs::symlink(&outside_logs, &logs).expect("create logs database symlink");
        let logs_error = restore_cleanup_operation(result.operation_id)
            .expect_err("reject logs database symlink");
        assert!(logs_error.contains("logs_2.sqlite"));
        assert!(logs_error.contains("symlink"));

        match previous {
            Some(value) => env::set_var("CODEX_HOME", value),
            None => env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("remove fixture");
        fs::remove_file(outside_history).expect("remove outside history database");
        fs::remove_file(outside_logs).expect("remove outside logs database");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_directory_rejects_symlink() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("recovery-symlink");
        let outside = root.with_file_name(format!(
            "codex-storage-manager-recovery-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_dir_all(&outside).expect("remove stale outside recovery directory");
        }
        fs::create_dir_all(&outside).expect("create outside recovery directory");
        std::os::unix::fs::symlink(&outside, root.join("recovery"))
            .expect("create recovery directory symlink");

        assert!(read_journal(&root).is_err());
        assert!(write_journal(&root, &Journal::default()).is_err());

        fs::remove_dir_all(&root).expect("remove fixture");
        fs::remove_dir_all(outside).expect("remove outside recovery directory");
    }

    #[test]
    fn reading_missing_journal_does_not_mutate_codex_home() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("journal-read-only");
        assert!(!root.join("recovery").exists());
        assert!(read_journal(&root)
            .expect("read missing journal")
            .operations
            .is_empty());
        assert!(!root.join("recovery").exists());
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn operation_directory_rejects_symlink() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("operation-symlink");
        let outside = root.with_file_name(format!(
            "codex-storage-manager-operation-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_dir_all(&outside).expect("remove stale outside operation directory");
        }
        fs::create_dir_all(&outside).expect("create outside operation directory");
        let recovery = root.join("recovery/codex-storage-manager");
        fs::create_dir_all(&recovery).expect("create recovery directory");
        std::os::unix::fs::symlink(&outside, recovery.join("compact-test"))
            .expect("create operation directory symlink");

        assert!(ensure_operation_dir(&root, "compact-test").is_err());

        fs::remove_dir_all(&root).expect("remove fixture");
        fs::remove_dir_all(outside).expect("remove outside operation directory");
    }

    #[cfg(unix)]
    #[test]
    fn nested_recovery_directories_reject_symlinks() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let root = fixture_root("nested-recovery-symlink");
        let quarantine = ensure_operation_dir(&root, "nested-test").expect("create operation dir");
        let outside = root.with_file_name(format!(
            "codex-storage-manager-nested-recovery-symlink-outside-{}",
            std::process::id()
        ));
        if outside.exists() {
            fs::remove_dir_all(&outside).expect("remove stale outside directory");
        }
        fs::create_dir_all(&outside).expect("create outside directory");

        let files = quarantine.join("files");
        fs::create_dir_all(files.join("sessions")).expect("create files directories");
        let files_symlink = files.join("sessions/2026");
        std::os::unix::fs::symlink(&outside, &files_symlink).expect("create deep files symlink");
        assert!(
            ensure_directory_chain(&quarantine, &files_symlink.join("08"), "清理隔离文件目录",)
                .is_err()
        );
        assert!(validate_directory_chain(
            &quarantine,
            &files_symlink.join("08"),
            "清理隔离文件目录",
        )
        .is_err());

        let databases = quarantine.join("databases");
        fs::create_dir_all(&databases).expect("create database directory");
        let databases_symlink = databases.join("sqlite");
        std::os::unix::fs::symlink(&outside, &databases_symlink)
            .expect("create deep databases symlink");
        assert!(ensure_directory_chain(
            &quarantine,
            &databases_symlink.join("plugin.db"),
            "数据库恢复目录",
        )
        .is_err());
        assert!(validate_directory_chain(
            &quarantine,
            &databases_symlink.join("plugin.db"),
            "数据库恢复目录",
        )
        .is_err());
        let nested_database = root.join("sqlite/plugin.db");
        assert!(compaction_artifact_paths(&quarantine, &root, &nested_database).is_err());

        fs::remove_dir_all(&root).expect("remove fixture");
        fs::remove_dir_all(outside).expect("remove outside directory");
    }
}
