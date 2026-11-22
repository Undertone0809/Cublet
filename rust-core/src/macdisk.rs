//! Read-only, bounded Mac disk inventory. Byte totals use allocated filesystem blocks.
use serde::{Deserialize, Serialize};
pub(crate) mod history;
mod incremental;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
#[cfg(target_os = "macos")]
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_ISSUES: usize = 100;
const MAX_LARGE_FILES: usize = 100;
const WORKERS: usize = scan_parameter(option_env!("MSM_BENCH_WORKERS"), 4, 1, 4);
const FILE_BATCH_SIZE: usize = scan_parameter(option_env!("MSM_BENCH_BATCH"), 2048, 32, 2048);
#[cfg(target_os = "macos")]
const BULK_BUFFER_BYTES: usize = scan_parameter(
    option_env!("MSM_BENCH_BULK_BYTES"),
    16 * 1024,
    8192,
    128 * 1024,
);
const PUBLISH_MS: usize = scan_parameter(option_env!("MSM_BENCH_PUBLISH"), 80, 40, 400);
const QUEUED_JOBS: usize = 24;
const CACHE_VERSION: u32 = 3;
const INDEX_POLICY_VERSION: u32 = 2;
#[cfg(test)]
const MAX_INDEX_DIRECTORIES: usize = 100_000;
const RESERVED_DIRECT_DIRECTORIES: usize = 10_000;
const RESERVED_SHALLOW_DIRECTORIES: usize = 20_000;
const RESERVED_DIRECT_INDEX_BYTES: usize = 8 * 1024 * 1024;
const RESERVED_SHALLOW_INDEX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIRECTORY_PAGE: usize = 500;
// Decoder safety ceiling follows the largest supported publication envelope.
// Actual snapshot admission is derived from its shared disk budget below.
const MAX_CACHE_FILE_BYTES: u64 = 512 * MIB / 2;
const DEFAULT_CACHE_LIMIT_MIB: u64 = 256;
const MIB: u64 = 1024 * 1024;
const CACHE_APP_DIRECTORY: &str = "com.zeeland.codex-storage-manager";
#[cfg(test)]
const MAX_CACHE_FILES: usize = 128;
const MAX_RESIDENT_RUNS: usize = 4;
const MAX_SHALLOW_DIRECTORY_ROWS: usize = 1024;
const MAX_SHALLOW_BYTES: usize = 1024 * 1024;
const MAX_TREEMAP_ROWS: usize = 256;
const MAX_TREEMAP_BYTES: usize = 256 * 1024;
const MAX_APPLICATIONS: usize = 512;
const MAX_COMPARISON_SCOPES: usize = 8;
const MAX_COMPARISON_MEMORY_BYTES: usize = 4 * 1024 * 1024;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(7);
const EARLY_DIRECTORY_ROWS: usize = scan_parameter(option_env!("MSM_BENCH_EARLY_ROWS"), 16, 1, 32);
const EARLY_DIRECTORY_MS: usize = scan_parameter(option_env!("MSM_BENCH_EARLY_MS"), 4, 1, 200);
// Build-time overrides let the fixture runner evaluate policies without adding
// an RPC knob or changing an installed app's policy at runtime.
const fn scan_parameter(value: Option<&str>, fallback: usize, min: usize, max: usize) -> usize {
    if !cfg!(any(test, feature = "benchmark")) {
        return fallback;
    }
    let Some(value) = value else { return fallback };
    let digits = value.as_bytes();
    let mut index = 0;
    let mut result = 0;
    while index < digits.len() {
        let digit = digits[index];
        if digit < b'0' || digit > b'9' {
            return fallback;
        }
        result = result * 10 + (digit - b'0') as usize;
        if result > max {
            return fallback;
        }
        index += 1;
    }
    if result < min {
        fallback
    } else {
        result
    }
}

type DirectoryIndex = HashMap<String, DiskEntry>;

/// Historical file identities have an independent allowance: ordinary files
/// must never displace directory summaries. Counters, including zero attributed
/// hardlink paths and symlinks, let comparison prove a whole direct file layer.
#[derive(Clone, Default, Serialize, Deserialize)]
struct FileDetails {
    rows: HashMap<String, (u64, u64)>,
    #[serde(skip)]
    bytes: usize,
}
impl FileDetails {
    fn retain(&mut self, path: &Path, bytes: u64, logical: u64, limit_mib: u64) -> bool {
        let (max_rows, max_bytes) = match limit_mib {
            64 => (25_000, 4 * MIB as usize),
            512 => (100_000, 16 * MIB as usize),
            _ => (50_000, 8 * MIB as usize),
        };
        let cost = 192 + path.as_os_str().len().saturating_mul(3);
        if self.rows.len() < max_rows && cost <= max_bytes.saturating_sub(self.bytes) {
            self.rows.insert(string(path), (bytes, logical));
            self.bytes += cost;
            true
        } else {
            false
        }
    }
}
fn record_file_detail(run: &Run, progress: &mut Progress, path: &Path, bytes: u64, logical: u64) {
    if run.file_details_full.load(Ordering::Relaxed) {
        return;
    }
    progress.file_detail_bytes += 64 + path.as_os_str().len();
    progress
        .file_details
        .push((path.to_path_buf(), bytes, logical));
    if progress.file_details.len() >= 256 || progress.file_detail_bytes >= 64 * 1024 {
        flush_file_details(run, progress);
    }
}
fn flush_file_details(run: &Run, progress: &mut Progress) {
    if !progress.file_details.is_empty() && !run.file_details_full.load(Ordering::Relaxed) {
        if let Ok(mut details) = run.file_details.lock() {
            for (path, bytes, logical) in progress.file_details.drain(..) {
                if !details.retain(&path, bytes, logical, run.file_detail_limit_mib) {
                    run.file_details_full.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    progress.file_details.clear();
    progress.file_detail_bytes = 0;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCapacity {
    pub path: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskRoot {
    pub path: String,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskOverview {
    pub capacity: DiskCapacity,
    pub default_roots: Vec<DiskRoot>,
    pub scope_note: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskEntry {
    pub path: String,
    pub name: String,
    pub parent_path: String,
    pub kind: String,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub complete: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskApplication {
    pub path: String,
    pub name: String,
    pub bundle_id: Option<String>,
    pub bundle_bytes: u64,
    pub support_bytes: u64,
    pub associated_paths: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskIssue {
    pub path: String,
    pub kind: String,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskScan {
    pub scan_id: String,
    pub status: String,
    pub roots: Vec<String>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    pub elapsed_ms: u64,
    pub scanned_files: u64,
    pub scanned_directories: u64,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub current_path: String,
    pub entries: Vec<DiskEntry>,
    /// Optional display-only partitions; never a complete file inventory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub treemap_entries: Vec<DiskEntry>,
    pub applications: Vec<DiskApplication>,
    #[serde(default)]
    pub application_copies_truncated: bool,
    #[serde(default)]
    application_inventory_complete: bool,
    pub issues: Vec<DiskIssue>,
    pub issue_count: u64,
    pub cache_hit: bool,
    pub cache_age_ms: u64,
    /// False for a running/cancelled scan, or any unreadable/excluded subtree.
    pub coverage_complete: bool,
    pub scope_note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_roots: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_path: Option<String>,
    /// Whether the selected layer contains summaries for every measured direct
    /// child directory. This does not describe deeper index or scan coverage.
    #[serde(default)]
    pub view_detail_complete: bool,
    /// Missing in older v3 caches; scope inspection may upgrade those once.
    #[serde(default)]
    pub index_policy_version: u32,
    /// Origin of an explicit single-directory inspection; polling retains it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_scan_id: Option<String>,
    #[serde(default = "full_scan_method")]
    pub scan_method: String,
    #[serde(default = "verified_freshness")]
    pub freshness: String,
    /// A verified measurement that could not become the saved history endpoint.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_save_failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}
fn full_scan_method() -> String {
    "full".into()
}
fn verified_freshness() -> String {
    "verified".into()
}
fn reusable_snapshot(snapshot: &DiskScan) -> bool {
    snapshot.status == "complete" && !matches!(snapshot.freshness.as_str(), "partial" | "updating")
}

#[derive(Clone, Copy)]
struct CachePolicy {
    index_rows: usize,
    index_bytes: usize,
}
fn cache_policy(limit_mib: u64) -> CachePolicy {
    match limit_mib {
        64 => CachePolicy {
            index_rows: 50_000,
            index_bytes: 24 * MIB as usize,
        },
        512 => CachePolicy {
            index_rows: 300_000,
            index_bytes: 96 * MIB as usize,
        },
        _ => CachePolicy {
            index_rows: 150_000,
            index_bytes: 48 * MIB as usize,
        },
    }
}
/// Additive, disjoint partitions of archived allocated-byte changes.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskScanComparisonEntry {
    pub path: String,
    pub name: String,
    pub parent_path: String,
    pub kind: String,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub growth_bytes: u64,
    pub reduction_bytes: u64,
    pub net_bytes: i64,
    pub detail_limited: bool,
}
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskScanComparison {
    pub status: String,
    /// The displayed pair is complete, while a newer scan is still measuring.
    pub refreshing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_started_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_updated_at_ms: Option<u64>,
    pub current_started_at_ms: u64,
    pub current_updated_at_ms: u64,
    pub roots: Vec<String>,
    pub entries: Vec<DiskScanComparisonEntry>,
    pub growth_bytes: u64,
    pub reduction_bytes: u64,
    pub net_bytes: i64,
    pub limited: bool,
}
/// A current-layer listing is separate from snapshot attribution. Missing sizes
/// mean unmeasured/excluded, never a measured empty directory.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskDirectoryItem {
    pub path: String,
    pub name: String,
    pub parent_path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_bytes: Option<u64>,
    pub measurement: String,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hard_link_identity: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskDirectoryPage {
    pub scan_id: String,
    pub path: String,
    pub source_updated_at_ms: u64,
    pub listed_at_ms: u64,
    pub entries: Vec<DiskDirectoryItem>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_path: Option<String>,
    pub issues: Vec<DiskIssue>,
    pub scope_note: String,
}
#[derive(Default)]
struct IndexAllowance {
    rows: usize,
    bytes: usize,
}
impl IndexAllowance {
    fn retain(&mut self, cost: usize, max_rows: usize, max_bytes: usize) -> bool {
        if self.rows >= max_rows || cost > max_bytes.saturating_sub(self.bytes) {
            return false;
        }
        self.rows += 1;
        self.bytes += cost;
        true
    }
}
struct IndexBudget {
    general: IndexAllowance,
    shallow: IndexAllowance,
    direct: IndexAllowance,
    policy: CachePolicy,
}
impl Default for IndexBudget {
    fn default() -> Self {
        Self {
            general: IndexAllowance::default(),
            shallow: IndexAllowance::default(),
            direct: IndexAllowance::default(),
            policy: cache_policy(configured_cache_limit()),
        }
    }
}
impl IndexBudget {
    fn retain(&mut self, path: &Path, root: &Path, root_is_home: bool) -> bool {
        // The key and displayed row store the path twice, plus name/parent.
        // Cap estimated resident bytes as well as count for very long paths.
        let cost = 192 + path.as_os_str().len().saturating_mul(3);
        let depth = path
            .strip_prefix(root)
            .map(|p| p.components().count())
            .unwrap_or(usize::MAX);
        if depth <= 1
            && self.direct.retain(
                cost,
                RESERVED_DIRECT_DIRECTORIES,
                RESERVED_DIRECT_INDEX_BYTES,
            )
        {
            return true;
        }
        if (depth <= 2 || shallow_directory(path, root, root_is_home))
            && self.shallow.retain(
                cost,
                RESERVED_SHALLOW_DIRECTORIES,
                RESERVED_SHALLOW_INDEX_BYTES,
            )
        {
            return true;
        }
        self.general.retain(
            cost,
            self.policy.index_rows - RESERVED_DIRECT_DIRECTORIES - RESERVED_SHALLOW_DIRECTORIES,
            self.policy.index_bytes - RESERVED_DIRECT_INDEX_BYTES - RESERVED_SHALLOW_INDEX_BYTES,
        )
    }
}
struct Run {
    allow_protected: bool,
    snapshot: Mutex<DiskScan>,
    cancelled: AtomicBool,
    measurement_gap: AtomicBool,
    index: Mutex<DirectoryIndex>,
    file_details: Mutex<FileDetails>,
    file_details_full: AtomicBool,
    file_detail_limit_mib: u64,
    index_complete: AtomicBool,
    index_budget: Mutex<IndexBudget>,
    index_limited: AtomicBool,
    shallow_limited: AtomicBool,
    finalization: Mutex<()>,
    finalized: AtomicBool,
    managed: bool,
    recorder: Mutex<Option<incremental::Recorder>>,
}
#[derive(Clone)]
struct CachedComparison {
    scan_id: String,
    scope: Option<String>,
    result: DiskScanComparison,
    bytes: usize,
    key: String,
    generation: Option<ComparisonGeneration>,
}
#[derive(Clone, PartialEq, Eq)]
struct ComparisonGeneration {
    current_path: PathBuf,
    baseline_path: PathBuf,
    current: (u64, u64, u64, SystemTime),
    previous: (u64, u64, u64, SystemTime),
}
#[cfg(test)]
fn comparison_generation(key: &str) -> Option<ComparisonGeneration> {
    comparison_generation_at(cache_file(key)?, previous_cache_file(key)?)
}
fn comparison_generation_at(
    current_path: PathBuf,
    baseline_path: PathBuf,
) -> Option<ComparisonGeneration> {
    let stamp = |path: PathBuf| {
        let metadata = fs::symlink_metadata(path).ok()?;
        #[cfg(unix)]
        let identity = (metadata.dev(), metadata.ino(), UNIX_EPOCH);
        #[cfg(not(unix))]
        let identity = (0, 0, metadata.modified().ok()?);
        // Access recency changes mtime. Atomic archive replacement changes inode
        // identity, so reading another scope must not invalidate existing scopes.
        metadata.file_type().is_file().then_some((
            identity.0,
            identity.1,
            metadata.len(),
            identity.2,
        ))
    };
    Some(ComparisonGeneration {
        current: stamp(current_path.clone())?,
        previous: stamp(baseline_path.clone())?,
        current_path,
        baseline_path,
    })
}
#[derive(Default)]
struct Registry {
    runs: HashMap<String, Arc<Run>>,
    active: Option<String>,
    focused: Option<String>,
    cached: HashMap<String, Cache>,
    comparisons: VecDeque<CachedComparison>,
}
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
static CACHE_STORAGE: OnceLock<CacheStorage> = OnceLock::new();
const CATALOG_RESERVE_BYTES: u64 = 512 * 1024;
struct CacheStorage(Mutex<()>);
struct CacheStorageGuard<'a> {
    _lock: Option<std::sync::MutexGuard<'a, ()>>,
    _process_lock: Option<fs::File>,
}
thread_local! { static CACHE_STORAGE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
impl CacheStorage {
    fn lock(&self) -> Result<CacheStorageGuard<'_>, String> {
        let nested = CACHE_STORAGE_DEPTH.with(|depth| depth.get() > 0);
        let lock = if nested {
            None
        } else {
            Some(
                self.0
                    .lock()
                    .map_err(|_| "Disk cache storage unavailable")?,
            )
        };
        let process_lock = if nested { None } else { cache_process_lock()? };
        CACHE_STORAGE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Ok(CacheStorageGuard {
            _lock: lock,
            _process_lock: process_lock,
        })
    }
}
impl Drop for CacheStorageGuard<'_> {
    fn drop(&mut self) {
        CACHE_STORAGE_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}
fn cache_storage_lock() -> &'static CacheStorage {
    CACHE_STORAGE.get_or_init(|| CacheStorage(Mutex::new(())))
}
// Both the desktop and MCP must participate. Old desktop binaries only have
// the in-process mutex, so MCP uses its own namespace unless sharing is opted in.
static AGENT_CACHE: OnceLock<bool> = OnceLock::new();
pub fn configure_agent_cache(shared: bool) -> Result<(), String> {
    AGENT_CACHE
        .set(!shared)
        .map_err(|_| "Agent cache is already configured".into())
}
fn cache_process_lock() -> Result<Option<fs::File>, String> {
    let Some(directory) = cache_directory() else {
        return Ok(None);
    };
    cache_process_lock_at(&directory, false)
}
fn cache_process_lock_at(directory: &Path, read_only: bool) -> Result<Option<fs::File>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    if !read_only {
        fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .create(!read_only)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("storage.lock"))
    {
        Ok(file) => file,
        Err(error) if read_only && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err("Invalid cache lock file".into());
    }
    let mode = if read_only {
        libc::LOCK_SH
    } else {
        libc::LOCK_EX
    };
    let started = Instant::now();
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error.to_string());
        }
        if started.elapsed() >= Duration::from_secs(3) {
            return Err("Cache storage is busy in another process; retry later".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
// An audit lease covers traversal AND final publication. Storage locks alone
// cannot stop an older process from publishing after a newer scan has finished.
// Local broad/focused workers share one lease; each keeps it alive until done.
fn cache_audit_lease() -> Result<Option<Arc<fs::File>>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    static LEASES: OnceLock<Mutex<HashMap<PathBuf, std::sync::Weak<fs::File>>>> = OnceLock::new();
    let Some(directory) = cache_directory() else {
        return Ok(None);
    };
    let mut leases = LEASES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| "Audit lease unavailable")?;
    if let Some(file) = leases.get(&directory).and_then(std::sync::Weak::upgrade) {
        return Ok(Some(file));
    }
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("audit.lock"))
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err("Invalid audit lease file".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another process is auditing this cache namespace; use its completed baseline or retry later".into());
    }
    let file = Arc::new(file);
    leases.retain(|_, file| file.strong_count() > 0);
    leases.insert(directory, Arc::downgrade(&file));
    Ok(Some(file))
}
/// Hold on the cleanup worker across mutation so another cooperating sidecar
/// cannot publish an audit taken halfway through this operation.
pub fn agent_cleanup_audit_lease() -> Result<Option<Arc<fs::File>>, String> {
    cache_audit_lease()
}
fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn scope_note() -> String {
    "Capacity covers the filesystem; analysis covers only the listed roots. Allocated bytes are filesystem blocks, exclude skipped cloud placeholders and unreadable paths, and may differ from APFS physical usage because of clones, compression and snapshots. Directory rows are summaries; file rows are only the source scan's largest-file samples. App attribution includes only exact associated directory names and is partial.".into()
}
fn home() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "Cannot locate home directory".into())
}
fn default_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = vec![home()?];
    if Path::new("/Applications").is_dir() {
        roots.push(PathBuf::from("/Applications"));
    }
    Ok(roots)
}
pub fn disk_overview() -> Result<DiskOverview, String> {
    let home = home()?;
    // df is a constant-time filesystem statistics query, never a tree walk.
    let output = crate::processes::bounded_output(
        Command::new("/bin/df").args(["-kP", &string(&home)]),
        Duration::from_secs(3),
    )
    .map_err(|e| format!("Cannot read filesystem capacity: {e}"))?;
    if !output.status.success() {
        return Err("Filesystem capacity query failed".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text
        .lines()
        .last()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    if fields.len() < 4 {
        return Err("Unrecognized filesystem capacity response".into());
    }
    let number = |index: usize| -> Result<u64, String> {
        fields[index]
            .parse::<u64>()
            .map(|n| n.saturating_mul(1024))
            .map_err(|_| "Invalid filesystem capacity".into())
    };
    let total = number(1)?;
    let available = number(3)?;
    Ok(DiskOverview {
        capacity: DiskCapacity {
            path: string(&home),
            total_bytes: total,
            available_bytes: available,
            used_bytes: total.saturating_sub(available),
        },
        default_roots: default_roots()?
            .into_iter()
            .map(|p| DiskRoot {
                label: if p == home {
                    "Home".into()
                } else {
                    "Applications".into()
                },
                path: string(&p),
            })
            .collect(),
        scope_note: scope_note(),
    })
}
#[cfg(test)]
fn validated_roots(path: Option<String>) -> Result<Vec<PathBuf>, String> {
    validated_roots_with_options(path, false)
}

// Pure lexical policy: never test protected locations to discover their permission.
// Deliberately exclude whole Library/media trees in basic mode, since individual
// app containers and privacy-controlled resources vary between macOS releases.
fn protected_path_with_home(path: &Path, user_home: &Path) -> bool {
    let parts = |p: &Path| {
        p.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(v) => Some(v.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut parts = parts(path);
    if parts.starts_with(&["system".into(), "volumes".into(), "data".into()]) {
        parts.drain(..3);
    }
    let home_parts = parts_for_home(user_home);
    let starts = |prefix: &[String]| parts.starts_with(prefix);
    if ["library", "private", "volumes", "network", "system", "dev"]
        .iter()
        .any(|p| parts.first().is_some_and(|s| s == p))
    {
        return true;
    }
    if parts.first().is_some_and(|p| p == "users") && parts.len() >= 2 && !starts(&home_parts) {
        return true;
    }
    if starts(&home_parts) {
        if let Some(first) = parts.get(home_parts.len()) {
            if [
                "desktop",
                "documents",
                "downloads",
                "library",
                "pictures",
                "music",
                "movies",
                ".trash",
            ]
            .contains(&first.as_str())
            {
                return true;
            }
        }
    }
    parts
        .iter()
        .any(|p| p.ends_with(".photoslibrary") || p.ends_with(".photolibrary"))
}
fn parts_for_home(path: &Path) -> Vec<String> {
    let mut parts = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(v) => Some(v.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if parts.starts_with(&["system".into(), "volumes".into(), "data".into()]) {
        parts.drain(..3);
    }
    parts
}
fn protected_path(path: &Path) -> bool {
    home()
        .map(|h| protected_path_with_home(path, &h))
        .unwrap_or(true)
}
fn scope_excluded(run: &Run, path: &Path) -> bool {
    !run.allow_protected && protected_path(path)
}
const PERMISSION_MESSAGE: &str = "Skipped privacy-protected scope. Enable Full Disk Access in System Settings, then explicitly choose a full scan to include it.";
fn validated_roots_with_options(
    path: Option<String>,
    allow_protected: bool,
) -> Result<Vec<PathBuf>, String> {
    let Some(path) = path else {
        return default_roots();
    };
    let input = PathBuf::from(path);
    if !input.is_absolute() {
        return Err("Scan path must be absolute".into());
    }
    if input
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Scan paths must not contain parent traversal".into());
    }
    // This must precede even symlink_metadata/canonicalize: preflight must not
    // cause the very Documents/Desktop/Downloads prompt it is avoiding.
    if !allow_protected && protected_path(&input) {
        return Err(format!("permission-required: {PERMISSION_MESSAGE}"));
    }
    if excluded(&input) {
        return Err("cloud-excluded: cloud storage is excluded".into());
    }
    // Reject a symlink at any component; canonicalization alone would silently traverse it.
    let mut prefix = PathBuf::new();
    for component in input.components() {
        prefix.push(component);
        let metadata =
            fs::symlink_metadata(&prefix).map_err(|e| format!("Cannot inspect scan path: {e}"))?;
        if metadata.file_type().is_symlink() {
            return Err("Scan paths must not traverse symbolic links".into());
        }
        if placeholder(&metadata) {
            return Err("cloud-excluded: dataless placeholders are excluded".into());
        }
    }
    let canonical =
        fs::canonicalize(&input).map_err(|e| format!("Cannot resolve scan path: {e}"))?;
    let mut allowed = default_roots()?;
    // Scope membership is lexical; canonicalizing the requested input already
    // checks its existence, so do not probe unrelated filesystem roots here.
    allowed.extend([
        PathBuf::from("/Library"),
        PathBuf::from("/System/Volumes/Data"),
    ]);
    if !allowed.iter().any(|root| canonical.starts_with(root)) {
        return Err(
            "Scan path must be inside Home, /Applications, /Library or /System/Volumes/Data".into(),
        );
    }
    if !canonical.is_dir() {
        return Err("Scan path must be a directory".into());
    }
    Ok(vec![canonical])
}
// Cache and snapshot lookups must not touch the selected filesystem path.
fn lexical_roots(path: Option<String>, allow_protected: bool) -> Result<Vec<PathBuf>, String> {
    let Some(path) = path else {
        return default_roots();
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err("Scan path must be absolute".into());
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Scan paths must not contain parent traversal".into());
    }
    let path: PathBuf = path.components().collect();
    if !allow_protected && protected_path(&path) {
        return Err(format!("permission-required: {PERMISSION_MESSAGE}"));
    }
    if ![
        home()?,
        PathBuf::from("/Applications"),
        PathBuf::from("/Library"),
        PathBuf::from("/System/Volumes/Data"),
    ]
    .iter()
    .any(|r| path.starts_with(r))
    {
        return Err(
            "Scan path must be inside Home, /Applications, /Library or /System/Volumes/Data".into(),
        );
    }
    Ok(vec![path])
}
fn cache_key(roots: &[PathBuf], allow_protected: bool) -> String {
    let roots = roots
        .iter()
        .map(|p| string(p))
        .collect::<Vec<_>>()
        .join("\n");
    format!("protected={allow_protected}\n{roots}")
}
#[cfg(test)]
fn test_cache_root() -> Option<PathBuf> {
    Some(
        fs::canonicalize(std::env::temp_dir())
            .ok()?
            .join(format!("macdisk-cache-tests-{}", std::process::id())),
    )
}
fn cache_directory() -> Option<PathBuf> {
    cache_directory_for(AGENT_CACHE.get() == Some(&true))
}
fn cache_directory_for(agent: bool) -> Option<PathBuf> {
    #[cfg(test)]
    let root = test_cache_root();
    #[cfg(all(not(test), not(feature = "benchmark")))]
    let root = debug_cache_root().or_else(dirs::cache_dir);
    #[cfg(all(not(test), feature = "benchmark"))]
    let root = debug_cache_root();
    root.map(|p| {
        p.join(CACHE_APP_DIRECTORY).join(if agent {
            "agent-disk-scans"
        } else {
            "disk-scans"
        })
    })
}
fn cache_limit_path() -> Option<PathBuf> {
    #[cfg(test)]
    let root = test_cache_root();
    #[cfg(all(not(test), not(feature = "benchmark")))]
    let root = debug_cache_root().or_else(dirs::config_dir);
    #[cfg(all(not(test), feature = "benchmark"))]
    let root = debug_cache_root();
    root.map(|p| {
        p.join(CACHE_APP_DIRECTORY)
            .join(if AGENT_CACHE.get() == Some(&true) {
                "agent-disk-cache-limit"
            } else {
                "disk-cache-limit"
            })
    })
}
#[cfg(not(test))]
fn debug_cache_root() -> Option<PathBuf> {
    if cfg!(any(debug_assertions, feature = "benchmark")) {
        return std::env::var_os("MSM_DISK_CACHE_ROOT").map(PathBuf::from);
    }
    None
}
fn cache_file(key: &str) -> Option<PathBuf> {
    // Stable FNV key; the content also validates roots/version before accepting a hit.
    let hash = key.bytes().fold(14695981039346656037u64, |h, b| {
        (h ^ b as u64).wrapping_mul(1099511628211)
    });
    cache_directory().map(|directory| directory.join(format!("{hash:x}.json")))
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCacheSettings {
    #[serde(rename = "limitMiB")]
    pub limit_mib: u64,
    pub used_bytes: u64,
}
fn supported_cache_limit(limit_mib: u64) -> bool {
    matches!(limit_mib, 64 | 256 | 512)
}
fn read_cache_limit(path: &Path) -> u64 {
    use std::io::Read;
    let read = || -> Option<u64> {
        let mut value = String::new();
        open_cache_file(path)?
            .take(33)
            .read_to_string(&mut value)
            .ok()?;
        if value.len() > 32 {
            return None;
        }
        let limit = value.trim().parse().ok()?;
        supported_cache_limit(limit).then_some(limit)
    };
    read().unwrap_or(DEFAULT_CACHE_LIMIT_MIB)
}
fn configured_cache_limit() -> u64 {
    cache_limit_path()
        .as_deref()
        .map(read_cache_limit)
        .unwrap_or(DEFAULT_CACHE_LIMIT_MIB)
}
fn save_cache_limit(path: &Path, limit_mib: u64) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Missing cache settings directory"))?;
    fs::create_dir_all(parent)?;
    static NEXT_SETTING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_SETTING.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}-{id}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        writeln!(file, "{limit_mib}")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
pub fn get_disk_cache_settings() -> Result<DiskCacheSettings, String> {
    let _storage = cache_storage_lock().lock()?;
    let directory = cache_directory().ok_or("Cannot locate disk scan cache directory")?;
    let used_bytes = cache_allocated_bytes(
        &cache_files(&directory, false)
            .map_err(|e| format!("Cannot measure disk scan cache: {e}"))?,
    );
    Ok(DiskCacheSettings {
        limit_mib: configured_cache_limit(),
        used_bytes,
    })
}
pub fn set_disk_cache_limit(limit_mib: u64) -> Result<DiskCacheSettings, String> {
    if !supported_cache_limit(limit_mib) {
        return Err("Disk cache limit must be 64, 256 or 512 MiB".into());
    }
    let _guard = cache_storage_lock()
        .lock()
        .map_err(|_| "Disk cache storage unavailable")?;
    let directory = cache_directory().ok_or("Cannot locate disk scan cache directory")?;
    let settings = cache_limit_path().ok_or("Cannot locate disk cache settings directory")?;
    set_disk_cache_limit_at(&directory, &settings, limit_mib)
}
fn set_disk_cache_limit_at(
    directory: &Path,
    settings: &Path,
    limit_mib: u64,
) -> Result<DiskCacheSettings, String> {
    let used_bytes = trim_cache_directory(
        directory,
        None,
        limit_mib * MIB - CATALOG_RESERVE_BYTES,
        usize::MAX,
    )
    .map_err(|e| format!("Cannot reduce disk scan cache: {e}"))?;
    save_cache_limit(settings, limit_mib)
        .map_err(|e| format!("Cannot save disk cache limit: {e}"))?;
    Ok(DiskCacheSettings {
        limit_mib,
        used_bytes,
    })
}
#[derive(Clone, Serialize, Deserialize)]
struct Cache {
    version: u32,
    key: String,
    snapshot: DiskScan,
    #[serde(default, with = "compact_index")]
    index: DirectoryIndex,
    #[serde(default)]
    index_complete: bool,
    #[serde(default)]
    file_details: FileDetails,
}
// Persist only the source path and counters; display labels and parent paths are derived.
mod compact_index {
    use super::*;
    type Row = (String, u64, u64, u64, u64, bool);
    pub fn row<'a>(path: &'a str, row: &DiskEntry) -> (&'a str, u64, u64, u64, u64, bool) {
        (
            path,
            row.allocated_bytes,
            row.logical_bytes,
            row.file_count,
            row.directory_count,
            row.complete,
        )
    }
    pub fn serialize<S: serde::Serializer>(
        index: &DirectoryIndex,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut rows = serializer.serialize_seq(Some(index.len()))?;
        for (path, entry) in index {
            rows.serialize_element(&row(path, entry))?;
        }
        rows.end()
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DirectoryIndex, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Rows {
            Compact(Vec<Row>),
            Legacy(DirectoryIndex),
        }
        match Rows::deserialize(deserializer)? {
            Rows::Legacy(index) => Ok(index),
            Rows::Compact(rows) => Ok(rows
                .into_iter()
                .map(|(path, bytes, logical, files, dirs, complete)| {
                    let mut row = new_entry(Path::new(&path), true);
                    row.allocated_bytes = bytes;
                    row.logical_bytes = logical;
                    row.file_count = files;
                    row.directory_count = dirs;
                    row.complete = complete;
                    (path, row)
                })
                .collect()),
        }
    }
}
fn previous_cache_file(key: &str) -> Option<PathBuf> {
    cache_file(key).map(|p| p.with_extension("previous.json"))
}
fn checkpoint_file(key: &str) -> Option<PathBuf> {
    cache_file(key).map(|p| p.with_extension("partial.json"))
}
fn open_cache_file(path: &Path) -> Option<fs::File> {
    let original = fs::symlink_metadata(path).ok()?;
    if !original.file_type().is_file() || original.len() > MAX_CACHE_FILE_BYTES {
        return None;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "macos")]
        options.custom_flags(0x20000000 | 0x00000004); // O_NOFOLLOW_ANY | O_NONBLOCK
        #[cfg(target_os = "linux")]
        options.custom_flags(0x00020000 | 0x00000800); // O_NOFOLLOW | O_NONBLOCK
    }
    let file = options.open(path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() {
        return None;
    }
    #[cfg(unix)]
    if original.dev() != opened.dev() || original.ino() != opened.ino() {
        return None;
    }
    Some(file)
}
fn read_cache_bytes(path: &Path) -> Option<Vec<u8>> {
    // A reader must not pin unlinked cache allocation while an evicting writer
    // reserves those supposedly freed blocks for a successor publication.
    let _storage = cache_storage_lock().lock().ok()?;
    read_locked_cache_bytes(path)
}
// The caller holds the storage lock for the namespace containing this file.
fn read_locked_cache_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = open_cache_file(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_CACHE_FILE_BYTES).then_some(bytes)
}
fn read_cache_at(path: &Path, key: &str, partial: bool) -> Option<Cache> {
    let data = read_cache_bytes(path)?;
    let mut value = decode_cache(&data, key, partial)?;
    // Older snapshots included these paths in application attribution.
    if let Ok(home) = home() {
        let caches = home.join("Library/Caches");
        for app in &mut value.snapshot.applications {
            app.associated_paths
                .retain(|path| !Path::new(path).starts_with(&caches));
        }
    }
    touch_cache(path);
    Some(value)
}
fn decode_cache(data: &[u8], key: &str, partial: bool) -> Option<Cache> {
    let mut value: Cache = serde_json::from_slice(data).ok()?;
    let expected_key = cache_key(
        &value
            .snapshot
            .roots
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>(),
        key.starts_with("protected=true\n"),
    );
    if value.version != CACHE_VERSION
        || value.key != key
        || expected_key != key
        || (!partial
            && (value.snapshot.status != "complete"
                || matches!(value.snapshot.freshness.as_str(), "partial" | "updating")))
    {
        return None;
    }
    if partial {
        value.snapshot.status = "cancelled".into();
        value.snapshot.coverage_complete = false;
        value.index_complete = false;
    }
    Some(value)
}
fn read_cache(key: &str) -> Option<Cache> {
    for (path, partial) in [(cache_file(key)?, false), (checkpoint_file(key)?, true)] {
        if let Some(value) = read_cache_at(&path, key, partial) {
            if let Some(directory) = path.parent() {
                let _ = trim_configured_cache(directory, Some(&path));
            }
            return Some(value);
        }
    }
    None
}
fn bounded_cache_bytes(value: &Cache, limit: u64) -> Option<Vec<u8>> {
    struct BoundedBuffer {
        bytes: Vec<u8>,
        limit: u64,
    }
    impl std::io::Write for BoundedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() as u64 > self.limit.saturating_sub(self.bytes.len() as u64) {
                return Err(std::io::Error::other(
                    "Disk snapshot exceeds its cache budget",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = BoundedBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value).ok()?;
    Some(buffer.bytes)
}
fn cache_bytes(value: &mut Cache, limit: u64) -> Option<Vec<u8>> {
    if let Some(data) = bounded_cache_bytes(value, limit) {
        return Some(data);
    }
    // File detail is optional. Under shared publication pressure preserve the
    // directory index first; omitted files remain in reliable residual totals.
    value.file_details = FileDetails::default();
    if let Some(data) = bounded_cache_bytes(value, limit) {
        return Some(data);
    }
    // Keep a useful prefix of directory depth levels rather than discarding the
    // entire index if very long names or unusual paths exhaust the cache file.
    let index = std::mem::take(&mut value.index);
    value.index_complete = false;
    value.snapshot.view_detail_complete = false;
    let mut used = bounded_cache_bytes(value, limit)?.len() as u64;
    let roots: Vec<_> = value.snapshot.roots.iter().map(Path::new).collect();
    let mut rows: Vec<_> = index
        .into_iter()
        .map(|(path, row)| {
            let depth = roots
                .iter()
                .filter_map(|root| Path::new(&path).strip_prefix(root).ok())
                .map(|relative| relative.components().count())
                .min()
                .unwrap_or(usize::MAX);
            (depth, std::cmp::Reverse(row.allocated_bytes), path, row)
        })
        .collect();
    rows.sort_unstable_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    for (_, _, path, row) in rows {
        let bytes = serde_json::to_vec(&compact_index::row(&path, &row))
            .ok()?
            .len() as u64;
        let additional = bytes + u64::from(!value.index.is_empty());
        if additional <= limit.saturating_sub(used) {
            used += additional;
            value.index.insert(path, row);
        }
    }
    bounded_cache_bytes(value, limit)
}
struct CacheFile {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    identity: Option<(u64, u64)>,
}
fn cache_files(directory: &Path, clean_stale_temporary: bool) -> std::io::Result<Vec<CacheFile>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let is_snapshot = path.extension().is_some_and(|e| e == "json")
            || path
                .file_name()
                .is_some_and(|name| name == "history.catalog");
        let is_index = path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.ends_with(".index.sqlite") || name.contains(".index.sqlite-")
        });
        let is_temporary = path.extension().is_some_and(|e| e == "tmp");
        if !(is_snapshot || is_index || is_temporary) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if is_temporary
            && clean_stale_temporary
            && modified
                .elapsed()
                .is_ok_and(|age| age >= Duration::from_secs(60))
        {
            let _ = fs::remove_file(path);
            continue;
        }
        files.push(CacheFile {
            path,
            bytes: allocated(&metadata),
            modified,
            #[cfg(unix)]
            identity: Some((metadata.dev(), metadata.ino())),
            #[cfg(not(unix))]
            identity: None,
        });
    }
    Ok(files)
}
fn cache_allocated_bytes(files: &[CacheFile]) -> u64 {
    let mut seen = HashSet::new();
    files
        .iter()
        .filter(|file| file.identity.is_none_or(|identity| seen.insert(identity)))
        .map(|file| file.bytes)
        .sum()
}
// Full snapshots, retained history, checkpoints and the small catalog share one
// physical allocation budget. Shared immutable hard links count once, including
// during rotation; deleting one link never claims its still-linked bytes freed.
fn trim_cache_directory(
    directory: &Path,
    keep: Option<&Path>,
    byte_limit: u64,
    file_limit: usize,
) -> std::io::Result<u64> {
    trim_cache_directory_with_policy(directory, keep, byte_limit, file_limit, true, &[])
}
fn trim_cache_directory_with_policy(
    directory: &Path,
    keep: Option<&Path>,
    byte_limit: u64,
    file_limit: usize,
    evict_snapshots: bool,
    protected_baselines: &[PathBuf],
) -> std::io::Result<u64> {
    let priorities = if evict_snapshots {
        history::priorities(directory)
    } else {
        HashMap::new()
    };
    let mut files = cache_files(directory, true)?;
    let mut bytes = cache_allocated_bytes(&files);
    let snapshot_count = |files: &[CacheFile]| {
        files
            .iter()
            .filter(|file| {
                file.path
                    .file_name()
                    .is_none_or(|name| name != "history.catalog")
            })
            .count()
    };
    let mut count = snapshot_count(&files);
    files.sort_by_key(|file| {
        (
            if file.path.to_string_lossy().ends_with(".partial.json") {
                0
            } else if file.path.to_string_lossy().contains(".index.sqlite") {
                // Measured history cannot be rebuilt from the current filesystem.
                5
            } else {
                priorities.get(&file.path).copied().unwrap_or(1)
            },
            file.modified,
        )
    });
    let candidates: Vec<_> = files.iter().map(|file| file.path.clone()).collect();
    for path in candidates {
        if bytes <= byte_limit && count <= file_limit {
            break;
        }
        if keep.is_some_and(|keep| {
            path == keep
                || (keep.to_string_lossy().ends_with(".index.sqlite")
                    && path
                        .to_string_lossy()
                        .starts_with(&format!("{}-", keep.display())))
        }) || protected_baselines.contains(&path)
            || path
                .file_name()
                .is_some_and(|name| name == "history.catalog")
            || (!evict_snapshots
                && path.extension().is_some_and(|ext| ext == "json")
                && !path.to_string_lossy().ends_with(".partial.json"))
        {
            continue;
        }
        // SQLite and its recovery files are one rebuildable eviction unit.
        // Never leave a hot journal attached to a later, unrelated new database.
        let name = path.to_string_lossy();
        let group = name
            .find(".index.sqlite")
            .map(|position| &name[..position + ".index.sqlite".len()]);
        let evictions: Vec<_> = files
            .iter()
            .filter(|file| {
                group.is_some_and(|base| {
                    file.path.to_string_lossy() == base
                        || file.path.to_string_lossy().starts_with(&format!("{base}-"))
                }) || file.path == path
            })
            .map(|file| file.path.clone())
            .collect();
        for eviction in evictions {
            match fs::remove_file(&eviction) {
                Ok(()) => files.retain(|file| file.path != eviction),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    files.retain(|file| file.path != eviction)
                }
                Err(_) => {}
            }
        }
        bytes = cache_allocated_bytes(&files);
        count = snapshot_count(&files);
    }
    if bytes > byte_limit || count > file_limit {
        return Err(std::io::Error::other(
            "Some cached snapshots could not be evicted",
        ));
    }
    Ok(bytes)
}
// Caller holds CACHE_STORAGE. The envelope includes the existing database and
// its journal; reserve the rest before SQLite is allowed to allocate any pages.
fn reserve_incremental_bytes(db_path: &Path, envelope_bytes: u64) -> Result<(), String> {
    let directory = db_path.parent().ok_or("Missing index directory")?;
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let files = cache_files(directory, false).map_err(|e| e.to_string())?;
    let prefix = format!("{}-", db_path.display());
    let own: Vec<_> = files
        .into_iter()
        .filter(|file| file.path == db_path || file.path.to_string_lossy().starts_with(&prefix))
        .collect();
    let additional = envelope_bytes.saturating_sub(cache_allocated_bytes(&own));
    let limit = configured_cache_limit() * MIB - CATALOG_RESERVE_BYTES;
    let target = limit
        .checked_sub(additional)
        .ok_or("Incremental store exceeds cache budget")?;
    // Decline optional SQLite storage instead of evicting an endpoint or baseline.
    trim_cache_directory_with_policy(directory, Some(db_path), target, usize::MAX, false, &[])
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn trim_configured_cache(directory: &Path, keep: Option<&Path>) -> std::io::Result<u64> {
    let _storage = cache_storage_lock().lock().map_err(std::io::Error::other)?;
    trim_cache_directory(
        directory,
        keep,
        configured_cache_limit() * MIB - CATALOG_RESERVE_BYTES,
        usize::MAX,
    )
}
fn touch_cache(path: &Path) {
    let Ok(_storage) = cache_storage_lock().lock() else {
        return;
    };
    if let Some(file) = open_cache_file(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}
// The generation is checked at rename, after serialization and the temporary write.
// A stale writer can never replace the last good snapshot or checkpoint.
fn write_cache(key: &str, run: &Run, partial: bool) -> bool {
    let Ok(mut snapshot) = run.snapshot.lock().map(|s| s.clone()) else {
        return false;
    };
    if !partial && (!reusable_snapshot(&snapshot) || run.cancelled.load(Ordering::Relaxed)) {
        return false;
    }
    let scan_id = snapshot.scan_id.clone();
    // Display partitions can be reconstructed from this source and its index.
    snapshot.treemap_entries.clear();
    snapshot.history_save_failed = false;
    if partial {
        snapshot.status = "cancelled".into();
        snapshot.coverage_complete = false;
    }
    let mut value = Cache {
        version: CACHE_VERSION,
        key: key.into(),
        snapshot,
        // Checkpoints stay compact; only finished snapshots publish the directory index.
        index: if partial {
            HashMap::new()
        } else {
            run.index.lock().map(|i| i.clone()).unwrap_or_default()
        },
        index_complete: !partial && run.index_complete.load(Ordering::Relaxed),
        file_details: if partial {
            FileDetails::default()
        } else {
            run.file_details
                .lock()
                .map(|details| details.clone())
                .unwrap_or_default()
        },
    };
    let Some(path) = (if partial {
        checkpoint_file(key)
    } else {
        cache_file(key)
    }) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    static NEXT_WRITE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let write_id = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("{scan_id}-{write_id}.tmp"));
    // Admission precedes the temporary write, including conservative allocation
    // headroom and the independent small history-catalog publication reserve.
    let Ok(mut reg) = registry().lock() else {
        return false;
    };
    if ![reg.active.as_deref(), reg.focused.as_deref()].contains(&Some(scan_id.as_str())) {
        return false;
    }
    let Ok(_storage) = cache_storage_lock().lock() else {
        return false;
    };
    let protected_baselines = if partial {
        Vec::new()
    } else {
        history::publication_baselines(key, value.snapshot.index_policy_version)
    };
    let Ok(files) = cache_files(parent, false) else {
        return false;
    };
    let protected_files: Vec<_> = files
        .into_iter()
        .filter(|file| file.path == path || protected_baselines.contains(&file.path))
        .collect();
    let budget = configured_cache_limit() * MIB - CATALOG_RESERVE_BYTES;
    // Compact the successor if necessary. Keep the old endpoint and either
    // comparison baseline readable even if a later write or rename fails.
    let publication_allowance =
        budget.saturating_sub(cache_allocated_bytes(&protected_files) + 128 * 1024) / 2;
    let Some(data) = cache_bytes(&mut value, publication_allowance.min(MAX_CACHE_FILE_BYTES))
    else {
        return false;
    };
    let reservation = (data.len() as u64)
        .saturating_mul(2)
        .saturating_add(128 * 1024);
    let Some(available) = budget.checked_sub(reservation) else {
        return false;
    };
    if trim_cache_directory_with_policy(
        parent,
        Some(&path),
        available,
        usize::MAX,
        !partial,
        &protected_baselines,
    )
    .is_err()
    {
        return false;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(&temp) else {
        return false;
    };
    use std::io::Write;
    let newer_checkpoint_exists = partial
        && read_cache_bytes(&path)
            .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
            .is_some_and(|existing| {
                existing.snapshot.scan_id == scan_id
                    && (
                        existing.snapshot.updated_at_ms,
                        existing.snapshot.scanned_files,
                        existing.snapshot.scanned_directories,
                    ) > (
                        value.snapshot.updated_at_ms,
                        value.snapshot.scanned_files,
                        value.snapshot.scanned_directories,
                    )
            });
    // Keep the last complete generation before publishing the replacement. The
    // registry lock keeps the writer current, and readers only see whole files.
    let published = !newer_checkpoint_exists
        && file.write_all(&data).is_ok()
        && file.sync_all().is_ok()
        && (partial || archive_previous_cache(key, &path, &scan_id, write_id))
        && fs::rename(&temp, &path).is_ok();
    if !published {
        let _ = fs::remove_file(temp);
        return false;
    }
    if !partial {
        // Avoid keeping a second full index copy for every past scan.
        reg.cached.clear();
        if !reg.runs.contains_key(&scan_id) {
            reg.cached.insert(key.into(), value);
        }
        if let Some(checkpoint) = checkpoint_file(key) {
            let _ = fs::remove_file(checkpoint);
        }
    }
    true
}
fn archive_previous_cache(key: &str, path: &Path, scan_id: &str, write_id: u64) -> bool {
    let Some(existing) = read_cache_at(path, key, false) else {
        return true;
    };
    if existing.snapshot.scan_id == scan_id {
        return true;
    }
    let Some(previous) = previous_cache_file(key) else {
        return false;
    };
    if !history::archive_previous(key, write_id) {
        return false;
    }
    let temporary = previous.with_extension(format!("{scan_id}-{write_id}.tmp"));
    // Hard links retain the already-synced immutable file without serializing or
    // copying a second full index. Rename keeps baseline readers on a whole file.
    let archived = fs::hard_link(path, &temporary)
        .and_then(|()| fs::rename(&temporary, &previous))
        .is_ok();
    let _ = fs::remove_file(temporary);
    archived
}

fn run_from_cache(mut cached: Cache, allow_protected: bool) -> Arc<Run> {
    cached.snapshot.cache_hit = true;
    cached.snapshot.scan_method = "cached".into();
    cached.snapshot.freshness = "cached".into();
    cached.snapshot.cache_age_ms = now_ms().saturating_sub(cached.snapshot.updated_at_ms);
    if !cached.snapshot.application_inventory_complete {
        restore_cached_applications(&mut cached.snapshot, &cached.index);
    }
    cached.snapshot.treemap_entries =
        treemap_projection(&cached.snapshot, &cached.index, &cached.snapshot.entries);
    Arc::new(Run {
        allow_protected,
        snapshot: Mutex::new(cached.snapshot),
        cancelled: AtomicBool::new(false),
        measurement_gap: AtomicBool::new(false),
        index_budget: Mutex::new(IndexBudget::default()),
        index_limited: AtomicBool::new(!cached.index_complete),
        shallow_limited: AtomicBool::new(false),
        finalization: Mutex::new(()),
        finalized: AtomicBool::new(true),
        managed: false,
        recorder: Mutex::new(None),
        file_details: Mutex::new(cached.file_details),
        file_details_full: AtomicBool::new(true),
        file_detail_limit_mib: configured_cache_limit(),
        index: Mutex::new(cached.index),
        index_complete: AtomicBool::new(cached.index_complete),
    })
}

fn restore_cached_applications(snapshot: &mut DiskScan, index: &DirectoryIndex) {
    let user_applications = home().ok().map(|home| home.join("Applications"));
    let is_installed =
        |path: &str| installed_application(Path::new(path), user_applications.as_deref());
    let mut remaining = MAX_APPLICATIONS.saturating_sub(
        snapshot
            .applications
            .iter()
            .filter(|app| is_installed(&app.path))
            .count(),
    );
    let mut seen: HashSet<_> = snapshot
        .applications
        .iter()
        .map(|app| app.path.clone())
        .collect();
    // Legacy snapshots used a single application cap. Recover any measured
    // installed rows that were displaced, without reading the live filesystem.
    for row in snapshot.entries.iter().chain(index.values()) {
        if remaining == 0 {
            break;
        }
        if row.kind == "application"
            && is_installed(&row.path)
            && primary_application(Path::new(&row.path))
            && seen.insert(row.path.clone())
        {
            snapshot.applications.push(DiskApplication::from(row));
            remaining -= 1;
        }
    }
}
fn clear_active(scan_id: &str) {
    if let Ok(mut reg) = registry().lock() {
        if reg.active.as_deref() == Some(scan_id) {
            reg.active = None;
        }
        if reg.focused.as_deref() == Some(scan_id) {
            reg.focused = None;
        }
    }
}
fn finish_run(key: &str, run: &Run) {
    let Ok(_publication) = run.finalization.lock() else {
        return;
    };
    if run.finalized.load(Ordering::Relaxed) {
        return;
    }
    let (scan_id, complete) = run
        .snapshot
        .lock()
        .map(|s| {
            (
                s.scan_id.clone(),
                reusable_snapshot(&s) && !run.cancelled.load(Ordering::Relaxed),
            )
        })
        .unwrap_or_default();
    let published = write_cache(key, run, !complete);
    if complete && !published {
        if let Ok(mut snapshot) = run.snapshot.lock() {
            snapshot.history_save_failed = true;
        }
    }
    run.finalized.store(true, Ordering::Release);
    clear_active(&scan_id);
}
pub fn start_disk_scan(path: Option<String>, refresh: bool) -> Result<DiskScan, String> {
    start_disk_scan_with_options(path, refresh, false)
}
pub fn start_disk_scan_with_options(
    path: Option<String>,
    refresh: bool,
    allow_protected: bool,
) -> Result<DiskScan, String> {
    start_disk_scan_impl(path, refresh, allow_protected, None)
}
pub fn start_disk_scan_with_mode(
    path: Option<String>,
    refresh: bool,
    allow_protected: bool,
    mode: Option<String>,
) -> Result<DiskScan, String> {
    match mode.as_deref().unwrap_or("auto") {
        "auto" => start_disk_scan_impl(path, refresh, allow_protected, None),
        "full" => start_disk_scan_mode_impl(path, true, allow_protected, None, true),
        _ => Err("Disk scan mode must be auto or full".into()),
    }
}
fn start_disk_scan_impl(
    path: Option<String>,
    refresh: bool,
    allow_protected: bool,
    inspect_source_id: Option<&str>,
) -> Result<DiskScan, String> {
    start_disk_scan_mode_impl(path, refresh, allow_protected, inspect_source_id, false)
}
fn start_disk_scan_mode_impl(
    path: Option<String>,
    refresh: bool,
    allow_protected: bool,
    inspect_source_id: Option<&str>,
    force_full: bool,
) -> Result<DiskScan, String> {
    loop {
        let roots = lexical_roots(path.clone(), allow_protected)?;
        let key = cache_key(&roots, allow_protected);
        let mut registry = registry()
            .lock()
            .map_err(|_| "Disk scan registry unavailable")?;
        // Observe the active status once. A worker may finish immediately after
        // this snapshot, but a running response remains safe to poll. Reading its
        // status twice could otherwise fall through to an older last-good cache.
        let mut finished = None;
        for id in [registry.active.as_ref(), registry.focused.as_ref()]
            .into_iter()
            .flatten()
        {
            let Some(run) = registry.runs.get(id).cloned() else {
                continue;
            };

            let snapshot = run
                .snapshot
                .lock()
                .map_err(|_| "Disk scan unavailable")?
                .clone();
            if snapshot.status != "running" {
                let source_key = cache_key(
                    &snapshot.roots.iter().map(PathBuf::from).collect::<Vec<_>>(),
                    run.allow_protected,
                );
                finished = Some((source_key, run));
                break;
            }
            // A checkpoint shares its source ID. Preserve the actual worker.
            if (!refresh || inspect_source_id.is_some())
                && run.allow_protected == allow_protected
                && snapshot.roots == roots.iter().map(|p| string(p)).collect::<Vec<_>>()
                && !run.cancelled.load(Ordering::Relaxed)
            {
                return Ok(snapshot);
            }
        }
        if let Some((source_key, run)) = finished {
            drop(registry);
            finish_run(&source_key, &run);
            continue;
        }
        let inspect_refresh = refresh && inspect_source_id.is_some();
        let reusable_after_refresh = |snapshot: &DiskScan| {
            reusable_snapshot(snapshot) && snapshot.index_policy_version >= INDEX_POLICY_VERSION
        };
        if inspect_refresh {
            // A second request can have observed an old cache just before a
            // concurrent inspection finished upgrading it. Recheck the new
            // exact result under the same lock used to start a worker.
            let exact_roots: Vec<_> = roots.iter().map(|p| string(p)).collect();
            let ready = registry
                .runs
                .values()
                .filter(|run| run.allow_protected == allow_protected)
                .filter_map(|run| run.snapshot.lock().ok())
                .filter(|snapshot| {
                    snapshot.roots == exact_roots && reusable_after_refresh(snapshot)
                })
                .max_by_key(|snapshot| (snapshot.updated_at_ms, snapshot.started_at_ms));
            if let Some(snapshot) = ready {
                let mut snapshot = snapshot.clone();
                snapshot.cache_hit = true;
                snapshot.cache_age_ms = now_ms().saturating_sub(snapshot.updated_at_ms);
                return Ok(snapshot);
            }
        }
        if !refresh || inspect_refresh {
            let eligible = |cache: &Cache| !refresh || reusable_after_refresh(&cache.snapshot);
            if let Some(cached) = registry
                .cached
                .get(&key)
                .filter(|cache| eligible(cache))
                .cloned()
                .or_else(|| read_cache(&key).filter(eligible))
            {
                if let Some(path) = cache_file(&key) {
                    touch_cache(&path);
                    if let Some(directory) = path.parent() {
                        let _ = trim_configured_cache(directory, Some(&path));
                    }
                }
                let run = run_from_cache(cached, allow_protected);
                let snapshot = run
                    .snapshot
                    .lock()
                    .map_err(|_| "Disk scan unavailable")?
                    .clone();
                if registry.runs.len() >= MAX_RESIDENT_RUNS {
                    let active = registry.active.clone();
                    let focused = registry.focused.clone();
                    registry.runs.retain(|id, _| {
                        [active.as_ref(), focused.as_ref()].contains(&Some(id))
                            || Some(id.as_str()) == inspect_source_id
                    });
                }
                registry.runs.entry(snapshot.scan_id.clone()).or_insert(run);
                return Ok(snapshot);
            }
        }
        let source = inspect_source_id
            .and_then(|id| registry.runs.get(id))
            .cloned();
        let focused = registry.active.is_some()
            && registry.focused.is_none()
            && source.as_ref().is_some_and(|source| {
                let contains_target = |run: &Run| {
                    run.allow_protected == allow_protected
                        && run.snapshot.lock().is_ok_and(|snapshot| {
                            roots.len() == 1
                                && snapshot.roots.iter().any(|root| roots[0].starts_with(root))
                        })
                };
                contains_target(source)
                    && registry
                        .active
                        .as_ref()
                        .and_then(|id| registry.runs.get(id))
                        .is_some_and(|active| contains_target(active))
            });
        if (registry.active.is_some() || registry.focused.is_some()) && !focused {
            drop(registry);
            if let (Some(source), Some(target)) = (&source, roots.first()) {
                validate_inspection_target(source, target)?;
            }
            return Err("A disk scan is already running; cancel it and wait for cancellation before starting another".into());
        }
        let roots = validated_roots_with_options(path.clone(), allow_protected)?;
        if let (Some(source), Some(target)) = (&source, roots.first()) {
            validate_inspection_target(source, target)?;
        }
        let key = cache_key(&roots, allow_protected);
        let now = now_ms();
        static NEXT_SCAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT_SCAN.fetch_add(1, Ordering::Relaxed);
        let snapshot = DiskScan {
            scan_id: format!("disk-{}-{now}-{unique}", std::process::id()),
            status: "running".into(),
            roots: roots.iter().map(|p| string(p)).collect(),
            started_at_ms: now,
            updated_at_ms: now,
            elapsed_ms: 0,
            scanned_files: 0,
            scanned_directories: 0,
            allocated_bytes: 0,
            logical_bytes: 0,
            current_path: String::new(),
            entries: vec![],
            treemap_entries: vec![],
            applications: vec![],
            application_copies_truncated: false,
            application_inventory_complete: false,
            issues: vec![],
            issue_count: 0,
            cache_hit: false,
            cache_age_ms: 0,
            coverage_complete: false,
            scope_note: scope_note(),
            source_roots: None,
            view_path: None,
            view_detail_complete: false,
            index_policy_version: INDEX_POLICY_VERSION,
            source_scan_id: None,
            scan_method: "full".into(),
            freshness: "updating".into(),
            history_save_failed: false,
            fallback_reason: None,
        };
        let run = Arc::new(Run {
            allow_protected,
            snapshot: Mutex::new(snapshot.clone()),
            cancelled: AtomicBool::new(false),
            measurement_gap: AtomicBool::new(false),
            index: Mutex::new(HashMap::new()),
            file_details: Mutex::new(FileDetails::default()),
            file_details_full: AtomicBool::new(false),
            file_detail_limit_mib: configured_cache_limit(),
            index_complete: AtomicBool::new(false),
            index_budget: Mutex::new(IndexBudget::default()),
            index_limited: AtomicBool::new(false),
            shallow_limited: AtomicBool::new(false),
            finalization: Mutex::new(()),
            finalized: AtomicBool::new(false),
            managed: true,
            recorder: Mutex::new(None),
        });
        // Retain a bounded number of past scans; completed snapshots remain in the on-disk cache.
        if registry.runs.len() >= MAX_RESIDENT_RUNS {
            let active = registry.active.clone();
            let focused = registry.focused.clone();
            registry.runs.retain(|id, _| {
                [active.as_ref(), focused.as_ref()].contains(&Some(id))
                    || Some(id.as_str()) == inspect_source_id
            });
        }
        if focused {
            registry.focused = Some(snapshot.scan_id.clone());
        } else {
            registry.active = Some(snapshot.scan_id.clone());
        }
        registry.runs.insert(snapshot.scan_id.clone(), run.clone());
        drop(registry);
        let failed_run = run.clone();
        std::thread::Builder::new()
            .name("mac-disk-coordinator".into())
            .spawn(move || {
                let _audit_lease = match cache_audit_lease() {
                    Ok(lease) => lease,
                    Err(error) => {
                        if let Ok(mut snapshot) = run.snapshot.lock() {
                            snapshot.status = "failed".into();
                            snapshot.freshness = "partial".into();
                            snapshot.coverage_complete = false;
                            snapshot.fallback_reason = Some(error);
                        }
                        run.finalized.store(true, Ordering::Relaxed);
                        let scan_id = run.snapshot.lock().ok().map(|s| s.scan_id.clone());
                        if let Some(scan_id) = scan_id {
                            clear_active(&scan_id);
                        }
                        return;
                    }
                };
                let checkpoint_done = Arc::new(AtomicBool::new(false));
                std::thread::scope(|scope| {
                    let done = checkpoint_done.clone();
                    let checkpoint_run = run.clone();
                    let checkpoint_key = key.clone();
                    scope.spawn(move || {
                        let mut last = Instant::now();
                        while !done.load(Ordering::Relaxed) {
                            std::thread::park_timeout(Duration::from_millis(250));
                            if last.elapsed() >= CHECKPOINT_INTERVAL
                                && !done.load(Ordering::Relaxed)
                            {
                                write_cache(&checkpoint_key, &checkpoint_run, true);
                                last = Instant::now();
                            }
                        }
                    });
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        scan_with_incremental(
                            run.clone(),
                            roots,
                            if focused { 2 } else { WORKERS },
                            &key,
                            force_full,
                        )
                    }));
                    if result.is_err() {
                        if let Ok(mut s) = run.snapshot.lock() {
                            s.status = "failed".into();
                            s.freshness = "partial".into();
                            s.coverage_complete = false;
                            issue(
                                &mut s,
                                Path::new(""),
                                "scan-failed",
                                "Disk scan worker failed".into(),
                            );
                        }
                    }
                    checkpoint_done.store(true, Ordering::Relaxed);
                });
                finish_run(&key, &run);
            })
            .map_err(|e| {
                if let Ok(mut snapshot) = failed_run.snapshot.lock() {
                    snapshot.status = "failed".into();
                    snapshot.freshness = "partial".into();
                    snapshot.coverage_complete = false;
                }
                clear_active(&snapshot.scan_id);
                format!("Cannot start disk scan: {e}")
            })?;
        return Ok(snapshot);
    }
}
pub fn get_disk_scan(scan_id: String) -> Result<DiskScan, String> {
    let run = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .runs
        .get(&scan_id)
        .cloned()
        .ok_or("Unknown disk scan ID")?;
    let mut snapshot = run
        .snapshot
        .lock()
        .map_err(|_| "Disk scan unavailable")?
        .clone();
    if snapshot.status == "running" {
        snapshot.elapsed_ms = now_ms().saturating_sub(snapshot.started_at_ms);
    } else {
        if !run.finalized.load(Ordering::Acquire) {
            // Returning a terminal status tells Electron it can release the active scan.
            // Join publication, then read back the outcome of the attempted save.
            let key = cache_key(
                &snapshot.roots.iter().map(PathBuf::from).collect::<Vec<_>>(),
                run.allow_protected,
            );
            finish_run(&key, &run);
        }
        snapshot.history_save_failed = run
            .snapshot
            .lock()
            .map_err(|_| "Disk scan unavailable")?
            .history_save_failed;
    }
    if snapshot.cache_hit {
        snapshot.cache_age_ms = now_ms().saturating_sub(snapshot.updated_at_ms);
    }
    Ok(snapshot)
}
/// Compare archived measurements only. This endpoint never lists or stats the
/// selected filesystem path, so a permission change cannot turn unknown into zero.
pub fn get_disk_scan_comparison(
    scan_id: String,
    path: Option<String>,
) -> Result<DiskScanComparison, String> {
    get_disk_scan_comparison_with_baseline(scan_id, path, None)
}

pub fn list_disk_scan_history(scan_id: String) -> Result<history::Listing, String> {
    let snapshot = get_disk_scan(scan_id.clone())?;
    let run = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .runs
        .get(&scan_id)
        .cloned()
        .ok_or("Unknown disk scan ID")?;
    let key = cache_key(
        &snapshot.roots.iter().map(PathBuf::from).collect::<Vec<_>>(),
        run.allow_protected,
    );
    let rows = history::records(&key, snapshot.index_policy_version);
    Ok(history::Listing {
        scope_id: history::scope_id(&key, snapshot.index_policy_version),
        current: rows
            .iter()
            .find(|row| row.is_current())
            .map(|row| row.summary.clone()),
        snapshots: rows
            .into_iter()
            .filter(|row| !row.is_current())
            .map(|row| row.summary)
            .collect(),
    })
}

pub fn get_disk_scan_comparison_with_baseline(
    scan_id: String,
    path: Option<String>,
    baseline_id: Option<String>,
) -> Result<DiskScanComparison, String> {
    let snapshot = get_disk_scan(scan_id.clone())?;
    let run = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .runs
        .get(&scan_id)
        .cloned()
        .ok_or("Unknown disk scan ID")?;
    let scope = path
        .as_deref()
        .map(|path| source_directory_path(&run, path))
        .transpose()?;
    let comparison_scope = scope.as_ref().map(|path| string(path));
    let key = cache_key(
        &snapshot.roots.iter().map(PathBuf::from).collect::<Vec<_>>(),
        run.allow_protected,
    );
    let mut result = DiskScanComparison {
        status: if snapshot.status == "running" {
            "scanning"
        } else {
            "unavailable"
        }
        .into(),
        refreshing: false,
        baseline_id: baseline_id.clone(),
        current_id: None,
        baseline_started_at_ms: None,
        baseline_updated_at_ms: None,
        current_started_at_ms: snapshot.started_at_ms,
        current_updated_at_ms: snapshot.updated_at_ms,
        roots: snapshot.roots.clone(),
        entries: vec![],
        growth_bytes: 0,
        reduction_bytes: 0,
        net_bytes: 0,
        limited: false,
    };
    // The comparison is between archived endpoints. A failed or stopped new
    // measurement has no bearing on whether those endpoints are readable.
    let rows = history::records(&key, snapshot.index_policy_version);
    let Some(current_row) = rows.iter().find(|row| row.is_current()) else {
        if baseline_id.is_some() {
            result.status = "baseline-unavailable".into();
        }
        return Ok(result);
    };
    if snapshot.status == "running"
        && history::snapshot_id(&key, &snapshot) == current_row.summary.id
    {
        return Ok(result);
    }
    result.current_id = Some(current_row.summary.id.clone());
    result.current_started_at_ms = current_row.summary.started_at_ms;
    result.current_updated_at_ms = current_row.summary.updated_at_ms;
    let baseline_row = if let Some(id) = baseline_id.as_deref() {
        rows.iter()
            .filter(|row| !row.is_current())
            .find(|row| row.summary.id == id)
    } else {
        // Existing callers continue to compare the adjacent retained generation.
        rows.iter().find(|row| !row.is_current())
    };
    let Some(baseline_row) = baseline_row else {
        result.status = if baseline_id.is_some() {
            "baseline-unavailable"
        } else if snapshot.status == "running" {
            "scanning"
        } else {
            "no-baseline"
        }
        .into();
        return Ok(result);
    };
    result.baseline_id = Some(baseline_row.summary.id.clone());
    let Some(current_path) = current_row.path() else {
        return Ok(result);
    };
    let Some(baseline_path) = baseline_row.path() else {
        return Ok(result);
    };
    let generation = comparison_generation_at(current_path.clone(), baseline_path.clone());
    if baseline_id.is_some() {
        history::select(
            &key,
            snapshot.index_policy_version,
            &baseline_row.summary.id,
        );
        if let Some(directory) = cache_directory() {
            let _ = trim_configured_cache(&directory, Some(&current_path));
        }
    }
    let cached = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .comparisons
        .iter()
        .rev()
        .find(|entry| {
            entry.key == key
                && entry.scope == comparison_scope
                && entry.result.baseline_id == result.baseline_id
                && entry.result.current_id == result.current_id
        })
        .cloned();
    if let Some(cached) = cached {
        if cached.generation.is_some()
            && cached.generation == generation
            && generation.as_ref().is_some_and(|generation| {
                comparison_generation_at(
                    generation.current_path.clone(),
                    generation.baseline_path.clone(),
                )
                .as_ref()
                    == Some(generation)
            })
        {
            let mut ready = cached.result;
            ready.refreshing = snapshot.status == "running";
            return Ok(ready);
        }
    }
    let Some(baseline) = read_cache_at(&baseline_path, &key, false).filter(|cache| {
        cache.snapshot.index_policy_version == snapshot.index_policy_version
            && history::snapshot_id(&key, &cache.snapshot) == baseline_row.summary.id
    }) else {
        result.status = if baseline_id.is_some() {
            "baseline-unavailable"
        } else {
            "no-baseline"
        }
        .into();
        return Ok(result);
    };
    let Some(current) = read_cache_at(&current_path, &key, false).filter(|cache| {
        cache.snapshot.index_policy_version == snapshot.index_policy_version
            && history::snapshot_id(&key, &cache.snapshot) == current_row.summary.id
    }) else {
        return Ok(result);
    };
    // The endpoint is a full archived measurement. Intermediate positive deltas
    // are never summed; creation and later deletion cancel at the endpoints.
    compare_archives(&baseline, &current, scope.as_deref(), &mut result);
    if result.status == "ready" {
        result.refreshing = snapshot.status == "running";
    }
    retain_comparison(&scan_id, comparison_scope, &key, generation, &result);
    Ok(result)
}

// Completed pairs are immutable. Cache only their small, bounded output; never
// retain another copy of the potentially 100k-row archive index per scope.
fn retain_comparison(
    scan_id: &str,
    scope: Option<String>,
    key: &str,
    generation: Option<ComparisonGeneration>,
    result: &DiskScanComparison,
) {
    if result.status != "ready" {
        return;
    }
    let bytes = result
        .entries
        .iter()
        .map(|row| row.path.len() + row.name.len() + row.parent_path.len() + row.kind.len() + 256)
        .sum::<usize>()
        + result.roots.iter().map(String::len).sum::<usize>()
        + scan_id.len()
        + key.len()
        + scope.as_ref().map_or(0, String::len)
        + 1024;
    if bytes > MAX_COMPARISON_MEMORY_BYTES {
        return;
    }
    // A background writer may replace archives while they are decoded. Do not
    // stamp an old computed result with a newly published pair's identities.
    if generation.as_ref().is_some_and(|generation| {
        comparison_generation_at(
            generation.current_path.clone(),
            generation.baseline_path.clone(),
        )
        .as_ref()
            != Some(generation)
    }) {
        return;
    }
    if let Ok(mut reg) = registry().lock() {
        reg.comparisons.retain(|entry| {
            entry.scan_id != scan_id
                || entry.scope != scope
                || entry.result.baseline_id != result.baseline_id
                || entry.result.current_id != result.current_id
        });
        while reg.comparisons.len() >= MAX_COMPARISON_SCOPES
            || reg
                .comparisons
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                + bytes
                > MAX_COMPARISON_MEMORY_BYTES
        {
            reg.comparisons.pop_front();
        }
        reg.comparisons.push_back(CachedComparison {
            scan_id: scan_id.into(),
            scope,
            result: result.clone(),
            bytes,
            key: key.into(),
            generation,
        });
    }
}

fn comparison_directories(cache: &Cache) -> HashMap<&str, &DiskEntry> {
    let mut rows: HashMap<_, _> = cache
        .index
        .iter()
        .map(|(path, row)| (path.as_str(), row))
        .collect();
    for row in cache
        .snapshot
        .entries
        .iter()
        .filter(|row| row.kind != "file")
    {
        rows.insert(row.path.as_str(), row);
    }
    rows
}
fn comparison_direct_coverage(rows: &HashMap<&str, &DiskEntry>) -> HashSet<String> {
    let mut represented = HashMap::<&str, u64>::new();
    for row in rows.values() {
        let count = represented.entry(row.parent_path.as_str()).or_default();
        *count = count.saturating_add(row.directory_count);
    }
    rows.values()
        .filter(|row| {
            row.complete
                && row.directory_count
                    == represented
                        .get(row.path.as_str())
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(1)
        })
        .map(|row| row.path.clone())
        .collect()
}
/// Absence propagates only through immediately connected retained directories.
/// Existing incomplete rows and missing intermediate index rows stop the proof.
fn comparison_absent_directories(
    rows: &HashMap<&str, &DiskEntry>,
    other: &HashMap<&str, &DiskEntry>,
    coverage: &HashSet<String>,
) -> HashSet<String> {
    let mut candidates: Vec<_> = other.values().collect();
    candidates.sort_unstable_by_key(|row| Path::new(&row.path).components().count());
    let mut absent = HashSet::new();
    for row in candidates {
        if !rows.contains_key(row.path.as_str())
            && (coverage.contains(&row.parent_path) || absent.contains(&row.parent_path))
        {
            absent.insert(row.path.clone());
        }
    }
    absent
}
fn comparison_files(cache: &Cache) -> HashMap<String, DiskEntry> {
    let mut files: HashMap<_, _> = cache
        .snapshot
        .entries
        .iter()
        .filter(|row| row.kind == "file" && row.complete)
        .map(|row| (row.path.clone(), row.clone()))
        .collect();
    for (path, &(bytes, logical)) in &cache.file_details.rows {
        let mut row = new_entry(Path::new(path), false);
        row.allocated_bytes = bytes;
        row.logical_bytes = logical;
        row.file_count = 1;
        row.complete = true;
        files.insert(path.clone(), row);
    }
    files
}
fn comparison_file_coverage(
    directories: &HashMap<&str, &DiskEntry>,
    files: &HashMap<String, DiskEntry>,
    directory_coverage: &HashSet<String>,
) -> HashSet<String> {
    let mut represented = HashMap::<&str, u64>::new();
    for row in directories.values() {
        let count = represented.entry(&row.parent_path).or_default();
        *count = count.saturating_add(row.file_count);
    }
    for row in files.values() {
        let count = represented.entry(&row.parent_path).or_default();
        *count = count.saturating_add(1);
    }
    directories
        .values()
        .filter(|row| {
            directory_coverage.contains(&row.path)
                && row.file_count == represented.get(row.path.as_str()).copied().unwrap_or(0)
        })
        .map(|row| row.path.clone())
        .collect()
}
fn comparison_net(before: u64, after: u64) -> i64 {
    (i128::from(after) - i128::from(before)).clamp(i128::from(i64::MIN), i128::from(i64::MAX))
        as i64
}
fn compare_archives(
    baseline: &Cache,
    current: &Cache,
    scope: Option<&Path>,
    result: &mut DiskScanComparison,
) {
    result.baseline_started_at_ms = Some(baseline.snapshot.started_at_ms);
    result.baseline_updated_at_ms = Some(baseline.snapshot.updated_at_ms);
    if baseline.snapshot.index_policy_version != current.snapshot.index_policy_version
        || baseline.snapshot.index_policy_version == 0
    {
        result.status = "unavailable".into();
        result.limited = true;
        return;
    }
    let before = comparison_directories(baseline);
    let after = comparison_directories(current);
    let before_coverage = comparison_direct_coverage(&before);
    let after_coverage = comparison_direct_coverage(&after);
    let absent_before = comparison_absent_directories(&before, &after, &before_coverage);
    let absent_after = comparison_absent_directories(&after, &before, &after_coverage);
    let in_scope = |path: &str| scope.is_none_or(|scope| Path::new(path).starts_with(scope));
    // Candidates are complete measurements in both archives, or measured
    // directories whose absence is proven by a complete immediate parent layer.
    // An asymmetric retained index row alone never proves creation/deletion.
    let paths: BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .copied()
        .filter(|path| in_scope(path))
        .collect();
    let mut partitions = BTreeMap::<String, DiskScanComparisonEntry>::new();
    for path in paths {
        let pair = match (before.get(path), after.get(path)) {
            (Some(a), Some(b)) if a.complete && b.complete => {
                Some((*a, a.allocated_bytes, b.allocated_bytes))
            }
            (Some(a), None) if a.complete && absent_after.contains(path) => {
                Some((*a, a.allocated_bytes, 0))
            }
            (None, Some(b)) if b.complete && absent_before.contains(path) => {
                Some((*b, 0, b.allocated_bytes))
            }
            _ => None,
        };
        if let Some((row, before_bytes, after_bytes)) = pair {
            partitions.insert(
                path.into(),
                DiskScanComparisonEntry {
                    path: path.into(),
                    name: row.name.clone(),
                    parent_path: row.parent_path.clone(),
                    kind: "directory".into(),
                    before_bytes,
                    after_bytes,
                    growth_bytes: 0,
                    reduction_bytes: 0,
                    net_bytes: 0,
                    detail_limited: true,
                },
            );
        }
    }
    let old_files = comparison_files(baseline);
    let new_files = comparison_files(current);
    let old_file_coverage = comparison_file_coverage(&before, &old_files, &before_coverage);
    let new_file_coverage = comparison_file_coverage(&after, &new_files, &after_coverage);
    let file_paths: BTreeSet<_> = old_files.keys().chain(new_files.keys()).collect();
    for path in file_paths {
        if !in_scope(path) {
            continue;
        }
        let pair = match (old_files.get(path), new_files.get(path)) {
            (Some(a), Some(b)) => Some((a, a.allocated_bytes, b.allocated_bytes)),
            (Some(a), None)
                if absent_after.contains(&a.parent_path)
                    || new_file_coverage.contains(&a.parent_path) =>
            {
                Some((a, a.allocated_bytes, 0))
            }
            (None, Some(b))
                if absent_before.contains(&b.parent_path)
                    || old_file_coverage.contains(&b.parent_path) =>
            {
                Some((b, 0, b.allocated_bytes))
            }
            _ => None,
        };
        if let Some((row, before_bytes, after_bytes)) = pair {
            // A path can change between file and directory. Preserve its
            // directory partition for descendants, and put the file endpoint
            // in that same partition rather than replacing its identity.
            if let Some(directory) = partitions.get_mut(path) {
                directory.before_bytes = directory.before_bytes.saturating_add(before_bytes);
                directory.after_bytes = directory.after_bytes.saturating_add(after_bytes);
                continue;
            }
            if Path::new(path).ancestors().skip(1).any(|parent| {
                parent
                    .to_str()
                    .is_some_and(|parent| partitions.contains_key(parent))
            }) {
                partitions.insert(
                    path.clone(),
                    DiskScanComparisonEntry {
                        path: path.clone(),
                        name: row.name.clone(),
                        parent_path: row.parent_path.clone(),
                        kind: "file".into(),
                        before_bytes,
                        after_bytes,
                        growth_bytes: 0,
                        reduction_bytes: 0,
                        net_bytes: 0,
                        detail_limited: false,
                    },
                );
            }
        }
    }
    // Lexical path order plus a prefix stack constructs a nearest-comparable
    // ancestor forest even if a bounded index omitted intermediate directories.
    // Subtract ORIGINAL child totals, never already-subtracted residuals.
    let mut ancestors = Vec::<String>::new();
    let mut deductions = HashMap::<String, (u64, u64)>::new();
    let mut ordered_partitions: Vec<_> = partitions.iter().collect();
    ordered_partitions.sort_unstable_by(|(a, _), (b, _)| Path::new(a).cmp(Path::new(b)));
    for (path, partition) in ordered_partitions {
        while ancestors
            .last()
            .is_some_and(|parent| !Path::new(path).starts_with(parent))
        {
            ancestors.pop();
        }
        if let Some(parent) = ancestors.last() {
            let deduction = deductions.entry(parent.clone()).or_default();
            deduction.0 = deduction.0.saturating_add(partition.before_bytes);
            deduction.1 = deduction.1.saturating_add(partition.after_bytes);
        }
        if partition.kind == "directory" {
            ancestors.push(path.clone());
        }
    }
    let scopes: Vec<&str> = scope
        .map(|path| vec![path.to_str().unwrap_or("")])
        .unwrap_or_else(|| current.snapshot.roots.iter().map(String::as_str).collect());
    result.limited = scopes.iter().any(|path| !partitions.contains_key(*path));
    let has_comparable = !partitions.is_empty();
    for (path, mut partition) in partitions {
        let (subtract_before, subtract_after) = deductions.get(&path).copied().unwrap_or_default();
        if subtract_before > partition.before_bytes || subtract_after > partition.after_bytes {
            // Reject inconsistent legacy/corrupt attribution instead of turning
            // a saturated subtraction into a plausible positive difference.
            result.limited = true;
            continue;
        }
        partition.before_bytes -= subtract_before;
        partition.after_bytes -= subtract_after;
        partition.growth_bytes = partition.after_bytes.saturating_sub(partition.before_bytes);
        partition.reduction_bytes = partition.before_bytes.saturating_sub(partition.after_bytes);
        partition.net_bytes = comparison_net(partition.before_bytes, partition.after_bytes);
        result.limited |=
            partition.detail_limited && (partition.before_bytes > 0 || partition.after_bytes > 0);
        result.growth_bytes = result.growth_bytes.saturating_add(partition.growth_bytes);
        result.reduction_bytes = result
            .reduction_bytes
            .saturating_add(partition.reduction_bytes);
        if partition.net_bytes != 0 {
            result.entries.push(partition);
        }
    }
    result.net_bytes = comparison_net(result.reduction_bytes, result.growth_bytes);
    result.entries.sort_unstable_by(|a, b| {
        b.growth_bytes
            .max(b.reduction_bytes)
            .cmp(&a.growth_bytes.max(a.reduction_bytes))
            .then(a.path.cmp(&b.path))
    });
    // RPC payload is bounded independently of the durable index budget. Totals
    // still cover every comparable partition, even when small rows are omitted.
    if result.entries.len() > MAX_SHALLOW_DIRECTORY_ROWS {
        result.entries.truncate(MAX_SHALLOW_DIRECTORY_ROWS);
        result.limited = true;
    }
    result.status = if has_comparable {
        "ready"
    } else {
        "unavailable"
    }
    .into();
}

/// Restore an exact scope for navigation without starting, finalizing or
/// cancelling any worker. A cache miss is explicitly represented as null.
pub fn get_cached_disk_scan(
    path: Option<String>,
    allow_protected: bool,
    retain_scan_id: Option<String>,
) -> Result<Option<DiskScan>, String> {
    lookup_cached_disk_scan(path, allow_protected, retain_scan_id.as_deref(), false)
}
fn lookup_cached_disk_scan(
    path: Option<String>,
    allow_protected: bool,
    retain_scan_id: Option<&str>,
    reusable_only: bool,
) -> Result<Option<DiskScan>, String> {
    let roots = lexical_roots(path, allow_protected)?;
    let root_strings: Vec<_> = roots.iter().map(|p| string(p)).collect();
    let key = cache_key(&roots, allow_protected);
    let mut reg = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?;
    let mut matching: Vec<_> = reg
        .runs
        .values()
        .filter(|run| run.allow_protected == allow_protected)
        .filter_map(|run| {
            run.snapshot
                .lock()
                .ok()
                .filter(|s| {
                    s.roots == root_strings
                        && (!reusable_only
                            || reusable_snapshot(s)
                            || (s.status == "running" && !run.cancelled.load(Ordering::Relaxed)))
                })
                .map(|s| s.clone())
        })
        .collect();
    matching.sort_by_key(|s| {
        (
            reusable_only && s.status == "running",
            s.updated_at_ms,
            s.started_at_ms,
        )
    });
    if let Some(mut snapshot) = matching.pop() {
        if snapshot.status == "running" {
            snapshot.elapsed_ms = now_ms().saturating_sub(snapshot.started_at_ms);
        } else {
            snapshot.cache_hit = true;
            snapshot.cache_age_ms = now_ms().saturating_sub(snapshot.updated_at_ms);
        }
        return Ok(Some(snapshot));
    }
    let Some(cached) = reg
        .cached
        .get(&key)
        .filter(|cache| !reusable_only || reusable_snapshot(&cache.snapshot))
        .cloned()
        .or_else(|| {
            read_cache(&key).filter(|cache| !reusable_only || reusable_snapshot(&cache.snapshot))
        })
    else {
        return Ok(None);
    };
    let run = run_from_cache(cached, allow_protected);
    let snapshot = run
        .snapshot
        .lock()
        .map_err(|_| "Disk scan unavailable")?
        .clone();
    if reg.runs.len() >= MAX_RESIDENT_RUNS {
        let active = reg.active.clone();
        let focused = reg.focused.clone();
        reg.runs.retain(|id, _| {
            [active.as_ref(), focused.as_ref()].contains(&Some(id))
                || Some(id.as_str()) == retain_scan_id
        });
    }
    reg.runs.insert(snapshot.scan_id.clone(), run);
    Ok(Some(snapshot))
}
fn source_run(scan_id: &str) -> Result<Arc<Run>, String> {
    registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .runs
        .get(scan_id)
        .cloned()
        .ok_or_else(|| "Unknown disk scan ID".into())
}
// Policy and scope membership precede any filesystem I/O, including metadata.
fn source_directory_path(run: &Run, requested: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(requested);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Directory path must be absolute and must not contain parent traversal".into());
    }
    let path: PathBuf = path.components().collect();
    if !run.allow_protected && protected_path(&path) {
        return Err(format!("permission-required: {PERMISSION_MESSAGE}"));
    }
    if excluded(&path) {
        return Err("cloud-excluded: cloud storage is excluded".into());
    }
    let snapshot = run.snapshot.lock().map_err(|_| "Disk scan unavailable")?;
    if !snapshot.roots.iter().any(|root| path.starts_with(root)) {
        return Err("Directory path is outside the source scan roots".into());
    }
    Ok(path)
}
fn readable_directory(path: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        let metadata =
            fs::symlink_metadata(&prefix).map_err(|e| format!("Cannot inspect directory: {e}"))?;
        if metadata.file_type().is_symlink() {
            return Err("Directory paths must not traverse symbolic links".into());
        }
        if placeholder(&metadata) {
            return Err("cloud-excluded: dataless placeholders are excluded".into());
        }
        if !metadata.is_dir() {
            return Err("Directory path must be a directory".into());
        }
    }
    Ok(())
}
/// Explicitly fill a missing snapshot scope, without restarting Home/Applications.
/// Exact persisted scopes are reused; a different active scan is never cancelled.
pub fn inspect_disk_directory(scan_id: String, requested: String) -> Result<DiskScan, String> {
    let source = source_run(&scan_id)?;
    let path = source_directory_path(&source, &requested)?;
    let source_roots = source
        .snapshot
        .lock()
        .map_err(|_| "Disk scan unavailable")?
        .roots
        .clone();
    let target_path = Some(string(&path));
    let mut result = match lookup_cached_disk_scan(
        target_path.clone(),
        source.allow_protected,
        Some(&scan_id),
        true,
    )? {
        Some(exact) => exact,
        None => start_disk_scan_impl(
            target_path.clone(),
            false,
            source.allow_protected,
            Some(&scan_id),
        )?,
    };
    // An old shallow cache can be upgraded once, only for a selected child
    // scope whose own layer lacks summaries. New policy results and old
    // complete layers are reusable even if deeper rows were clipped.
    let selected_child = !source_roots.iter().any(|root| path == Path::new(root));
    let legacy_missing_detail = if selected_child
        && result.status == "complete"
        && result.index_policy_version < INDEX_POLICY_VERSION
    {
        let candidate = source_run(&result.scan_id)?;
        match snapshot_view(&candidate, &string(&path)) {
            Ok(view) => !view.view_detail_complete,
            Err(error) => error.starts_with("not-indexed:"),
        }
    } else {
        false
    };
    if legacy_missing_detail || !(result.status == "running" || reusable_snapshot(&result)) {
        result = start_disk_scan_impl(target_path, true, source.allow_protected, Some(&scan_id))?;
    }
    if result.scan_id != scan_id {
        let target = source_run(&result.scan_id)?;
        let mut snapshot = target
            .snapshot
            .lock()
            .map_err(|_| "Disk scan unavailable")?;
        snapshot.source_scan_id = Some(scan_id);
        snapshot.source_roots = Some(source_roots);
        snapshot.cache_hit |= result.cache_hit;
        if snapshot.cache_hit {
            snapshot.cache_age_ms = now_ms().saturating_sub(snapshot.updated_at_ms);
        }
        result = snapshot.clone();
    }
    Ok(result)
}
/// Enumerate names in the selected layer only. A bounded ordered window avoids
/// retaining all files in memory or adding them to the persistent scan cache.
pub fn list_disk_directory(
    scan_id: String,
    requested: String,
    after_path: Option<String>,
    limit: Option<usize>,
) -> Result<DiskDirectoryPage, String> {
    let run = source_run(&scan_id)?;
    directory_page(
        &run,
        &requested,
        after_path.as_deref(),
        limit.unwrap_or(200),
    )
}
fn directory_page(
    run: &Run,
    requested: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<DiskDirectoryPage, String> {
    let path = source_directory_path(run, requested)?;
    let limit = limit.clamp(1, MAX_DIRECTORY_PAGE);
    if let Some(after) = after {
        let cursor = Path::new(after);
        if cursor.parent() != Some(path.as_path())
            || cursor
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(
                "Directory cursor must identify a direct child of the selected directory".into(),
            );
        }
    }
    readable_directory(&path)?;
    let mut reader = DirectoryFrame::open_layer(&path, source_device(run, &path)?)
        .map_err(|e| format!("Cannot list directory: {e}"))?;
    directory_page_from_reader(run, &path, after, limit, &mut reader)
}
fn directory_page_from_reader(
    run: &Run,
    path: &Path,
    after: Option<&str>,
    limit: usize,
    reader: &mut DirectoryFrame,
) -> Result<DiskDirectoryPage, String> {
    let mut names = BTreeSet::<PathBuf>::new();
    let mut issues = Vec::new();
    while let Some(child) = reader.next() {
        match child {
            Ok(child) => {
                let child_path = child.path;
                if after.is_some_and(|cursor| child_path.as_path() <= Path::new(cursor)) {
                    continue;
                }
                names.insert(child_path);
                if names.len() > limit + 1 {
                    names.pop_last();
                }
            }
            Err(error) if issues.len() < MAX_ISSUES => issues.push(DiskIssue {
                path: string(path),
                kind: "unreadable".into(),
                message: error.to_string(),
            }),
            Err(_) => {}
        }
    }
    let has_more = names.len() > limit;
    if has_more {
        names.pop_last();
    }
    let next_after_path = has_more.then(|| names.last().map(|p| string(p))).flatten();
    let snapshot = run
        .snapshot
        .lock()
        .map_err(|_| "Disk scan unavailable")?
        .clone();
    let index = run
        .index
        .lock()
        .map_err(|_| "Disk scan index unavailable")?;
    let mut entries = Vec::with_capacity(names.len());
    for child in names {
        let mut item = DiskDirectoryItem {
            path: string(&child),
            name: child
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            parent_path: string(path),
            kind: "unknown".into(),
            allocated_bytes: None,
            logical_bytes: None,
            measurement: "unknown".into(),
            complete: false,
            hard_link_identity: None,
        };
        if scope_excluded(run, &child) || excluded(&child) {
            item.measurement = "excluded".into();
        } else {
            match reader.child_metadata(child.file_name().unwrap_or_default()) {
                Ok(meta) if meta.dataless => {
                    item.measurement = "excluded".into();
                }
                Ok(meta) if meta.symlink => {
                    item.kind = "symlink".into();
                    item.measurement = "excluded".into();
                }
                Ok(meta) if meta.directory => {
                    item.kind = if child.extension().is_some_and(|e| e == "app") {
                        "application"
                    } else {
                        "directory"
                    }
                    .into();
                    if let Some(row) = snapshot
                        .entries
                        .iter()
                        .find(|e| e.path == item.path && e.kind != "file")
                        .filter(|e| e.complete || e.directory_count > 0)
                        .or_else(|| index.get(&item.path))
                    {
                        item.allocated_bytes = Some(row.allocated_bytes);
                        item.logical_bytes = Some(row.logical_bytes);
                        item.measurement = "summary".into();
                        item.complete = row.complete;
                    }
                }
                Ok(meta) => {
                    item.kind = "file".into();
                    item.measurement = "file".into();
                    item.complete = true;
                    item.allocated_bytes = Some(meta.allocated);
                    item.logical_bytes = Some(meta.logical);
                    if meta.links > 1 {
                        item.hard_link_identity = Some(format!("{}:{}", meta.device, meta.inode));
                    }
                }
                Err(error) => {
                    if issues.len() < MAX_ISSUES {
                        issues.push(DiskIssue {
                            path: item.path.clone(),
                            kind: "unreadable".into(),
                            message: error.to_string(),
                        });
                    }
                }
            }
        }
        entries.push(item);
    }
    Ok(DiskDirectoryPage {
        scan_id: snapshot.scan_id, path: string(path), source_updated_at_ms: snapshot.updated_at_ms,
        listed_at_ms: now_ms(), entries, has_more, next_after_path, issues,
        scope_note: "Current directory names and file metadata only; no recursive scan or content reads. Directory sizes are snapshot summaries and missing sizes are unknown. File bytes are inode sizes before source-scan hard-link attribution; shared hardLinkIdentity values must not be summed twice. Pages reflect the live directory and may change between requests.".into(),
    })
}
fn source_device(run: &Run, path: &Path) -> Result<Option<u64>, String> {
    #[cfg(unix)]
    {
        let roots = run
            .snapshot
            .lock()
            .map_err(|_| "Disk scan unavailable")?
            .roots
            .clone();
        let root = roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.len())
            .ok_or("Directory path is outside the source scan roots")?;
        Ok(Some(
            fs::symlink_metadata(root).map_err(|e| e.to_string())?.dev(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (run, path);
        Ok(None)
    }
}

fn validate_inspection_target(run: &Run, path: &Path) -> Result<(), String> {
    readable_directory(path)?;
    #[cfg(unix)]
    if source_device(run, path)?
        .is_some_and(|device| fs::symlink_metadata(path).is_ok_and(|meta| meta.dev() != device))
    {
        return Err("mount-excluded: directory is on a different filesystem".into());
    }
    Ok(())
}

/// Browse a source snapshot without probing paths or changing the source run.
pub fn get_disk_scan_view(scan_id: String, path: String) -> Result<DiskScan, String> {
    let run = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .runs
        .get(&scan_id)
        .cloned()
        .ok_or("Unknown disk scan ID")?;
    let result = snapshot_view(&run, &path);
    let incomplete = match &result {
        Ok(view) => !view.view_detail_complete,
        Err(error) => error.starts_with("not-indexed:"),
    };
    if !incomplete {
        return result;
    }
    let requested = source_directory_path(&run, &path)?;
    let snapshot = run
        .snapshot
        .lock()
        .map_err(|_| "Disk scan unavailable")?
        .clone();
    if snapshot.status != "complete"
        || snapshot.freshness == "partial"
        || snapshot.index_policy_version != INDEX_POLICY_VERSION
    {
        return result;
    }
    let roots: Vec<_> = snapshot.roots.iter().map(PathBuf::from).collect();
    let key = cache_key(&roots, run.allow_protected);
    let Some(db) = cache_file(&key).map(|file| file.with_extension("index.sqlite")) else {
        return result;
    };
    let Ok(Some(layer)) = incremental::cached_layer(
        &db,
        &format!("{INDEX_POLICY_VERSION}\n{key}"),
        &roots,
        run.allow_protected,
        configured_cache_limit() * MIB / 2,
        &snapshot.scan_id,
        &requested,
    ) else {
        return result;
    };
    let complete = layer.detail_complete;
    let mut rows: DirectoryIndex = layer
        .entries
        .into_iter()
        .chain(layer.treemap_directories)
        .map(|row| (row.path.clone(), row))
        .collect();
    rows.insert(layer.summary.path.clone(), layer.summary);
    let mut view = snapshot_view_rows(snapshot, &requested, &rows, false)?;
    view.view_detail_complete &= complete;
    view.cache_hit = true;
    view.cache_age_ms = now_ms().saturating_sub(view.updated_at_ms);
    Ok(view)
}
fn snapshot_view(run: &Run, requested: &str) -> Result<DiskScan, String> {
    let path = source_directory_path(run, requested)?;
    let snapshot = run
        .snapshot
        .lock()
        .map_err(|_| "Disk scan unavailable")?
        .clone();
    let index = run
        .index
        .lock()
        .map_err(|_| "Disk scan index unavailable")?;
    snapshot_view_rows(
        snapshot,
        &path,
        &index,
        run.cancelled.load(Ordering::Relaxed),
    )
}
/// Select display detail from a single measurement, never from live metadata.
/// Missing ancestors reject a candidate instead of implying a different parent.
fn treemap_extra_rows<'a>(
    roots: &[String],
    base: &[DiskEntry],
    candidates: impl Iterator<Item = &'a DiskEntry>,
) -> Vec<DiskEntry> {
    let root_for = |path: &Path| {
        roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.len())
    };
    let eligible = |row: &&DiskEntry| {
        let path = Path::new(&row.path);
        row.allocated_bytes > 0
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            && root_for(path).is_some_and(|root| {
                path.strip_prefix(root)
                    .is_ok_and(|relative| (1..=3).contains(&relative.components().count()))
            })
    };
    let mut available: HashMap<&str, &DiskEntry> = candidates
        .filter(eligible)
        .map(|row| (row.path.as_str(), row))
        .collect();
    available.extend(
        base.iter()
            .filter(eligible)
            .map(|row| (row.path.as_str(), row)),
    );
    let existing: HashSet<_> = base.iter().map(|row| row.path.as_str()).collect();
    let mut ordered: Vec<_> = available
        .values()
        .copied()
        .filter(|row| !existing.contains(row.path.as_str()))
        .collect();
    ordered.sort_unstable_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.path.cmp(&b.path))
    });
    let mut admitted = HashSet::<&str>::new();
    let mut result = Vec::new();
    let mut used_bytes = 0usize;
    for row in ordered {
        if admitted.contains(row.path.as_str()) {
            continue;
        }
        let root = Path::new(root_for(Path::new(&row.path)).unwrap());
        let mut pending = Vec::new();
        let mut cursor = Path::new(&row.path);
        let mut connected = true;
        while cursor != root {
            let Some(ancestor) = cursor
                .to_str()
                .and_then(|path| available.get(path))
                .copied()
            else {
                connected = false;
                break;
            };
            if ancestor.allocated_bytes == 0
                || (cursor != Path::new(&row.path) && ancestor.kind == "file")
            {
                connected = false;
                break;
            }
            if !existing.contains(ancestor.path.as_str())
                && !admitted.contains(ancestor.path.as_str())
            {
                pending.push(ancestor);
            }
            let Some(parent) = cursor.parent() else {
                connected = false;
                break;
            };
            if Some(ancestor.parent_path.as_str()) != parent.to_str() {
                connected = false;
                break;
            }
            cursor = parent;
        }
        let cost = pending.iter().map(|row| entry_bytes(row)).sum::<usize>();
        if !connected
            || result.len() + pending.len() > MAX_TREEMAP_ROWS
            || cost > MAX_TREEMAP_BYTES.saturating_sub(used_bytes)
        {
            continue;
        }
        used_bytes += cost;
        for entry in pending.into_iter().rev() {
            admitted.insert(entry.path.as_str());
            result.push(entry.clone());
        }
    }
    result.sort_unstable_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.path.cmp(&b.path))
    });
    result
}
fn treemap_projection(
    snapshot: &DiskScan,
    index: &DirectoryIndex,
    samples: &[DiskEntry],
) -> Vec<DiskEntry> {
    if snapshot.status != "complete" {
        return Vec::new();
    }
    treemap_extra_rows(
        &snapshot.roots,
        &snapshot.entries,
        index.values().chain(samples.iter()),
    )
}
fn snapshot_view_rows(
    mut snapshot: DiskScan,
    path: &Path,
    index: &DirectoryIndex,
    cancelled: bool,
) -> Result<DiskScan, String> {
    let path_string = string(path);
    // During traversal a source-root index row has only its own inode. Live shallow
    // rows carry newer cumulative totals and also cover jobs still being scanned.
    let mut directory_rows: HashMap<String, DiskEntry> = snapshot
        .entries
        .iter()
        .filter(|e| e.kind != "file" && (e.path == path_string || e.parent_path == path_string))
        .map(|e| (e.path.clone(), e.clone()))
        .collect();
    for row in index
        .values()
        .filter(|e| e.path == path_string || e.parent_path == path_string)
    {
        if !snapshot.roots.contains(&row.path) || !directory_rows.contains_key(&row.path) {
            directory_rows.insert(row.path.clone(), row.clone());
        }
    }
    let Some(summary) = directory_rows.get(&path_string).cloned() else {
        if let Some(issue) = snapshot.issues.iter().find(|issue| {
            !issue.path.is_empty()
                && path.starts_with(&issue.path)
                && matches!(
                    issue.kind.as_str(),
                    "permission-excluded" | "cloud-excluded" | "mount-excluded" | "unreadable"
                )
        }) {
            let kind = if issue.kind == "permission-excluded" {
                "permission-required"
            } else {
                &issue.kind
            };
            return Err(format!("{kind}: {}", issue.message));
        }
        return Err("not-indexed: directory is not recorded in this snapshot; scan this directory to inspect it".into());
    };
    // Each child row's count includes that child's own inode and its disjoint
    // descendant tree. Missing deeper rows do not change a retained child
    // summary, while a missing immediate child necessarily leaves a deficit.
    let represented_directories = directory_rows
        .values()
        .filter(|row| row.parent_path == path_string)
        .fold(1u64, |sum, child| sum.saturating_add(child.directory_count));
    let indexed_finished =
        !snapshot.roots.iter().any(|root| root == &path_string) && index.contains_key(&path_string);
    let scope_finished = snapshot.status == "complete"
        || (snapshot.status == "running" && indexed_finished && !cancelled);
    snapshot.view_detail_complete =
        scope_finished && summary.directory_count == represented_directories;
    let mut entries: Vec<_> = directory_rows.into_values().collect();
    let source_samples = snapshot
        .entries
        .iter()
        .filter(|row| row.kind == "file")
        .cloned()
        .collect::<Vec<_>>();
    let child_paths: HashSet<_> = entries
        .iter()
        .filter(|row| row.parent_path == path_string)
        .map(|row| row.path.as_str())
        .collect();
    let mut grandchildren = index
        .values()
        .filter(|row| child_paths.contains(row.parent_path.as_str()))
        .collect::<Vec<_>>();
    grandchildren.sort_unstable_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.path.cmp(&b.path))
    });
    entries.extend(
        grandchildren
            .into_iter()
            .take(MAX_SHALLOW_DIRECTORY_ROWS)
            .cloned(),
    );
    // These are the source snapshot's bounded largest-file samples, never a full file list.
    entries.extend(
        snapshot
            .entries
            .iter()
            .filter(|e| e.kind == "file" && e.parent_path == path_string)
            .cloned(),
    );
    entries.sort_by_key(|e| std::cmp::Reverse(e.allocated_bytes));
    snapshot
        .source_roots
        .get_or_insert_with(|| snapshot.roots.clone());
    snapshot.roots = vec![path_string.clone()];
    snapshot.view_path = Some(path_string);
    snapshot.allocated_bytes = summary.allocated_bytes;
    snapshot.logical_bytes = summary.logical_bytes;
    snapshot.scanned_files = summary.file_count;
    snapshot.scanned_directories = summary.directory_count;
    snapshot.coverage_complete =
        summary.complete && matches!(snapshot.status.as_str(), "running" | "complete");
    snapshot.entries = entries;
    snapshot.treemap_entries = treemap_projection(&snapshot, index, &source_samples);
    snapshot
        .issues
        .retain(|i| Path::new(&i.path).starts_with(path));
    if snapshot.cache_hit {
        snapshot.cache_age_ms = now_ms().saturating_sub(snapshot.updated_at_ms);
    }
    if snapshot.status == "running" {
        snapshot.elapsed_ms = now_ms().saturating_sub(snapshot.started_at_ms);
    }
    Ok(snapshot)
}
pub fn cancel_disk_scan(scan_id: String) -> Result<DiskScan, String> {
    let run = registry()
        .lock()
        .map_err(|_| "Disk scan registry unavailable")?
        .runs
        .get(&scan_id)
        .cloned()
        .ok_or("Unknown disk scan ID")?;
    let key = {
        let snapshot = run.snapshot.lock().map_err(|_| "Disk scan unavailable")?;
        if snapshot.status != "running" {
            drop(snapshot);
            return get_disk_scan(scan_id);
        }
        run.cancelled.store(true, Ordering::Relaxed);
        cache_key(
            &snapshot.roots.iter().map(PathBuf::from).collect::<Vec<_>>(),
            run.allow_protected,
        )
    };
    // A normal app exit can stop the core before the periodic writer gets another turn.
    // Save current shallow progress synchronously, independently of the last-good file.
    write_cache(&key, &run, true);
    get_disk_scan(scan_id)
}
fn issue(s: &mut DiskScan, path: &Path, kind: &str, message: String) {
    s.issue_count += 1;
    if s.issues.len() < MAX_ISSUES {
        s.issues.push(DiskIssue {
            path: string(path),
            kind: kind.into(),
            message,
        });
    }
}
fn allocated(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}
fn placeholder(meta: &fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        meta.st_flags() & 0x40000000 != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = meta;
        false
    }
}
fn internal_cache_path(path: &Path) -> bool {
    static CACHE_PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CACHE_PATHS
        .get_or_init(|| {
            let Some(cache) = cache_directory() else {
                return Vec::new();
            };
            let mut paths = vec![cache.clone()];
            #[cfg(target_os = "macos")]
            if let Ok(relative) = cache.strip_prefix("/") {
                let physical = Path::new("/System/Volumes/Data").join(relative);
                if fs::symlink_metadata(&cache)
                    .ok()
                    .zip(fs::symlink_metadata(&physical).ok())
                    .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
                {
                    paths.push(physical);
                }
            }
            paths
        })
        .iter()
        .any(|cache| path.starts_with(cache))
}
fn excluded(path: &Path) -> bool {
    let mut previous = false;
    for component in path.components() {
        if previous
            && (component.as_os_str() == "CloudStorage"
                || component.as_os_str() == "Mobile Documents")
        {
            return true;
        }
        previous = component.as_os_str() == "Library";
    }
    false
}

// Descendants are visited only after their parent passed `excluded`. Checking
// the new boundary avoids rescanning every ancestor for every leaf in deep trees.
fn excluded_child(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "Library")
        && path
            .file_name()
            .is_some_and(|name| name == "CloudStorage" || name == "Mobile Documents")
}

fn new_entry(path: &Path, directory: bool) -> DiskEntry {
    DiskEntry {
        path: string(path),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| string(path)),
        parent_path: path.parent().map(string).unwrap_or_default(),
        kind: if directory && path.extension().is_some_and(|s| s == "app") {
            "application"
        } else if directory {
            "directory"
        } else {
            "file"
        }
        .into(),
        allocated_bytes: 0,
        logical_bytes: 0,
        file_count: 0,
        directory_count: 0,
        complete: false,
    }
}
#[derive(Default)]
struct Progress {
    files: u64,
    dirs: u64,
    bytes: u64,
    logical: u64,
    rows: BTreeMap<PathBuf, DiskEntry>,
    row_bytes: usize,
    shallow_limited: bool,
    largest: Vec<DiskEntry>,
    issues: Vec<DiskIssue>,
    issue_count: u64,
    current: PathBuf,
    root: PathBuf,
    root_is_home: bool,
    user_applications: Option<PathBuf>,
    app_candidates: Vec<DiskApplication>,
    completed: DirectoryIndex,
    file_details: Vec<(PathBuf, u64, u64)>,
    file_detail_bytes: usize,
}
fn entry_bytes(row: &DiskEntry) -> usize {
    row.path.len() + row.name.len() + row.parent_path.len() + row.kind.len() + 192
}
fn retain_shallow(p: &mut Progress, path: &Path, row: DiskEntry) {
    if let Some(existing) = p.rows.get_mut(path) {
        *existing = row;
        return;
    }
    let bytes = entry_bytes(&row);
    if p.rows.len() >= MAX_SHALLOW_DIRECTORY_ROWS
        || p.row_bytes.saturating_add(bytes) > MAX_SHALLOW_BYTES
    {
        p.shallow_limited = true;
        return;
    }
    p.row_bytes += bytes;
    p.rows.insert(path.to_path_buf(), row);
}
fn flush(run: &Run, p: &mut Progress) {
    flush_file_details(run, p);
    if let Ok(mut s) = run.snapshot.lock() {
        if !p.app_candidates.is_empty() {
            let is_installed = |app: &DiskApplication| {
                installed_application(Path::new(&app.path), p.user_applications.as_deref())
            };
            let installed = s
                .applications
                .iter()
                .filter(|app| is_installed(app))
                .count();
            let mut remaining = [
                MAX_APPLICATIONS.saturating_sub(s.applications.len() - installed),
                MAX_APPLICATIONS.saturating_sub(installed),
            ];
            for app in p.app_candidates.drain(..) {
                let installed = is_installed(&app);
                let allowance = &mut remaining[usize::from(installed)];
                if *allowance > 0 {
                    *allowance -= 1;
                    s.applications.push(app);
                } else if !installed {
                    s.application_copies_truncated = true;
                }
            }
        }
        if !p.completed.is_empty() {
            if let Ok(mut index) = run.index.lock() {
                index.extend(p.completed.drain());
            }
        }
        s.scanned_files += p.files;
        s.scanned_directories += p.dirs;
        s.allocated_bytes += p.bytes;
        s.logical_bytes += p.logical;
        if let Some(row) = s.entries.iter_mut().find(|e| e.path == string(&p.root)) {
            row.allocated_bytes += p.bytes;
            row.logical_bytes += p.logical;
            row.file_count += p.files;
            row.directory_count += p.dirs;
        }
        s.current_path = string(&p.current);
        s.updated_at_ms = now_ms();
        s.elapsed_ms = s.updated_at_ms.saturating_sub(s.started_at_ms);
        s.issue_count += p.issue_count;
        for i in p.issues.drain(..) {
            if s.issues.len() < MAX_ISSUES {
                s.issues.push(i);
            }
        }
        let include_files = !p.largest.is_empty() && {
            let mut files = s.entries.iter().rev().take_while(|e| e.kind == "file");
            let smallest = files.next();
            smallest.is_none_or(|e| p.largest[0].allocated_bytes > e.allocated_bytes)
                || files.count() + 1 < MAX_LARGE_FILES
        };
        if !include_files {
            p.largest.clear();
        }
        if p.rows.is_empty() && !p.shallow_limited && !include_files {
            p.files = 0;
            p.dirs = 0;
            p.bytes = 0;
            p.logical = 0;
            return;
        }
        let mut indexes: HashMap<String, usize> = s
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i))
            .collect();
        let mut directory_rows = s.entries.iter().filter(|e| e.kind != "file").count();
        let mut directory_bytes: usize = s
            .entries
            .iter()
            .filter(|e| e.kind != "file")
            .map(entry_bytes)
            .sum();
        let mut shallow_limited = p.shallow_limited;
        for row in p.rows.values_mut() {
            if let Some(index) = indexes.get(&row.path) {
                s.entries[*index] = row.clone();
            } else if directory_rows < MAX_SHALLOW_DIRECTORY_ROWS
                && directory_bytes.saturating_add(entry_bytes(row)) <= MAX_SHALLOW_BYTES
            {
                directory_rows += 1;
                directory_bytes += entry_bytes(row);
                indexes.insert(row.path.clone(), s.entries.len());
                s.entries.push(row.clone());
            } else {
                shallow_limited = true;
            }
        }
        if shallow_limited && !run.shallow_limited.swap(true, Ordering::Relaxed) {
            issue(&mut s, Path::new(""), "summary-limit",
                "Live directory summaries are bounded; scanning and totals continue for the full scope. Browse the directory index for further detail.".into());
        }
        for row in p.largest.drain(..) {
            if !indexes.contains_key(&row.path) {
                indexes.insert(row.path.clone(), s.entries.len());
                s.entries.push(row);
            }
        }
        let mut file_rows: Vec<_> = s
            .entries
            .iter()
            .filter(|e| e.kind == "file")
            .cloned()
            .collect();
        file_rows.sort_by_key(|e| std::cmp::Reverse(e.allocated_bytes));
        file_rows.truncate(MAX_LARGE_FILES);
        s.entries.retain(|e| e.kind != "file");
        s.entries.extend(file_rows);
    }
    p.files = 0;
    p.dirs = 0;
    p.bytes = 0;
    p.logical = 0;
    // Keep cumulative per-job issue count so completed parent rows remain visibly partial.
}
#[derive(Clone, Debug)]
struct InventoryMeta {
    directory: bool,
    symlink: bool,
    allocated: u64,
    logical: u64,
    device: u64,
    inode: u64,
    links: u64,
    dataless: bool,
    mount: bool,
}
impl InventoryMeta {
    fn read(path: &Path) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            return DirectoryFrame::open_layer(parent, None)?.child_metadata(name);
        }
        let m = fs::symlink_metadata(path)?;
        Ok(Self::from_metadata(&m))
    }
    fn from_metadata(m: &fs::Metadata) -> Self {
        Self {
            directory: m.is_dir(),
            symlink: m.file_type().is_symlink(),
            allocated: allocated(m),
            logical: if m.is_dir() { 0 } else { m.len() },
            #[cfg(unix)]
            device: m.dev(),
            #[cfg(not(unix))]
            device: 0,
            #[cfg(unix)]
            inode: m.ino(),
            #[cfg(not(unix))]
            inode: 0,
            #[cfg(unix)]
            links: m.nlink(),
            #[cfg(not(unix))]
            links: 1,
            dataless: placeholder(m),
            mount: false,
        }
    }
}
struct InventoryNode {
    path: PathBuf,
    metadata: Option<InventoryMeta>,
    error: Option<String>,
}
// A queued job pins only its root directory. Readers themselves never cross
// threads, and each worker opens an independent cursor relative to this anchor.
#[cfg(target_os = "macos")]
#[derive(Clone)]
struct DescriptorAnchor(Arc<fs::File>);
struct ScanJob {
    root_index: usize,
    path: PathBuf,
    metadata: InventoryMeta,
    root_device: u64,
    #[cfg(target_os = "macos")]
    parent: DescriptorAnchor,
}
struct DirectoryFrame {
    path: PathBuf,
    reader: DirectoryReader,
    inventory: Option<incremental::Layer>,
}
enum DirectoryReader {
    #[cfg(not(target_os = "macos"))]
    Standard(fs::ReadDir),
    #[cfg(target_os = "macos")]
    Portable(bulk::PortableDirectory),
    #[cfg(target_os = "macos")]
    Bulk(bulk::BulkDirectory),
}
impl DirectoryFrame {
    fn open_layer(path: &Path, device: Option<u64>) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Self::open_on_device(path, false, device)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (path, device);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Safe directory listing requires the macOS descriptor reader",
            ))
        }
    }
    fn child_metadata(&self, name: &std::ffi::OsStr) -> std::io::Result<InventoryMeta> {
        #[cfg(target_os = "macos")]
        match &self.reader {
            DirectoryReader::Portable(reader) => reader.child_metadata(name),
            DirectoryReader::Bulk(reader) => reader.child_metadata(name),
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = name;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Relative metadata is unavailable for this reader",
            ))
        }
    }
    fn node_metadata(&self, child: &InventoryNode) -> std::io::Result<InventoryMeta> {
        if let Some(error) = &child.error {
            return Err(std::io::Error::other(error.clone()));
        }
        if let Some(meta) = &child.metadata {
            return Ok(meta.clone());
        }
        #[cfg(target_os = "macos")]
        {
            self.child_metadata(child.path.file_name().unwrap_or_default())
        }
        #[cfg(not(target_os = "macos"))]
        {
            InventoryMeta::read(&child.path)
        }
    }

    #[cfg(test)]
    fn open(path: &Path, native: bool) -> std::io::Result<Self> {
        Self::open_on_device(path, native, None)
    }
    fn open_on_device(path: &Path, native: bool, device: Option<u64>) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Self::from_file(path, native, bulk::open_directory(path, device)?)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (native, device);
            Ok(Self {
                path: path.to_path_buf(),
                reader: DirectoryReader::Standard(fs::read_dir(path)?),
                inventory: None,
            })
        }
    }
    fn open_verified(path: &Path, native: bool, expected: &InventoryMeta) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let file = bulk::open_directory(path, Some(expected.device))?;
            bulk::verify_identity(&file, expected)?;
            Self::from_file(path, native, file)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let actual = InventoryMeta::read(path)?;
            if !actual.directory
                || actual.device != expected.device
                || actual.inode != expected.inode
            {
                return Err(std::io::Error::other(
                    "Directory identity changed before traversal",
                ));
            }
            Self::open_on_device(path, native, Some(expected.device))
        }
    }
    #[cfg(target_os = "macos")]
    fn from_file(path: &Path, native: bool, file: fs::File) -> std::io::Result<Self> {
        let file = if native {
            match bulk::BulkDirectory::from_file(file) {
                Ok(reader) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                        reader: DirectoryReader::Bulk(reader),
                        inventory: None,
                    })
                }
                Err((error, file)) if error.kind() == std::io::ErrorKind::Unsupported => file,
                Err((error, _)) => return Err(error),
            }
        } else {
            file
        };
        Ok(Self {
            path: path.to_path_buf(),
            reader: DirectoryReader::Portable(bulk::PortableDirectory::from_file(file)?),
            inventory: None,
        })
    }
    #[cfg(target_os = "macos")]
    fn raw_fd(&self) -> std::os::fd::RawFd {
        match &self.reader {
            DirectoryReader::Portable(reader) => reader.raw_fd(),
            DirectoryReader::Bulk(reader) => reader.raw_fd(),
        }
    }
    #[cfg(target_os = "macos")]
    fn anchor(&self) -> std::io::Result<DescriptorAnchor> {
        use std::os::fd::BorrowedFd;
        // Clone the opened descriptor once per root, never its raw DIR pointer.
        let owned = unsafe { BorrowedFd::borrow_raw(self.raw_fd()) }.try_clone_to_owned()?;
        Ok(DescriptorAnchor(Arc::new(fs::File::from(owned))))
    }
    fn open_child(
        &self,
        path: &Path,
        native: bool,
        expected: &InventoryMeta,
    ) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Self::from_file(
                path,
                native,
                bulk::open_relative(
                    self.raw_fd(),
                    path.file_name().unwrap_or_default(),
                    expected,
                )?,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::open_on_device(path, native, Some(expected.device))
        }
    }
    fn open_job(job: &ScanJob, native: bool) -> std::io::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            Self::from_file(
                &job.path,
                native,
                bulk::open_relative(
                    job.parent.0.as_raw_fd(),
                    job.path.file_name().unwrap_or_default(),
                    &job.metadata,
                )?,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::open_on_device(&job.path, native, Some(job.metadata.device))
        }
    }
    fn next(&mut self) -> Option<std::io::Result<InventoryNode>> {
        match &mut self.reader {
            #[cfg(not(target_os = "macos"))]
            DirectoryReader::Standard(children) => children.next().map(|r| {
                r.map(|e| InventoryNode {
                    path: e.path(),
                    metadata: None,
                    error: None,
                })
            }),
            #[cfg(target_os = "macos")]
            DirectoryReader::Bulk(reader) => reader.next(&self.path),
            #[cfg(target_os = "macos")]
            DirectoryReader::Portable(reader) => reader.next(&self.path),
        }
    }
}

/// Darwin's metadata-only directory batches. Buffer fields are 4-byte packed and
/// conditional on returned masks; parse every offset/bound rather than casting structs.
#[cfg(target_os = "macos")]
mod bulk {
    use super::{InventoryMeta, InventoryNode, BULK_BUFFER_BYTES};
    use std::{
        ffi::OsStr,
        fs::File,
        io,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Path,
    };
    // Each failed Darwin call leaves its descriptor/cursor usable. In particular,
    // getattrlistbulk advances its cursor only when it returns a positive count;
    // never replay a successful batch. Bound retries so a signal storm remains a
    // visible partial scan rather than making traversal unresponsive indefinitely.
    const INTERRUPTED_RETRIES: usize = 8;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Operation {
        OpenDirectory,
        OpenRelative,
        MetadataOpen,
        Metadata,
        FdOpenDirectory,
        ReadDirectory,
        BulkRead,
    }
    fn retry_interrupted<T>(
        _operation: Operation,
        mut call: impl FnMut() -> io::Result<T>,
    ) -> io::Result<T> {
        for attempt in 0..=INTERRUPTED_RETRIES {
            #[cfg(test)]
            let result = faults::take(_operation).map_or_else(&mut call, Err);
            #[cfg(not(test))]
            let result = call();
            match result {
                Err(error)
                    if error.raw_os_error() == Some(libc::EINTR)
                        && attempt < INTERRUPTED_RETRIES => {}
                result => return result,
            }
        }
        unreachable!()
    }
    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        common: u32,
        volume: u32,
        directory: u32,
        file: u32,
        fork: u32,
    }
    unsafe extern "C" {
        fn getattrlistbulk(
            fd: i32,
            attributes: *mut AttrList,
            buffer: *mut std::ffi::c_void,
            size: usize,
            options: u64,
        ) -> i32;
    }
    pub(super) fn open_directory(path: &Path, device: Option<u64>) -> io::Result<File> {
        use std::os::unix::fs::MetadataExt;

        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW_ANY. O_NOFOLLOW
        // and O_NOFOLLOW_ANY are mutually exclusive. Use the raw call because
        // std::OpenOptions retries EINTR internally without a bound.
        let fd = retry_interrupted(Operation::OpenDirectory, || {
            let fd = unsafe { libc::open(path.as_ptr(), 0x00100000 | 0x01000000 | 0x20000000) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(fd)
            }
        })?;
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = retry_interrupted(Operation::Metadata, || file.metadata())?;
        if super::placeholder(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Cloud placeholder directory is excluded",
            ));
        }
        if device.is_some_and(|device| metadata.dev() != device) {
            return Err(io::Error::other("Directory is on a different filesystem"));
        }
        Ok(file)
    }
    unsafe extern "C" {
        #[cfg_attr(target_arch = "x86_64", link_name = "fdopendir$INODE64")]
        fn fdopendir(fd: i32) -> *mut std::ffi::c_void;
        #[cfg_attr(target_arch = "x86_64", link_name = "readdir$INODE64")]
        fn readdir(directory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn closedir(directory: *mut std::ffi::c_void) -> i32;
        fn dirfd(directory: *mut std::ffi::c_void) -> i32;
        fn openat(fd: i32, path: *const std::ffi::c_char, flags: i32, ...) -> i32;
        fn __error() -> *mut i32;
    }
    pub(super) fn open_relative(
        fd: i32,
        name: &OsStr,
        expected: &InventoryMeta,
    ) -> io::Result<File> {
        use std::os::fd::FromRawFd;
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(invalid());
        }
        let name = std::ffi::CString::new(bytes).map_err(|_| invalid())?;
        // O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC: the parent descriptor is trusted.
        let fd = retry_interrupted(Operation::OpenRelative, || {
            let child = unsafe { openat(fd, name.as_ptr(), 0x00000100 | 0x00100000 | 0x01000000) };
            if child < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(child)
            }
        })?;
        let file = unsafe { File::from_raw_fd(fd) };
        verify_identity(&file, expected)?;
        Ok(file)
    }
    pub(super) fn verify_identity(file: &File, expected: &InventoryMeta) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let actual = retry_interrupted(Operation::Metadata, || file.metadata())?;
        if !actual.is_dir() || actual.dev() != expected.device || actual.ino() != expected.inode {
            return Err(io::Error::other(
                "Directory identity changed before traversal",
            ));
        }
        if super::placeholder(&actual) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Cloud placeholder directory is excluded",
            ));
        }
        Ok(())
    }
    fn metadata_at(fd: i32, name: &OsStr) -> io::Result<InventoryMeta> {
        use std::os::fd::FromRawFd;
        let name = name.as_bytes();
        if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
            return Err(invalid());
        }
        let name = std::ffi::CString::new(name).map_err(|_| invalid())?;
        let child = retry_interrupted(Operation::MetadataOpen, || {
            let child = unsafe { openat(fd, name.as_ptr(), 0x00008000 | 0x00200000 | 0x01000000) };
            if child < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(child)
            }
        })?;
        let file = unsafe { File::from_raw_fd(child) };
        Ok(InventoryMeta::from_metadata(&retry_interrupted(
            Operation::Metadata,
            || file.metadata(),
        )?))
    }
    // Descriptor-backed fallback for filesystems without getattrlistbulk support.
    // Never reopen through read_dir(path), which would undo O_NOFOLLOW rejection.
    pub(super) struct PortableDirectory {
        directory: *mut std::ffi::c_void,
        ended: bool,
    }
    impl PortableDirectory {
        pub fn from_file(file: File) -> io::Result<Self> {
            use std::os::fd::{FromRawFd, IntoRawFd};
            let fd = file.into_raw_fd();
            let directory = match retry_interrupted(Operation::FdOpenDirectory, || {
                let directory = unsafe { fdopendir(fd) };
                if directory.is_null() {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(directory)
                }
            }) {
                Ok(directory) => directory,
                Err(error) => {
                    drop(unsafe { File::from_raw_fd(fd) });
                    return Err(error);
                }
            };
            Ok(Self {
                directory,
                ended: false,
            })
        }
        pub fn raw_fd(&self) -> std::os::fd::RawFd {
            unsafe { dirfd(self.directory) }
        }
        /// Metadata stays relative to the directory we actually opened, even
        /// if any ancestor is renamed or replaced after enumeration. O_EVTONLY
        /// opens no data stream; O_SYMLINK inspects a symlink inode itself.
        pub fn child_metadata(&self, name: &OsStr) -> io::Result<InventoryMeta> {
            metadata_at(unsafe { dirfd(self.directory) }, name)
        }
        pub fn next(&mut self, parent: &Path) -> Option<io::Result<InventoryNode>> {
            if self.ended {
                return None;
            }
            loop {
                let entry = match retry_interrupted(Operation::ReadDirectory, || {
                    unsafe {
                        *__error() = 0;
                    }
                    let entry = unsafe { readdir(self.directory) }.cast::<u8>();
                    if !entry.is_null() {
                        Ok(Some(entry))
                    } else {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(0) {
                            Ok(None)
                        } else {
                            Err(error)
                        }
                    }
                }) {
                    Ok(Some(entry)) => entry,
                    Ok(None) => {
                        self.ended = true;
                        return None;
                    }
                    Err(error) => {
                        self.ended = true;
                        return Some(Err(error));
                    }
                };
                // Darwin struct dirent: ino64, seekoff64, reclen16, namlen16, type8, name[].
                let reclen =
                    unsafe { std::ptr::read_unaligned(entry.add(16).cast::<u16>()) } as usize;
                let namlen =
                    unsafe { std::ptr::read_unaligned(entry.add(18).cast::<u16>()) } as usize;
                if namlen == 0 || namlen > 1024 || 21 + namlen >= reclen {
                    return Some(Err(invalid()));
                }
                let name = unsafe { std::slice::from_raw_parts(entry.add(21), namlen) };
                if name == b"." || name == b".." {
                    continue;
                }
                if name.contains(&b'/') || name.contains(&0) {
                    return Some(Err(invalid()));
                }
                return Some(Ok(InventoryNode {
                    path: parent.join(OsStr::from_bytes(name)),
                    metadata: None,
                    error: None,
                }));
            }
        }
    }
    impl Drop for PortableDirectory {
        fn drop(&mut self) {
            unsafe {
                closedir(self.directory);
            }
        }
    }
    const COMMON: u32 = 0x80000000 | 0x20000000 | 0x02000000 | 0x00040000 | 0x8 | 0x2 | 0x1;
    const DIR: u32 = 0x4 | 0x8;
    const FILE: u32 = 0x1 | 0x4 | 0x200;
    pub(super) struct BulkDirectory {
        file: File,
        buffer: Vec<u64>,
        offset: usize,
        remaining: usize,
        ended: bool,
    }
    impl BulkDirectory {
        pub fn child_metadata(&self, name: &OsStr) -> io::Result<InventoryMeta> {
            metadata_at(self.file.as_raw_fd(), name)
        }
        #[cfg(test)]
        pub fn open(path: &Path, device: Option<u64>) -> io::Result<Self> {
            Self::from_file(open_directory(path, device)?).map_err(|(error, _)| error)
        }
        pub fn raw_fd(&self) -> std::os::fd::RawFd {
            self.file.as_raw_fd()
        }
        pub fn from_file(file: File) -> Result<Self, (io::Error, File)> {
            let mut this = Self {
                file,
                buffer: vec![0; BULK_BUFFER_BYTES / 8],
                offset: 0,
                remaining: 0,
                ended: false,
            };
            if let Err(error) = this.fill() {
                let error = if matches!(error.raw_os_error(), Some(22 | 45 | 78)) {
                    io::Error::new(io::ErrorKind::Unsupported, error)
                } else {
                    error
                };
                return Err((error, this.file));
            }
            Ok(this)
        }
        fn bytes(&self) -> &[u8] {
            unsafe {
                std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), self.buffer.len() * 8)
            }
        }
        fn fill(&mut self) -> io::Result<()> {
            let count = retry_interrupted(Operation::BulkRead, || {
                let mut attributes = AttrList {
                    bitmapcount: 5,
                    reserved: 0,
                    common: COMMON,
                    volume: 0,
                    directory: DIR,
                    file: FILE,
                    fork: 0,
                };
                let count = unsafe {
                    getattrlistbulk(
                        self.file.as_raw_fd(),
                        &mut attributes,
                        self.buffer.as_mut_ptr().cast(),
                        self.buffer.len() * 8,
                        0,
                    )
                };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(count)
                }
            })?;
            self.offset = 0;
            self.remaining = count as usize;
            self.ended = count == 0;
            Ok(())
        }
        pub fn next(&mut self, parent: &Path) -> Option<io::Result<InventoryNode>> {
            if self.remaining == 0 {
                if self.ended {
                    return None;
                }
                if let Err(e) = self.fill() {
                    self.ended = true;
                    return Some(Err(e));
                }
                if self.ended {
                    return None;
                }
            }
            let available = &self.bytes()[self.offset..];
            let length = read_u32(available, 0).unwrap_or(0) as usize;
            if length < 24 || length > available.len() {
                self.ended = true;
                self.remaining = 0;
                return Some(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid Darwin directory batch record",
                )));
            }
            let result = parse_with_metadata(&available[..length], parent, |name| {
                metadata_at(self.file.as_raw_fd(), name)
            });
            self.offset += length;
            self.remaining -= 1;
            Some(result)
        }
    }
    fn read_u32(b: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(
            b.get(offset..offset + 4)?.try_into().ok()?,
        ))
    }
    fn read_u64(b: &[u8], offset: usize) -> Option<u64> {
        Some(u64::from_ne_bytes(
            b.get(offset..offset + 8)?.try_into().ok()?,
        ))
    }
    fn take32(b: &[u8], offset: &mut usize, present: bool) -> Option<u32> {
        if !present {
            return None;
        }
        let value = read_u32(b, *offset);
        *offset += 4;
        value
    }
    fn take64(b: &[u8], offset: &mut usize, present: bool) -> Option<u64> {
        if !present {
            return None;
        }
        let value = read_u64(b, *offset);
        *offset += 8;
        value
    }
    fn invalid() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid Darwin directory batch attributes",
        )
    }
    #[cfg(test)]
    fn parse(record: &[u8], parent: &Path) -> io::Result<InventoryNode> {
        parse_with_metadata(record, parent, |_| {
            Err(io::Error::from_raw_os_error(libc::EINTR))
        })
    }
    fn parse_with_metadata(
        record: &[u8],
        parent: &Path,
        recover: impl FnOnce(&OsStr) -> io::Result<InventoryMeta>,
    ) -> io::Result<InventoryNode> {
        let common = read_u32(record, 4).ok_or_else(invalid)?;
        let directory = read_u32(record, 12).ok_or_else(invalid)?;
        let file = read_u32(record, 16).ok_or_else(invalid)?;
        let mut cursor = 24;
        let error = take32(record, &mut cursor, common & 0x20000000 != 0).unwrap_or(0);
        if common & 1 == 0 {
            return Err(invalid());
        }
        let name_reference = cursor;
        let displacement = read_u32(record, cursor).ok_or_else(invalid)? as i32 as i64;
        let name_length = read_u32(record, cursor + 4).ok_or_else(invalid)? as usize;
        cursor += 8;
        let name_start = (name_reference as i64)
            .checked_add(displacement)
            .filter(|p| *p >= 0)
            .ok_or_else(invalid)? as usize;
        let name = record
            .get(name_start..name_start.checked_add(name_length).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let name = name.strip_suffix(&[0]).ok_or_else(invalid)?;
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(invalid());
        }
        let path = parent.join(OsStr::from_bytes(name));
        if error != 0 {
            if error as i32 == libc::EINTR {
                return Ok(match recover(OsStr::from_bytes(name)) {
                    Ok(metadata) => InventoryNode {
                        path,
                        metadata: Some(metadata),
                        error: None,
                    },
                    Err(error) => InventoryNode {
                        path,
                        metadata: None,
                        error: Some(error.to_string()),
                    },
                });
            }
            return Ok(InventoryNode {
                path,
                metadata: None,
                error: Some(io::Error::from_raw_os_error(error as i32).to_string()),
            });
        }
        let device = take32(record, &mut cursor, common & 2 != 0).map(|v| v as u64);
        let kind = take32(record, &mut cursor, common & 8 != 0);
        let flags = take32(record, &mut cursor, common & 0x40000 != 0);
        let inode = take64(record, &mut cursor, common & 0x02000000 != 0);
        let mount = take32(record, &mut cursor, directory & 4 != 0);
        let dir_allocated = take64(record, &mut cursor, directory & 8 != 0);
        let links = take32(record, &mut cursor, file & 1 != 0).map(|v| v as u64);
        let file_allocated = take64(record, &mut cursor, file & 4 != 0);
        let logical = take64(record, &mut cursor, file & 0x200 != 0);
        let metadata = (|| {
            let kind = kind?;
            let directory = kind == 2;
            Some(InventoryMeta {
                directory,
                symlink: kind == 5,
                allocated: if directory {
                    dir_allocated?
                } else {
                    file_allocated?
                },
                logical: if directory { 0 } else { logical? },
                device: device?,
                inode: inode?,
                links: if directory { 1 } else { links? },
                dataless: flags? & 0x40000000 != 0,
                mount: directory && mount? & 3 != 0,
            })
        })();
        Ok(InventoryNode {
            path,
            metadata,
            error: None,
        })
    }
    #[cfg(test)]
    mod faults {
        use super::Operation;
        use std::{cell::RefCell, collections::VecDeque, io};

        #[derive(Default)]
        struct State {
            pending: VecDeque<(Operation, i32)>,
            calls: Vec<Operation>,
        }
        thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }
        pub(super) struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                STATE.with(|state| *state.borrow_mut() = None);
            }
        }
        pub(super) fn inject(errors: impl IntoIterator<Item = (Operation, i32)>) -> Guard {
            STATE.with(|state| {
                *state.borrow_mut() = Some(State {
                    pending: errors.into_iter().collect(),
                    calls: Vec::new(),
                });
            });
            Guard
        }
        pub(super) fn take(operation: Operation) -> Option<io::Error> {
            STATE.with(|state| {
                let mut slot = state.borrow_mut();
                let state = slot.as_mut()?;
                state.calls.push(operation);
                if state
                    .pending
                    .front()
                    .is_some_and(|(next, _)| *next == operation)
                {
                    state
                        .pending
                        .pop_front()
                        .map(|(_, error)| io::Error::from_raw_os_error(error))
                } else {
                    None
                }
            })
        }
        pub(super) fn calls(operation: Operation) -> usize {
            STATE.with(|state| {
                state.borrow().as_ref().map_or(0, |state| {
                    state
                        .calls
                        .iter()
                        .filter(|&&call| call == operation)
                        .count()
                })
            })
        }
    }
    #[cfg(test)]
    mod retry_tests {
        use super::*;
        use std::{
            collections::BTreeSet,
            fs,
            path::PathBuf,
            sync::atomic::{AtomicUsize, Ordering},
        };

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let root = std::env::temp_dir().join(format!(
                    "cublet-eintr-{}-{nonce}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&root).unwrap();
                // macOS temp_dir commonly starts at /var, an ancestor symlink
                // which the production O_NOFOLLOW_ANY guard correctly rejects.
                Self(fs::canonicalize(root).unwrap())
            }
            fn reader(&self) -> BulkDirectory {
                // Force several native batches, independently of the production
                // buffer size, to exercise failures after an earlier success.
                BulkDirectory {
                    file: open_directory(&self.0, None).unwrap(),
                    buffer: vec![0; 64],
                    offset: 0,
                    remaining: 0,
                    ended: false,
                }
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fn error_record(name: &[u8], error: i32) -> Vec<u8> {
            let length = (36 + name.len() + 1 + 7) & !7;
            let mut record = vec![0; length];
            record[0..4].copy_from_slice(&(length as u32).to_ne_bytes());
            record[4..8].copy_from_slice(&0xa0000001u32.to_ne_bytes());
            record[24..28].copy_from_slice(&error.to_ne_bytes());
            record[28..32].copy_from_slice(&8u32.to_ne_bytes());
            record[32..36].copy_from_slice(&((name.len() + 1) as u32).to_ne_bytes());
            record[36..36 + name.len()].copy_from_slice(name);
            record
        }
        fn set_record(reader: &mut BulkDirectory, record: &[u8]) {
            reader.buffer = record
                .chunks_exact(8)
                .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
                .collect();
            reader.offset = 0;
            reader.remaining = 1;
        }
        fn names(reader: &mut BulkDirectory, parent: &Path) -> Vec<PathBuf> {
            let mut found = Vec::new();
            while let Some(node) = reader.next(parent) {
                found.push(node.unwrap().path);
            }
            found
        }
        #[test]
        fn bulk_cursor_retries_initial_and_later_batches_without_replay() {
            let fixture = Fixture::new();
            let expected: BTreeSet<_> = (0..24)
                .map(|n| {
                    let path = fixture.0.join(format!("entry-{n:02}"));
                    fs::write(&path, b"data").unwrap();
                    path
                })
                .collect();
            let mut reader = fixture.reader();
            let mut found = Vec::new();
            {
                let _fault = faults::inject([(Operation::BulkRead, libc::EINTR)]);
                found.push(reader.next(&fixture.0).unwrap().unwrap().path);
                assert_eq!(faults::calls(Operation::BulkRead), 2);
                while reader.remaining > 0 {
                    found.push(reader.next(&fixture.0).unwrap().unwrap().path);
                }
            }
            assert!(
                found.len() < expected.len(),
                "fixture must span native batches"
            );
            {
                let _fault = faults::inject([(Operation::BulkRead, libc::EINTR)]);
                found.extend(names(&mut reader, &fixture.0));
                assert!(faults::calls(Operation::BulkRead) >= 3);
            }
            assert_eq!(
                found.len(),
                expected.len(),
                "successful batches must not be replayed"
            );
            assert_eq!(found.into_iter().collect::<BTreeSet<_>>(), expected);
        }
        #[test]
        fn bulk_retry_exhaustion_and_real_errors_remain_terminal() {
            let fixture = Fixture::new();
            for error in [libc::EINTR, libc::EACCES, libc::EIO] {
                let mut reader = fixture.reader();
                let attempts = if error == libc::EINTR {
                    INTERRUPTED_RETRIES + 1
                } else {
                    1
                };
                let _fault =
                    faults::inject(std::iter::repeat_n((Operation::BulkRead, error), attempts));
                assert_eq!(
                    reader
                        .next(&fixture.0)
                        .unwrap()
                        .err()
                        .unwrap()
                        .raw_os_error(),
                    Some(error)
                );
                assert!(reader.next(&fixture.0).is_none());
                assert_eq!(faults::calls(Operation::BulkRead), attempts);
            }
        }
        #[test]
        fn portable_cursor_recovers_eintr_and_distinguishes_eof_from_errors() {
            let fixture = Fixture::new();
            for name in ["a", "b"] {
                fs::write(fixture.0.join(name), name).unwrap();
            }
            let _fault = faults::inject([
                (Operation::FdOpenDirectory, libc::EINTR),
                (Operation::ReadDirectory, libc::EINTR),
            ]);
            let mut reader =
                PortableDirectory::from_file(open_directory(&fixture.0, None).unwrap()).unwrap();
            let mut found = BTreeSet::new();
            while let Some(node) = reader.next(&fixture.0) {
                assert!(found.insert(node.unwrap().path));
            }
            assert_eq!(
                found,
                BTreeSet::from([fixture.0.join("a"), fixture.0.join("b")])
            );
            assert!(reader.next(&fixture.0).is_none());
            assert_eq!(faults::calls(Operation::FdOpenDirectory), 2);
            drop(_fault);
            for error in [libc::EINTR, libc::EACCES] {
                let mut reader =
                    PortableDirectory::from_file(open_directory(&fixture.0, None).unwrap())
                        .unwrap();
                let attempts = if error == libc::EINTR {
                    INTERRUPTED_RETRIES + 1
                } else {
                    1
                };
                let _fault = faults::inject(std::iter::repeat_n(
                    (Operation::ReadDirectory, error),
                    attempts,
                ));
                assert_eq!(
                    reader
                        .next(&fixture.0)
                        .unwrap()
                        .err()
                        .unwrap()
                        .raw_os_error(),
                    Some(error)
                );
                assert!(reader.next(&fixture.0).is_none());
                assert_eq!(faults::calls(Operation::ReadDirectory), attempts);
            }
        }
        #[test]
        fn descriptor_open_and_stat_retry_without_weakening_nofollow_or_identity() {
            use std::os::unix::fs::symlink;
            let fixture = Fixture::new();
            let child = fixture.0.join("child");
            fs::create_dir(&child).unwrap();
            symlink(&child, fixture.0.join("link")).unwrap();
            let expected = InventoryMeta::from_metadata(&fs::symlink_metadata(&child).unwrap());
            let _fault = faults::inject([
                (Operation::OpenDirectory, libc::EINTR),
                (Operation::Metadata, libc::EINTR),
                (Operation::OpenRelative, libc::EINTR),
                (Operation::Metadata, libc::EINTR),
                (Operation::MetadataOpen, libc::EINTR),
                (Operation::Metadata, libc::EINTR),
            ]);
            let parent = open_directory(&fixture.0, Some(expected.device)).unwrap();
            let opened = open_relative(parent.as_raw_fd(), OsStr::new("child"), &expected).unwrap();
            assert!(opened.metadata().unwrap().is_dir());
            let actual = metadata_at(parent.as_raw_fd(), OsStr::new("child")).unwrap();
            assert_eq!(actual.inode, expected.inode);
            assert_eq!(faults::calls(Operation::OpenDirectory), 2);
            assert_eq!(faults::calls(Operation::OpenRelative), 2);
            assert_eq!(faults::calls(Operation::MetadataOpen), 2);
            assert_eq!(faults::calls(Operation::Metadata), 6);
            assert!(open_relative(parent.as_raw_fd(), OsStr::new("link"), &expected).is_err());
            assert!(open_directory(&fixture.0.join("link"), None).is_err());
            assert!(
                metadata_at(parent.as_raw_fd(), OsStr::new("link"))
                    .unwrap()
                    .symlink
            );
            let mut wrong_device = expected.clone();
            wrong_device.device ^= 1;
            assert!(open_relative(parent.as_raw_fd(), OsStr::new("child"), &wrong_device).is_err());
            assert!(open_directory(&fixture.0, Some(wrong_device.device)).is_err());
        }
        #[test]
        fn per_entry_eintr_recovers_metadata_from_original_parent_after_rename() {
            let fixture = Fixture::new();
            let original = fixture.0.join("original");
            fs::create_dir(&original).unwrap();
            fs::write(original.join("child"), b"original").unwrap();
            let expected = fs::symlink_metadata(original.join("child")).unwrap();
            let mut reader = BulkDirectory {
                file: open_directory(&original, None).unwrap(),
                buffer: Vec::new(),
                offset: 0,
                remaining: 0,
                ended: false,
            };
            fs::rename(&original, fixture.0.join("renamed")).unwrap();
            fs::create_dir(&original).unwrap();
            fs::write(
                original.join("child"),
                b"replacement has different metadata",
            )
            .unwrap();
            set_record(&mut reader, &error_record(b"child", libc::EINTR));
            let _fault = faults::inject([
                (Operation::MetadataOpen, libc::EINTR),
                (Operation::Metadata, libc::EINTR),
            ]);
            let node = reader.next(&original).unwrap().unwrap();
            let metadata = node.metadata.unwrap();
            assert!(node.error.is_none());
            assert_eq!(
                metadata.inode,
                InventoryMeta::from_metadata(&expected).inode
            );
            assert_eq!(metadata.logical, expected.len());
            assert_eq!(
                reader.remaining, 0,
                "the error record is consumed exactly once"
            );
            assert_eq!(faults::calls(Operation::MetadataOpen), 2);
        }
        #[test]
        fn per_entry_recovery_keeps_real_failures_and_sustained_eintr_visible() {
            let fixture = Fixture::new();
            let mut reader = fixture.reader();
            for error in [libc::EACCES, libc::EIO] {
                set_record(&mut reader, &error_record(b"missing", error));
                let _fault = faults::inject([]);
                let node = reader.next(&fixture.0).unwrap().unwrap();
                assert!(node.metadata.is_none());
                assert_eq!(
                    node.error.unwrap(),
                    io::Error::from_raw_os_error(error).to_string()
                );
                assert_eq!(
                    faults::calls(Operation::MetadataOpen),
                    0,
                    "real entry errors are not retried"
                );
            }
            fs::write(fixture.0.join("child"), b"data").unwrap();
            set_record(&mut reader, &error_record(b"child", libc::EINTR));
            let _fault = faults::inject(std::iter::repeat_n(
                (Operation::MetadataOpen, libc::EINTR),
                INTERRUPTED_RETRIES + 1,
            ));
            let node = reader.next(&fixture.0).unwrap().unwrap();
            assert!(node.metadata.is_none());
            assert_eq!(
                node.error.unwrap(),
                io::Error::from_raw_os_error(libc::EINTR).to_string()
            );
            assert_eq!(
                faults::calls(Operation::MetadataOpen),
                INTERRUPTED_RETRIES + 1
            );
            drop(_fault);
            set_record(&mut reader, &error_record(b"missing", libc::EINTR));
            let node = reader.next(&fixture.0).unwrap().unwrap();
            assert!(node.metadata.is_none());
            assert_eq!(
                node.error.unwrap(),
                io::Error::from_raw_os_error(libc::ENOENT).to_string()
            );
        }
    }
    #[cfg(test)]
    mod parser_tests {
        use super::*;
        fn name_only() -> Vec<u8> {
            let mut record = vec![0u8; 40];
            record[0..4].copy_from_slice(&40u32.to_ne_bytes());
            record[4..8].copy_from_slice(&0x80000001u32.to_ne_bytes());
            record[24..28].copy_from_slice(&8u32.to_ne_bytes());
            record[28..32].copy_from_slice(&2u32.to_ne_bytes());
            record[32] = b'x';
            record
        }
        #[test]
        fn missing_attribute_groups_request_per_entry_stat_fallback() {
            let node = parse(&name_only(), Path::new("/fixture")).unwrap();
            assert_eq!(node.path, Path::new("/fixture/x"));
            assert!(node.metadata.is_none());
        }
        #[test]
        fn malformed_and_truncated_name_references_fail_without_panicking() {
            let record = name_only();
            for length in 0..34 {
                assert!(parse(&record[..length], Path::new("/fixture")).is_err());
            }
            let mut negative = record.clone();
            negative[24..28].copy_from_slice(&i32::MIN.to_ne_bytes());
            assert!(parse(&negative, Path::new("/fixture")).is_err());
            let mut oversized = record;
            oversized[28..32].copy_from_slice(&u32::MAX.to_ne_bytes());
            assert!(parse(&oversized, Path::new("/fixture")).is_err());
        }
    }
}

// Open children only after their cloud/mount checks. Keep <=64 directory handles per worker.
enum TraversalItem {
    Node(InventoryNode),
    Closed(PathBuf, Option<incremental::Layer>),
}
fn next_path(
    pending: &mut Option<InventoryNode>,
    stack: &mut Vec<DirectoryFrame>,
) -> Option<Result<TraversalItem, (PathBuf, String)>> {
    if let Some(node) = pending.take() {
        return Some(Ok(TraversalItem::Node(node)));
    }
    let frame = stack.last_mut()?;
    match frame.next() {
        Some(Ok(child)) => Some(Ok(TraversalItem::Node(child))),
        Some(Err(error)) => Some(Err((frame.path.clone(), error.to_string()))),
        None => {
            let frame = stack.pop()?;
            Some(Ok(TraversalItem::Closed(frame.path, frame.inventory)))
        }
    }
}
fn primary_application(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "app")
        && !path
            .ancestors()
            .skip(1)
            .any(|parent| parent.extension().is_some_and(|e| e == "app"))
}
fn installed_application(path: &Path, user_applications: Option<&Path>) -> bool {
    let path = path.strip_prefix("/System/Volumes/Data").unwrap_or(path);
    let installed_under = |root: &Path| path != root && path.starts_with(root);
    installed_under(Path::new("/Applications")) || user_applications.is_some_and(installed_under)
}

impl From<&DiskEntry> for DiskApplication {
    fn from(row: &DiskEntry) -> Self {
        Self {
            path: row.path.clone(),
            name: Path::new(&row.path)
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| row.name.clone()),
            bundle_id: None,
            bundle_bytes: row.allocated_bytes,
            support_bytes: 0,
            associated_paths: vec![],
        }
    }
}
fn shallow_directory(path: &Path, root: &Path, root_is_home: bool) -> bool {
    let depth = path
        .strip_prefix(root)
        .map(|p| p.components().count())
        .unwrap_or(0);
    let limit = if root_is_home {
        if path
            .strip_prefix(root)
            .ok()
            .and_then(|path| path.components().next())
            .is_some_and(|part| part.as_os_str() == "Library")
        {
            3
        } else {
            1
        }
    } else if root == Path::new("/Applications") {
        1
    } else {
        2
    };
    path != root && (depth <= limit || primary_application(path))
}
// A directory is folded once when its traversal frame closes. Only the <=64 open
// frames need direct counters; retaining a directory index is an independent budget.
fn fold_directory(
    path: &Path,
    root: &Path,
    active: &mut HashMap<PathBuf, DiskEntry>,
    retained: &mut HashSet<PathBuf>,
    p: &mut Progress,
) {
    let Some(row) = active.remove(path) else {
        return;
    };
    if let Some(parent) = path.parent().and_then(|parent| active.get_mut(parent)) {
        parent.allocated_bytes += row.allocated_bytes;
        parent.logical_bytes += row.logical_bytes;
        parent.file_count += row.file_count;
        parent.directory_count += row.directory_count;
        parent.complete &= row.complete;
    }
    if row.kind == "application" && primary_application(path) {
        p.app_candidates.push(DiskApplication::from(&row));
    }
    if shallow_directory(path, root, p.root_is_home) {
        retain_shallow(p, path, row.clone());
    }
    if retained.remove(path) {
        p.completed.insert(row.path.clone(), row);
    }
}
fn mark_index_limited(run: &Run) {
    if !run.index_limited.swap(true, Ordering::Relaxed) {
        if let Ok(mut s) = run.snapshot.lock() {
            issue(&mut s, Path::new(""), "index-limit",
                "Directory detail index reached its limit; scanning and summary byte totals continue for the full scope".into());
        }
    }
}
fn coverage_issues(run: &Run, scan: &DiskScan) -> bool {
    scan.issue_count
        > u64::from(run.index_limited.load(Ordering::Relaxed))
            + u64::from(run.shallow_limited.load(Ordering::Relaxed))
}
#[cfg(test)]
fn scan_job(run: &Run, job: &Path, root: &Path, seen: &Mutex<HardlinkOwners>, native: bool) {
    scan_subtree(run, job, root, seen, native);
}

#[cfg(test)]
fn scan_subtree(run: &Run, job: &Path, root: &Path, seen: &Mutex<HardlinkOwners>, native: bool) {
    scan_subtree_inner(run, job, root, seen, native, None);
}
fn scan_queued_subtree(
    run: &Run,
    job: &ScanJob,
    root: &Path,
    seen: &Mutex<HardlinkOwners>,
    native: bool,
) {
    scan_subtree_inner(run, &job.path, root, seen, native, Some(job));
}
fn scan_subtree_inner(
    run: &Run,
    job: &Path,
    root: &Path,
    seen: &Mutex<HardlinkOwners>,
    native: bool,
    queued: Option<&ScanJob>,
) {
    if scope_excluded(run, job) || scope_excluded(run, root) {
        record_exclusion(run, job, "permission-excluded", PERMISSION_MESSAGE);
        if let Ok(mut s) = run.snapshot.lock() {
            issue(
                &mut s,
                job,
                "permission-excluded",
                PERMISSION_MESSAGE.into(),
            );
        }
        return;
    }
    let home = home().ok();
    let mut p = Progress {
        root: root.to_path_buf(),
        root_is_home: home.as_deref() == Some(root),
        user_applications: home.map(|home| home.join("Applications")),
        ..Progress::default()
    };
    let mut direct = HashMap::<PathBuf, DiskEntry>::new();
    let mut retained = HashSet::new();
    #[cfg(unix)]
    let root_device = queued
        .map(|job| job.root_device)
        .unwrap_or_else(|| InventoryMeta::read(root).map(|m| m.device).unwrap_or(0));
    let mut last = Instant::now();
    let mut pending = Some(InventoryNode {
        path: job.to_path_buf(),
        metadata: queued.map(|job| job.metadata.clone()),
        error: None,
    });
    let mut stack = Vec::<DirectoryFrame>::new();
    let mut completed_children = 0;
    while let Some(next) = next_path(&mut pending, &mut stack) {
        if run.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let node = match next {
            Ok(TraversalItem::Node(node)) => node,
            Ok(TraversalItem::Closed(path, inventory)) => {
                merge_inventory(run, inventory);
                fold_directory(&path, root, &mut direct, &mut retained, &mut p);
                if path.parent() == Some(job) {
                    completed_children += 1;
                    if completed_children == 2
                        || (completed_children <= EARLY_DIRECTORY_ROWS
                            && last.elapsed() >= Duration::from_millis(EARLY_DIRECTORY_MS as u64))
                    {
                        flush(run, &mut p);
                        p.issue_count = 0;
                        last = Instant::now();
                    }
                }
                continue;
            }
            Err((path, message)) => {
                record_exclusion(run, &path, "unreadable", &message);
                p.issue_count += 1;
                if let Some(row) = direct.get_mut(&path) {
                    row.complete = false;
                }
                if p.issues.len() < MAX_ISSUES {
                    p.issues.push(DiskIssue {
                        path: string(&path),
                        kind: "unreadable".into(),
                        message,
                    });
                }
                continue;
            }
        };
        let path = node.path.as_path();
        if internal_cache_path(path) {
            continue;
        }
        if scope_excluded(run, path) {
            record_exclusion(run, path, "permission-excluded", PERMISSION_MESSAGE);
            p.issue_count += 1;
            if let Some(row) = path.parent().and_then(|parent| direct.get_mut(parent)) {
                row.complete = false;
            }
            if p.issues.len() < MAX_ISSUES {
                p.issues.push(DiskIssue {
                    path: string(path),
                    kind: "permission-excluded".into(),
                    message: PERMISSION_MESSAGE.into(),
                });
            }
            continue;
        }
        let metadata = if let Some(parent) = stack.last() {
            parent.node_metadata(&node)
        } else if let Some(error) = &node.error {
            Err(std::io::Error::other(error.clone()))
        } else {
            node.metadata
                .clone()
                .map(Ok)
                .unwrap_or_else(|| InventoryMeta::read(path))
        };
        let meta = match metadata {
            Ok(m) => m,
            Err(e) => {
                record_exclusion(run, path, "unreadable", &e.to_string());
                p.issue_count += 1;
                if let Some(row) = path.parent().and_then(|parent| direct.get_mut(parent)) {
                    row.complete = false;
                }
                if p.issues.len() < MAX_ISSUES {
                    p.issues.push(DiskIssue {
                        path: string(path),
                        kind: "unreadable".into(),
                        message: e.to_string(),
                    });
                }
                continue;
            }
        };
        if (if path == job {
            excluded(path)
        } else {
            excluded_child(path)
        }) || meta.dataless
        {
            record_exclusion(
                run,
                path,
                "cloud-excluded",
                "Skipped cloud storage or a dataless placeholder to avoid downloading content",
            );
            p.issue_count += 1;
            if let Some(row) = path.parent().and_then(|parent| direct.get_mut(parent)) {
                row.complete = false;
            }
            if p.issues.len() < MAX_ISSUES {
                p.issues.push(DiskIssue { path: string(path), kind: "cloud-excluded".into(), message: "Skipped cloud storage or a dataless placeholder to avoid downloading content".into() });
            }
            continue;
        }
        #[cfg(unix)]
        if meta.directory && (meta.mount || (root_device != 0 && meta.device != root_device)) {
            record_exclusion(
                run,
                path,
                "mount-excluded",
                "Skipped a different filesystem",
            );
            p.issue_count += 1;
            if let Some(row) = path.parent().and_then(|parent| direct.get_mut(parent)) {
                row.complete = false;
            }
            if p.issues.len() < MAX_ISSUES {
                p.issues.push(DiskIssue {
                    path: string(path),
                    kind: "mount-excluded".into(),
                    message: "Skipped a different filesystem".into(),
                });
            }
            continue;
        }
        let dir = meta.directory;
        let mut opened_directory = false;
        if dir {
            // Deep app trees cannot consume the allowance for the roots'
            // direct children or the next layer (including Home dotfolders).
            if run
                .index_budget
                .lock()
                .is_ok_and(|mut budget| budget.retain(path, root, p.root_is_home))
            {
                retained.insert(path.to_path_buf());
            } else {
                mark_index_limited(run);
            }
            let mut row = new_entry(path, true);
            row.complete = true;
            direct.insert(path.to_path_buf(), row);
            let opened = if stack.len() >= 64 {
                Err("Directory nesting exceeds the bounded 64-handle traversal limit".to_string())
            } else {
                let reader = if let Some(parent) = stack.last() {
                    parent.open_child(path, native && run.allow_protected, &meta)
                } else if let Some(queued) = queued {
                    DirectoryFrame::open_job(queued, native && run.allow_protected)
                } else {
                    DirectoryFrame::open_on_device(
                        path,
                        native && run.allow_protected,
                        Some(meta.device),
                    )
                };
                reader.map_err(|e| e.to_string()).map(|mut frame| {
                    frame.inventory = inventory_layer(run, path, &meta);
                    stack.push(frame);
                    opened_directory = true;
                })
            };
            if let Err(message) = opened {
                record_exclusion(run, path, "unreadable", &message);
                if let Some(row) = direct.get_mut(path) {
                    row.complete = false;
                }
                p.issue_count += 1;
                if p.issues.len() < MAX_ISSUES {
                    p.issues.push(DiskIssue {
                        path: string(path),
                        kind: "unreadable".into(),
                        message,
                    });
                }
            }
        }
        if !dir {
            if let Some(layer) = stack.last_mut().and_then(|frame| frame.inventory.as_mut()) {
                layer.observe(path, &meta);
            } else if let Ok(recorder) = run.recorder.lock() {
                if let Some(recorder) = recorder.as_ref() {
                    recorder.mark_incomplete();
                }
            }
        }
        let (bytes, logical) = attributed_bytes(&meta, path, seen);
        if dir {
            p.dirs += 1;
        } else {
            p.files += 1;
        }
        p.bytes += bytes;
        p.logical += logical;
        // Count a file only in its direct parent; descendants are folded once
        // when their directory frame closes (no per-file ancestor walk).
        let owner = if dir { Some(path) } else { path.parent() };
        if let Some(row) = owner.and_then(|owner| direct.get_mut(owner)) {
            row.allocated_bytes += bytes;
            row.logical_bytes += logical;
            row.file_count += u64::from(!dir);
            row.directory_count += u64::from(dir);
        }
        // A single shallow job row gives useful live progress without copying the index.
        if path == job && dir && job != root {
            retain_shallow(&mut p, job, new_entry(job, true));
        }
        if let Some(row) = p.rows.get_mut(job) {
            row.allocated_bytes += bytes;
            row.logical_bytes += logical;
            row.file_count += u64::from(!dir);
            row.directory_count += u64::from(dir);
        }
        if dir && !opened_directory {
            fold_directory(path, root, &mut direct, &mut retained, &mut p);
        }
        if !dir {
            record_file_detail(run, &mut p, path, bytes, logical);
        }
        if !dir && !meta.symlink && (meta.links <= 1 || bytes != 0 || logical != 0) {
            record_large_file(&mut p, path, bytes, logical);
        }
        if last.elapsed() >= Duration::from_millis(PUBLISH_MS as u64) {
            p.current = path.to_path_buf();
            flush(run, &mut p);
            p.issue_count = 0;
            last = Instant::now();
        }
    }
    // Cancellation leaves open frames. Fold their partial counters in postorder.
    let mut remaining: Vec<_> = direct.keys().cloned().collect();
    remaining.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in remaining {
        if let Some(row) = direct.get_mut(&path) {
            row.complete = false;
        }
        fold_directory(&path, root, &mut direct, &mut retained, &mut p);
    }
    flush(run, &mut p);
}
// Only multiply linked non-directories need a ledger. Workers initially charge
// the first encounter, then finalization moves that charge to a stable path.
type HardlinkOwners = HashMap<(u64, u64), HardlinkOwner>;
struct HardlinkOwner {
    first: PathBuf,
    canonical: PathBuf,
    allocated: u64,
    logical: u64,
    symlink: bool,
}
fn attributed_bytes(meta: &InventoryMeta, path: &Path, seen: &Mutex<HardlinkOwners>) -> (u64, u64) {
    #[cfg(unix)]
    if !meta.directory && meta.links > 1 {
        let mut owners = seen.lock().unwrap_or_else(|e| e.into_inner());
        match owners.entry((meta.device, meta.inode)) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if path < entry.get().canonical.as_path() {
                    entry.get_mut().canonical = path.to_path_buf();
                }
                return (0, 0);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(HardlinkOwner {
                    first: path.to_path_buf(),
                    canonical: path.to_path_buf(),
                    allocated: meta.allocated,
                    logical: meta.logical,
                    symlink: meta.symlink,
                });
            }
        }
    }
    (meta.allocated, meta.logical)
}

fn normalize_hardlink_ownership(run: &Run, seen: &Mutex<HardlinkOwners>, roots: &[PathBuf]) {
    let owners = seen.lock().unwrap_or_else(|e| e.into_inner());
    if owners.is_empty() {
        return;
    }
    // Follow the same snapshot -> index lock order as progress publication.
    let Ok(mut snapshot) = run.snapshot.lock() else {
        return;
    };
    let Ok(mut index) = run.index.lock() else {
        return;
    };
    let rows: HashMap<_, _> = snapshot
        .entries
        .iter()
        .enumerate()
        .filter(|(_, row)| row.kind != "file")
        .map(|(position, row)| (row.path.clone(), position))
        .collect();
    let applications: HashMap<_, _> = snapshot
        .applications
        .iter()
        .enumerate()
        .map(|(position, app)| (app.path.clone(), position))
        .collect();
    let mut adjust = |path: &Path, owner: &HardlinkOwner, add: bool| {
        let key = string(path);
        let update = |value: &mut u64, bytes: u64| {
            *value = if add {
                value.saturating_add(bytes)
            } else {
                value.saturating_sub(bytes)
            };
        };
        if let Some(row) = index.get_mut(&key) {
            update(&mut row.allocated_bytes, owner.allocated);
            update(&mut row.logical_bytes, owner.logical);
        }
        if let Some(&position) = rows.get(&key) {
            let row = &mut snapshot.entries[position];
            update(&mut row.allocated_bytes, owner.allocated);
            update(&mut row.logical_bytes, owner.logical);
        }
        if let Some(&position) = applications.get(&key) {
            update(
                &mut snapshot.applications[position].bundle_bytes,
                owner.allocated,
            );
        }
    };
    for owner in owners
        .values()
        .filter(|owner| owner.first != owner.canonical)
    {
        for (path, other, add) in [
            (&owner.first, &owner.canonical, false),
            (&owner.canonical, &owner.first, true),
        ] {
            for ancestor in path
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|ancestor| roots.iter().any(|root| ancestor.starts_with(root)))
            {
                // Common ancestors already have the one charge they should retain.
                if !other.starts_with(ancestor) {
                    adjust(ancestor, owner, add);
                }
            }
        }
    }
    // Every retained name stays represented, but only the deterministic owner
    // carries bytes. Never insert an omitted path beyond the detail allowance.
    if let Ok(mut details) = run.file_details.lock() {
        for owner in owners.values() {
            if let Some(value) = details.rows.get_mut(&string(&owner.first)) {
                *value = (0, 0);
            }
            if let Some(value) = details.rows.get_mut(&string(&owner.canonical)) {
                *value = (owner.allocated, owner.logical);
            }
        }
    }
    // Keep only one sampled path per hardlink identity, including when the
    // incremental recorder declined to retain a complete filesystem index.
    let first_paths: HashSet<_> = owners.values().map(|owner| owner.first.as_path()).collect();
    let mut largest: Vec<_> = snapshot
        .entries
        .iter()
        .filter(|row| row.kind == "file" && !first_paths.contains(Path::new(&row.path)))
        .cloned()
        .collect();
    largest.sort_unstable_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    for owner in owners.values().filter(|owner| !owner.symlink) {
        let mut row = new_entry(&owner.canonical, false);
        row.allocated_bytes = owner.allocated;
        row.logical_bytes = owner.logical;
        row.file_count = 1;
        row.complete = true;
        let position = largest.partition_point(|entry| {
            entry.allocated_bytes > row.allocated_bytes
                || (entry.allocated_bytes == row.allocated_bytes && entry.path < row.path)
        });
        if position < MAX_LARGE_FILES {
            largest.insert(position, row);
            largest.truncate(MAX_LARGE_FILES);
        }
    }
    snapshot.entries.retain(|row| row.kind != "file");
    snapshot.entries.extend(largest);
}

fn inventory_layer(run: &Run, path: &Path, meta: &InventoryMeta) -> Option<incremental::Layer> {
    run.recorder
        .lock()
        .ok()
        .and_then(|recorder| recorder.as_ref().map(|recorder| recorder.layer(path, meta)))
}
fn merge_inventory(run: &Run, layer: Option<incremental::Layer>) {
    if let Some(layer) = layer {
        if let Ok(recorder) = run.recorder.lock() {
            if let Some(recorder) = recorder.as_ref() {
                recorder.merge(layer);
            }
        }
    }
}
fn record_exclusion(run: &Run, path: &Path, kind: &str, message: &str) {
    if kind == "unreadable" {
        // This must survive the bounded issue list and an absent optional index.
        run.measurement_gap.store(true, Ordering::Relaxed);
    }
    if let Ok(recorder) = run.recorder.lock() {
        if let Some(recorder) = recorder.as_ref() {
            recorder.exclude(path, kind, message);
        }
    }
}

fn scan_with_incremental(
    run: Arc<Run>,
    roots: Vec<PathBuf>,
    workers: usize,
    key: &str,
    force_full: bool,
) {
    let path = cache_file(key).map(|path| path.with_extension("index.sqlite"));
    let mut session = path.as_deref().and_then(|path| {
        incremental::Session::begin_audit(
            path,
            &format!("{INDEX_POLICY_VERSION}\n{key}"),
            &roots,
            run.allow_protected,
            configured_cache_limit() * MIB / 2,
            !force_full,
        )
        .ok()
    });
    if let Some(session) = &session {
        if !force_full && !session.has_baseline() {
            if let Ok(mut s) = run.snapshot.lock() {
                s.fallback_reason = Some(
                    "No compatible index was retained; a full metadata audit establishes it".into(),
                );
            }
        }
        *run.recorder.lock().unwrap() = Some(session.recorder());
    }
    // FSEvents cannot prove query synchronization on macOS. Every fresh result
    // therefore comes from this single complete native metadata walk.
    scan_roots_with_workers(run.clone(), roots, workers);
    if let Ok(mut target) = run.recorder.lock() {
        *target = None;
    }
    let mut reused_store_rows = false;
    if let Some(session) = &mut session {
        match session.finish_audit(&run) {
            Ok(Some(commit)) => {
                reused_store_rows = commit.diagnostics.reused_directories > 0;
                if let Ok(mut s) = run.snapshot.lock() {
                    s.fallback_reason = commit.persistence_error;
                }
            }
            Ok(None) => {
                if let Ok(mut s) = run.snapshot.lock() {
                    s.fallback_reason = Some(
                        "The complete index could not be retained; metadata was fully measured"
                            .into(),
                    );
                }
            }
            Err(_) => {
                if let Ok(mut s) = run.snapshot.lock() {
                    s.fallback_reason = Some(
                        "The reusable index is unavailable; metadata was fully measured".into(),
                    );
                }
            }
        }
    }
    if let Ok(mut s) = run.snapshot.lock() {
        s.application_inventory_complete = !run.cancelled.load(Ordering::Relaxed);
        s.status = if run.cancelled.load(Ordering::Relaxed) {
            "cancelled"
        } else {
            "complete"
        }
        .into();
        s.scan_method = if reused_store_rows {
            "validated-reuse"
        } else {
            "full"
        }
        .into();
        s.freshness = if s.status == "complete" && !run.measurement_gap.load(Ordering::Relaxed) {
            "verified"
        } else {
            "partial"
        }
        .into();
        s.coverage_complete &= s.status == "complete";
        s.updated_at_ms = now_ms();
        s.elapsed_ms = s.updated_at_ms.saturating_sub(s.started_at_ms);
        s.current_path.clear();
        if let Ok(index) = run.index.lock() {
            s.treemap_entries = treemap_projection(&s, &index, &s.entries);
        }
    }
}

fn record_large_file(p: &mut Progress, path: &Path, bytes: u64, logical: u64) {
    if p.largest.len() == MAX_LARGE_FILES
        && p.largest
            .last()
            .is_some_and(|entry| bytes <= entry.allocated_bytes)
    {
        return;
    }
    let mut row = new_entry(path, false);
    row.allocated_bytes = bytes;
    row.logical_bytes = logical;
    row.file_count = 1;
    row.complete = true;
    let position = p
        .largest
        .partition_point(|entry| entry.allocated_bytes >= bytes);
    p.largest.insert(position, row);
    p.largest.truncate(MAX_LARGE_FILES);
}

/// A producer streams direct children, so a flat folder cannot build a queue
/// proportional to its width. Known file metadata is accumulated before
/// publishing, while subtree jobs retain the original depth-first accounting.
fn queue_root(
    run: &Run,
    root: &Path,
    seen: &Mutex<HardlinkOwners>,
    sender: &std::sync::mpsc::SyncSender<ScanJob>,
    root_index: usize,
) {
    let report = |path: &Path, kind: &str, message: String| {
        record_exclusion(run, path, kind, &message);
        if let Ok(mut s) = run.snapshot.lock() {
            issue(&mut s, path, kind, message);
        }
    };
    if scope_excluded(run, root) {
        report(root, "permission-excluded", PERMISSION_MESSAGE.into());
        return;
    }
    let meta = match InventoryMeta::read(root) {
        Ok(meta) if meta.directory => meta,
        Ok(_) => {
            report(
                root,
                "unreadable",
                "Scan root is no longer a directory".into(),
            );
            return;
        }
        Err(error) => {
            report(root, "unreadable", error.to_string());
            return;
        }
    };
    if excluded(root) || meta.dataless {
        report(
            root,
            "cloud-excluded",
            "Cloud storage is excluded to avoid content downloads".into(),
        );
        return;
    }
    let mut children = match DirectoryFrame::open_verified(root, run.allow_protected, &meta) {
        Ok(children) => children,
        Err(error) => {
            report(root, "unreadable", error.to_string());
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let anchor = match children.anchor() {
        Ok(anchor) => anchor,
        Err(error) => {
            report(root, "unreadable", error.to_string());
            return;
        }
    };
    // No root counters, retained rows or inventory layers are published until
    // the opened descriptor proves the identity checked before opening it.
    if let Ok(mut s) = run.snapshot.lock() {
        s.allocated_bytes += meta.allocated;
        s.scanned_directories += 1;
        let mut row = new_entry(root, true);
        row.allocated_bytes = meta.allocated;
        row.directory_count = 1;
        if let Ok(mut index) = run.index.lock() {
            index.insert(row.path.clone(), row.clone());
        }
        s.entries.push(row);
    }
    children.inventory = inventory_layer(run, root, &meta);
    let progress = || Progress {
        root: root.to_path_buf(),
        current: root.to_path_buf(),
        ..Progress::default()
    };
    let mut files = progress();
    let mut last = Instant::now();
    while !run.cancelled.load(Ordering::Relaxed) {
        let Some(child) = children.next() else { break };
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                report(root, "unreadable", error.to_string());
                continue;
            }
        };
        if internal_cache_path(&child.path) {
            continue;
        }
        if scope_excluded(run, &child.path) {
            report(
                &child.path,
                "permission-excluded",
                PERMISSION_MESSAGE.into(),
            );
            continue;
        }
        let metadata = match children.node_metadata(&child) {
            Ok(metadata) => metadata,
            Err(error) => {
                report(&child.path, "unreadable", error.to_string());
                continue;
            }
        };
        if excluded_child(&child.path) || metadata.dataless {
            report(
                &child.path,
                "cloud-excluded",
                "Skipped cloud storage or a dataless placeholder to avoid downloading content"
                    .into(),
            );
            continue;
        }
        if metadata.directory {
            let job = ScanJob {
                root_index,
                path: child.path,
                metadata,
                root_device: meta.device,
                #[cfg(target_os = "macos")]
                parent: anchor.clone(),
            };
            if sender.send(job).is_err() {
                return;
            }
            continue;
        }
        if let Some(layer) = children.inventory.as_mut() {
            layer.observe(&child.path, &metadata);
        }
        let (bytes, logical) = attributed_bytes(&metadata, &child.path, seen);
        files.files += 1;
        files.bytes += bytes;
        files.logical += logical;
        record_file_detail(run, &mut files, &child.path, bytes, logical);
        if !metadata.symlink && (metadata.links <= 1 || bytes != 0 || logical != 0) {
            record_large_file(&mut files, &child.path, bytes, logical);
        }
        if files.files as usize >= FILE_BATCH_SIZE
            || last.elapsed() >= Duration::from_millis(PUBLISH_MS as u64)
        {
            flush(run, &mut files);
            last = Instant::now();
        }
    }
    if files.files != 0 {
        flush(run, &mut files);
    }
    merge_inventory(run, children.inventory.take());
}

#[cfg(test)]
fn scan_roots(run: Arc<Run>, roots: Vec<PathBuf>) {
    scan_roots_with_workers(run, roots, WORKERS);
}

fn scan_roots_with_workers(run: Arc<Run>, roots: Vec<PathBuf>, workers: usize) {
    let seen = Mutex::new(HardlinkOwners::new());
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ScanJob>(QUEUED_JOBS);
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..workers {
            let receiver = Arc::clone(&receiver);
            let (run, roots, seen) = (&run, &roots, &seen);
            scope.spawn(move || loop {
                let job = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let Ok(job) = job else {
                    break;
                };
                if run.cancelled.load(Ordering::Relaxed) {
                    break;
                }
                scan_queued_subtree(run, &job, &roots[job.root_index], seen, true);
            });
        }
        // The producers can stop even when workers leave early on cancellation.
        drop(receiver);
        for (index, root) in roots.iter().enumerate() {
            let (run, seen) = (&run, &seen);
            if roots.len() > 1 && root == Path::new("/Applications") {
                let (apps, jobs) = std::sync::mpsc::sync_channel::<ScanJob>(QUEUED_JOBS);
                scope.spawn(move || queue_root(run, root, seen, &apps, index));
                scope.spawn(move || {
                    for job in jobs {
                        if run.cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        scan_queued_subtree(run, &job, root, seen, true);
                    }
                });
            } else {
                let sender = sender.clone();
                scope.spawn(move || queue_root(run, root, seen, &sender, index));
            }
        }
    });
    normalize_hardlink_ownership(&run, &seen, &roots);
    if let Ok(mut s) = run.snapshot.lock() {
        let complete = !coverage_issues(&run, &s) && !run.cancelled.load(Ordering::Relaxed);
        for root in &roots {
            if let Some(row) = s.entries.iter_mut().find(|e| e.path == string(root)) {
                row.complete = complete;
            }
        }
        s.updated_at_ms = now_ms();
        s.elapsed_ms = s.updated_at_ms.saturating_sub(s.started_at_ms);
        s.current_path.clear();
        s.entries
            .sort_by_key(|e| std::cmp::Reverse(e.allocated_bytes));
    }
    // Reading Info.plist never reads app executables or user documents. Cancellable between apps.
    let applications = application_attribution(&run);
    if let Ok(mut s) = run.snapshot.lock() {
        s.applications = applications;
        s.application_inventory_complete = !run.cancelled.load(Ordering::Relaxed);
        s.status = if run.cancelled.load(Ordering::Relaxed) {
            "cancelled"
        } else if run.managed {
            "running"
        } else {
            "complete"
        }
        .into();
        s.coverage_complete = !run.cancelled.load(Ordering::Relaxed) && !coverage_issues(&run, &s);
        if !run.managed {
            s.freshness = if s.status == "complete" && !run.measurement_gap.load(Ordering::Relaxed)
            {
                "verified"
            } else {
                "partial"
            }
            .into();
        }
        if let Ok(mut index) = run.index.lock() {
            for root in &roots {
                if let Some(row) = s.entries.iter().find(|e| e.path == string(root)) {
                    index.insert(row.path.clone(), row.clone());
                }
            }
        }
        run.index_complete.store(
            !run.cancelled.load(Ordering::Relaxed) && !run.index_limited.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        s.updated_at_ms = now_ms();
        s.elapsed_ms = s.updated_at_ms.saturating_sub(s.started_at_ms);
        if !run.managed {
            if let Ok(index) = run.index.lock() {
                s.treemap_entries = treemap_projection(&s, &index, &s.entries);
            }
        }
    }
}
fn bundle_id(path: &Path) -> Option<String> {
    let contents = path.join("Contents");
    let plist = contents.join("Info.plist");
    for component in [path, contents.as_path(), plist.as_path()] {
        let metadata = fs::symlink_metadata(component).ok()?;
        if metadata.file_type().is_symlink() || placeholder(&metadata) {
            return None;
        }
    }
    if fs::symlink_metadata(&plist).ok()?.len() > 16 * 1024 * 1024 {
        return None;
    }
    let output = crate::processes::bounded_output(
        Command::new("/usr/bin/plutil").args([
            "-extract",
            "CFBundleIdentifier",
            "raw",
            "-o",
            "-",
            &string(&plist),
        ]),
        Duration::from_secs(2),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!id.is_empty() && id.len() < 256 && !id.contains('/') && !id.contains("..")).then_some(id)
}
fn application_attribution(run: &Run) -> Vec<DiskApplication> {
    let Ok((entries, mut candidates)) = run
        .snapshot
        .lock()
        .map(|s| (s.entries.clone(), s.applications.clone()))
    else {
        return vec![];
    };
    let Ok(home) = home() else {
        return vec![];
    };
    let user_applications = home.join("Applications");
    let mut seen = candidates
        .iter()
        .map(|app| app.path.clone())
        .collect::<HashSet<_>>();
    // App candidates are recorded at directory close, independently of the
    // shallow summary budget. Include a scan root if it is itself a bundle.
    candidates.extend(
        entries
            .iter()
            .filter(|entry| {
                entry.kind == "application" && primary_application(Path::new(&entry.path))
            })
            .filter(|entry| seen.insert(entry.path.clone()))
            .map(DiskApplication::from),
    );
    let (mut installed, mut copies): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|app| installed_application(Path::new(&app.path), Some(&user_applications)));
    installed.sort_by_key(|app| std::cmp::Reverse(app.bundle_bytes));
    copies.sort_by_key(|app| std::cmp::Reverse(app.bundle_bytes));
    let measured = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.allocated_bytes))
        .collect::<HashMap<_, _>>();
    let mut result = vec![];
    let candidates = installed
        .into_iter()
        .take(MAX_APPLICATIONS)
        .map(|app| (app, true))
        .chain(
            copies
                .into_iter()
                .take(MAX_APPLICATIONS)
                .map(|app| (app, false)),
        );
    for (mut app, installed) in candidates {
        if run.cancelled.load(Ordering::Relaxed) {
            break;
        }
        if installed {
            app.bundle_id = bundle_id(Path::new(&app.path));
        }
        let mut names = vec![app.name.as_str()];
        if let Some(id) = &app.bundle_id {
            names.push(id);
        }
        names.sort();
        names.dedup();
        for dirname in names {
            for group in ["Application Support", "Containers"] {
                let associated_path = string(&home.join("Library").join(group).join(dirname));
                if let Some(&bytes) = measured.get(associated_path.as_str()) {
                    app.support_bytes += bytes;
                    app.associated_paths.push(associated_path);
                }
            }
        }
        result.push(app);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let p = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "macdisk-{}-{}-{unique}",
                std::process::id(),
                now_ms()
            ));
        fs::create_dir_all(p.join("nested/child")).unwrap();
        fs::write(p.join("nested/child/data"), vec![1u8; 16384]).unwrap();
        p
    }
    pub(super) fn fixture_run(root: &Path) -> Arc<Run> {
        let t = now_ms();
        Arc::new(Run {
            allow_protected: true,
            snapshot: Mutex::new(DiskScan {
                scan_id: "fixture".into(),
                status: "running".into(),
                roots: vec![string(root)],
                started_at_ms: t,
                updated_at_ms: t,
                elapsed_ms: 0,
                scanned_files: 0,
                scanned_directories: 0,
                allocated_bytes: 0,
                logical_bytes: 0,
                current_path: String::new(),
                entries: vec![],
                treemap_entries: vec![],
                applications: vec![],
                application_copies_truncated: false,
                application_inventory_complete: false,
                issues: vec![],
                issue_count: 0,
                cache_hit: false,
                cache_age_ms: 0,
                coverage_complete: false,
                scope_note: scope_note(),
                source_roots: None,
                view_path: None,
                view_detail_complete: false,
                index_policy_version: INDEX_POLICY_VERSION,
                source_scan_id: None,
                scan_method: "full".into(),
                freshness: "updating".into(),
                history_save_failed: false,
                fallback_reason: None,
            }),
            cancelled: AtomicBool::new(false),
            measurement_gap: AtomicBool::new(false),
            index: Mutex::new(HashMap::new()),
            file_details: Mutex::new(FileDetails::default()),
            file_details_full: AtomicBool::new(false),
            file_detail_limit_mib: configured_cache_limit(),
            index_complete: AtomicBool::new(false),
            index_budget: Mutex::new(IndexBudget::default()),
            index_limited: AtomicBool::new(false),
            shallow_limited: AtomicBool::new(false),
            finalization: Mutex::new(()),
            finalized: AtomicBool::new(false),
            managed: false,
            recorder: Mutex::new(None),
        })
    }
    fn run_fixture(root: &Path) -> DiskScan {
        let run = fixture_run(root);
        scan_roots(run.clone(), vec![root.to_path_buf()]);
        let snapshot = run.snapshot.lock().unwrap().clone();
        snapshot
    }
    fn treemap_test_row(path: &str, bytes: u64, directory: bool) -> DiskEntry {
        let mut row = new_entry(Path::new(path), directory);
        row.allocated_bytes = bytes;
        row.complete = true;
        row
    }
    #[test]
    fn treemap_projection_is_largest_first_bounded_connected_and_physical() {
        let root = "/treemap-fixture".to_string();
        let base = vec![treemap_test_row(&root, 1_000_000, true)];
        let mut candidates = Vec::new();
        for n in 0..300 {
            candidates.push(treemap_test_row(&format!("{root}/d{n:03}"), n + 1, true));
        }
        candidates.extend([
            treemap_test_row(&format!("{root}/missing/leaf"), 900_000, true),
            treemap_test_row(&format!("{root}/chain"), 800_000, true),
            treemap_test_row(&format!("{root}/chain/second"), 700_000, true),
            treemap_test_row(&format!("{root}/chain/second/third"), 600_000, true),
            treemap_test_row(&format!("{root}/chain/second/third/fourth"), 500_000, true),
            treemap_test_row(&format!("{root}/zero"), 0, true),
        ]);
        let selected = treemap_extra_rows(std::slice::from_ref(&root), &base, candidates.iter());
        assert_eq!(selected.len(), MAX_TREEMAP_ROWS);
        assert!(selected.iter().map(entry_bytes).sum::<usize>() <= MAX_TREEMAP_BYTES);
        assert_eq!(selected[0].path, format!("{root}/chain"));
        assert!(selected.iter().any(|row| row.path.ends_with("/third")));
        assert!(!selected.iter().any(|row| row.path.contains("missing")
            || row.path.ends_with("fourth")
            || row.path.ends_with("zero")));
        assert!(selected.iter().any(|row| row.path.ends_with("/d299")));
        assert!(!selected.iter().any(|row| row.path.ends_with("/d000")));
        let expected: Vec<_> = selected.iter().map(|row| &row.path).collect();
        let reversed =
            treemap_extra_rows(std::slice::from_ref(&root), &base, candidates.iter().rev());
        assert_eq!(
            expected,
            reversed.iter().map(|row| &row.path).collect::<Vec<_>>()
        );
        let long_rows = (0..50)
            .map(|n| treemap_test_row(&format!("{root}/{n}-{}", "x".repeat(4096)), 1, true))
            .collect::<Vec<_>>();
        let limited = treemap_extra_rows(&[root], &base, long_rows.iter());
        assert!(limited.len() < long_rows.len());
        assert!(limited.iter().map(entry_bytes).sum::<usize>() <= MAX_TREEMAP_BYTES);
    }
    #[test]
    fn treemap_source_scope_cache_and_eviction_preserve_snapshot_accounting() {
        let root = fixture();
        let third = root.join("branch/second/third");
        fs::create_dir_all(third.join("fourth")).unwrap();
        fs::write(third.join("payload"), [4u8; 8192]).unwrap();
        let first = root.join("branch/second/a.bin");
        fs::write(&first, [8u8; 32768]).unwrap();
        fs::hard_link(&first, root.join("branch/second/z.bin")).unwrap();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let source = run.snapshot.lock().unwrap().clone();
        assert!(source
            .treemap_entries
            .iter()
            .any(|row| row.path == string(&third)));
        assert!(!source
            .treemap_entries
            .iter()
            .any(|row| row.path.ends_with("fourth")));
        let source_entries = serde_json::to_value(&source.entries).unwrap();
        let view = snapshot_view(&run, &string(&root.join("branch"))).unwrap();
        let linked: Vec<_> = view
            .treemap_entries
            .iter()
            .filter(|row| row.path.ends_with("a.bin") || row.path.ends_with("z.bin"))
            .collect();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].path, string(&first));
        assert_eq!(
            linked[0].allocated_bytes,
            allocated(&fs::metadata(&first).unwrap())
        );
        let cache = serde_json::to_value(cache_value("treemap-fixture", &run)).unwrap();
        assert!(cache["snapshot"].get("treemapEntries").is_none());
        let restored = run_from_cache(serde_json::from_value(cache.clone()).unwrap(), true);
        fs::remove_dir_all(&root).unwrap();
        let restored_source = restored.snapshot.lock().unwrap().clone();
        assert_eq!(
            serde_json::to_value(&restored_source.entries).unwrap(),
            source_entries
        );
        assert_eq!(restored_source.allocated_bytes, source.allocated_bytes);
        assert_eq!(
            serde_json::to_value(&restored_source.treemap_entries).unwrap(),
            serde_json::to_value(&source.treemap_entries).unwrap()
        );
        let restored_view = snapshot_view(&restored, &string(&root.join("branch"))).unwrap();
        assert_eq!(restored_view.allocated_bytes, view.allocated_bytes);
        assert_eq!(
            serde_json::to_value(restored_view.treemap_entries).unwrap(),
            serde_json::to_value(view.treemap_entries).unwrap()
        );
        let mut evicted: Cache = serde_json::from_value(cache).unwrap();
        // A stale derived field is ignored when the underlying detail was evicted.
        evicted.snapshot.treemap_entries = source.treemap_entries;
        evicted.index.clear();
        evicted.file_details = FileDetails::default();
        let evicted = run_from_cache(evicted, true);
        assert!(evicted.snapshot.lock().unwrap().treemap_entries.is_empty());
    }
    #[test]
    fn treemap_extra_rows_require_terminal_source_and_preserve_partial_flags() {
        let root = Path::new("/treemap-fixture");
        let run = fixture_run(root);
        let mut snapshot = run.snapshot.lock().unwrap().clone();
        let row = treemap_test_row("/treemap-fixture/child", 100, true);
        let index = HashMap::from([(row.path.clone(), row)]);
        for (status, freshness) in [
            ("running", "updating"),
            ("cancelled", "partial"),
            ("failed", "partial"),
        ] {
            snapshot.status = status.into();
            snapshot.freshness = freshness.into();
            assert!(treemap_projection(&snapshot, &index, &[]).is_empty());
        }
        assert!(serde_json::to_value(&snapshot)
            .unwrap()
            .get("treemapEntries")
            .is_none());
        snapshot.status = "complete".into();
        snapshot.freshness = "partial".into();
        assert_eq!(treemap_projection(&snapshot, &index, &[]).len(), 1);
        let mut partial_index = index;
        partial_index.values_mut().next().unwrap().complete = false;
        assert!(!treemap_projection(&snapshot, &partial_index, &[])[0].complete);
    }
    #[test]
    fn basic_scope_policy_is_lexical_and_covers_data_aliases_and_other_users() {
        let home = Path::new("/Users/tester");
        for path in [
            "/Users/tester/Documents/never-open",
            "/Users/tester/Desktop",
            "/Users/tester/downloads",
            "/Users/tester/Library/Mail",
            "/Users/tester/Pictures",
            "/Users/tester/Music",
            "/Users/tester/Movies",
            "/Users/another/.codex",
            "/System/Volumes/Data/Users/tester/Documents",
            "/system/volumes/data/users/TESTER/LIBRARY/Containers",
            "/System/Volumes/Data/Library",
            "/Volumes/External",
            "/Library/Application Support",
            "/somewhere/archive.photoslibrary",
            "/private/var/db",
        ] {
            assert!(protected_path_with_home(Path::new(path), home), "{path}");
        }
        for path in [
            "/Applications",
            "/Users/tester",
            "/Users/tester/.codex",
            "/Users/tester/projects",
            "/System/Volumes/Data/Applications",
        ] {
            assert!(!protected_path_with_home(Path::new(path), home), "{path}");
        }
    }
    #[test]
    fn protected_selected_root_is_rejected_before_existence_or_metadata_checks() {
        let candidate = home()
            .unwrap()
            .join("Documents/nonexistent-permission-fixture");
        let error = start_disk_scan(Some(string(&candidate)), false).unwrap_err();
        assert!(error.starts_with("permission-required:"), "{error}");
        let traversal = home()
            .unwrap()
            .join(".codex/../Documents/nonexistent-permission-fixture");
        assert!(
            validated_roots_with_options(Some(string(&traversal)), false)
                .unwrap_err()
                .contains("parent traversal")
        );
    }
    #[test]
    fn full_and_basic_inventory_caches_have_distinct_keys() {
        let roots = vec![PathBuf::from("/Applications")];
        assert_ne!(cache_key(&roots, false), cache_key(&roots, true));
    }
    #[test]
    fn basic_traversal_excludes_protected_children_with_partial_coverage() {
        // Owned temporary fixture only: never access real protected user data.
        let root = home().unwrap().join(format!(
            ".macdisk-permission-fixture-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("public-fixture"), [1u8; 4096]).unwrap();
        let private = root.join("archive.photoslibrary");
        fs::create_dir(&private).unwrap();
        fs::write(private.join("must-not-inventory"), [7u8; 16384]).unwrap();
        let mut run = fixture_run(&root);
        Arc::get_mut(&mut run).unwrap().allow_protected = false;
        scan_roots(run.clone(), vec![root.clone()]);
        let snapshot = run.snapshot.lock().unwrap();
        assert_eq!(snapshot.status, "complete");
        assert!(!snapshot.coverage_complete);
        assert_eq!(
            snapshot.freshness, "verified",
            "Intentional privacy exclusions are a known scope policy"
        );
        assert_eq!(snapshot.scanned_files, 1);
        assert!(snapshot
            .issues
            .iter()
            .any(|i| i.path == string(&private) && i.kind == "permission-excluded"));
        assert!(!snapshot
            .entries
            .iter()
            .any(|e| e.path.starts_with(&string(&private))));
        drop(snapshot);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn bulk_metadata_matches_stat_and_portable_traversal() {
        let root = fixture();
        let child = root.join("nested/child/data");
        let mut directory = bulk::BulkDirectory::open(child.parent().unwrap(), None).unwrap();
        let node = directory.next(child.parent().unwrap()).unwrap().unwrap();
        let native = node
            .metadata
            .expect("APFS should return requested batch metadata");
        let portable = InventoryMeta::read(&child).unwrap();
        assert_eq!(
            (
                native.allocated,
                native.logical,
                native.device,
                native.inode,
                native.links
            ),
            (
                portable.allocated,
                portable.logical,
                portable.device,
                portable.inode,
                portable.links
            )
        );
        let native_run = fixture_run(&root);
        let portable_run = fixture_run(&root);
        scan_job(
            &native_run,
            &root,
            &root,
            &Mutex::new(HardlinkOwners::new()),
            true,
        );
        scan_job(
            &portable_run,
            &root,
            &root,
            &Mutex::new(HardlinkOwners::new()),
            false,
        );
        let a = native_run.snapshot.lock().unwrap();
        let b = portable_run.snapshot.lock().unwrap();
        assert_eq!(
            (
                a.allocated_bytes,
                a.logical_bytes,
                a.scanned_files,
                a.scanned_directories,
                a.issue_count
            ),
            (
                b.allocated_bytes,
                b.logical_bytes,
                b.scanned_files,
                b.scanned_directories,
                b.issue_count
            )
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "focused owned-fixture performance benchmark"]
    #[cfg(target_os = "macos")]
    fn benchmark_bulk_against_portable_owned_fixture() {
        let root = fixture();
        for directory in 0..32 {
            let path = root.join(format!("batch-{directory}"));
            fs::create_dir(&path).unwrap();
            for file in 0..400 {
                fs::write(path.join(format!("entry-{file}")), [1u8; 16]).unwrap();
            }
        }
        let fork_file = root.join("resource");
        fs::write(&fork_file, [1u8; 4096]).unwrap();
        fs::write(fork_file.join("..namedfork/rsrc"), [2u8; 8192]).unwrap();
        let sparse = fs::File::create(root.join("sparse")).unwrap();
        sparse.set_len(64 * 1024 * 1024).unwrap();
        let native_run = fixture_run(&root);
        let portable_run = fixture_run(&root);
        let started = Instant::now();
        scan_job(
            &native_run,
            &root,
            &root,
            &Mutex::new(HardlinkOwners::new()),
            true,
        );
        let native_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        scan_job(
            &portable_run,
            &root,
            &root,
            &Mutex::new(HardlinkOwners::new()),
            false,
        );
        let portable_ms = started.elapsed().as_secs_f64() * 1000.0;
        let a = native_run.snapshot.lock().unwrap();
        let b = portable_run.snapshot.lock().unwrap();
        assert_eq!(
            (
                a.allocated_bytes,
                a.logical_bytes,
                a.scanned_files,
                a.scanned_directories,
                a.issue_count
            ),
            (
                b.allocated_bytes,
                b.logical_bytes,
                b.scanned_files,
                b.scanned_directories,
                b.issue_count
            )
        );
        let result = serde_json::json!({"nativeMs":native_ms,"portableMs":portable_ms,"speedup":portable_ms/native_ms,"files":a.scanned_files,"directories":a.scanned_directories,"allocatedBytes":a.allocated_bytes,"logicalBytes":a.logical_bytes,"issueCount":a.issue_count,"includesSparseAndResourceFork":true,"metadataBatchBytes":65536});
        println!("{result}");
        if let Ok(path) = std::env::var("MACDISK_FIXTURE_BENCH_OUTPUT") {
            fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn native_and_portable_directory_open_reject_symlinks() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let link = root.join("link");
        symlink(root.join("nested"), &link).unwrap();
        assert!(DirectoryFrame::open(&link, true).is_err());
        assert!(DirectoryFrame::open(&link, false).is_err());
        assert!(DirectoryFrame::open(&link.join("child"), true).is_err());
        assert!(DirectoryFrame::open(&link.join("child"), false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn application_bundle_appears_before_whole_scan_finishes() {
        let root = fixture();
        let app = root.join("Applications/Demo.app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("demo"), [1u8; 4096]).unwrap();
        let run = fixture_run(&root);
        let mut p = Progress {
            user_applications: Some(root.join("Applications")),
            row_bytes: MAX_SHALLOW_BYTES,
            ..Progress::default()
        };
        let mut row = new_entry(&app, true);
        row.allocated_bytes = 4096;
        fold_directory(
            &app,
            &root,
            &mut HashMap::from([(app.clone(), row)]),
            &mut HashSet::new(),
            &mut p,
        );
        flush(&run, &mut p);
        {
            let snapshot = run.snapshot.lock().unwrap();
            assert_eq!(snapshot.status, "running");
            assert_eq!(snapshot.applications[0].bundle_bytes, 4096);
            assert!(snapshot.entries.is_empty());
        }
        assert_eq!(application_attribution(&run)[0].path, string(&app));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelled_scan_never_claims_complete_coverage() {
        let root = fixture();
        let run = fixture_run(&root);
        run.cancelled.store(true, Ordering::Relaxed);
        scan_roots(run.clone(), vec![root.clone()]);
        let s = run.snapshot.lock().unwrap();
        assert_eq!(s.status, "cancelled");
        assert!(!s.coverage_complete);
        assert_eq!(s.scanned_files, 0);
        assert!(s.entries.iter().all(|e| !e.complete));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn permission_gap_is_visible_and_does_not_abort_other_jobs() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture();
        let denied = root.join("denied");
        fs::create_dir(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
        let s = run_fixture(&root);
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(s.status, "complete");
        assert!(!s.coverage_complete);
        assert_eq!(s.freshness, "partial");
        assert!(s.issue_count > 0);
        assert_eq!(s.scanned_files, 1);
        assert!(s.issues.iter().any(|i| i.kind == "unreadable"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn unreadable_refresh_keeps_the_complete_cache_history_and_durable_generation() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let key = cache_key(std::slice::from_ref(&root), true);
        let good = fixture_run(&root);
        let good_id = unique_id(&good, "before-read-gap");
        registry().lock().unwrap().active = Some(good_id.clone());
        scan_with_incremental(good.clone(), vec![root.clone()], 2, &key, false);
        write_cache(&key, &good, false);
        let cache_path = cache_file(&key).unwrap();
        let db_path = cache_path.with_extension("index.sqlite");
        let complete_bytes = fs::read(&cache_path).unwrap();
        let index_bytes = fs::read(&db_path).unwrap();
        let history_before = history::records(&key, INDEX_POLICY_VERSION)
            .into_iter()
            .map(|r| r.summary.id)
            .collect::<Vec<_>>();
        fs::set_permissions(root.join("nested"), fs::Permissions::from_mode(0o000)).unwrap();
        let failed = fixture_run(&root);
        let failed_id = unique_id(&failed, "after-read-gap");
        registry().lock().unwrap().active = Some(failed_id);
        scan_with_incremental(failed.clone(), vec![root.clone()], 2, &key, false);
        fs::set_permissions(root.join("nested"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(failed.snapshot.lock().unwrap().freshness, "partial");
        finish_run(&key, &failed);
        assert_eq!(fs::read(&cache_path).unwrap(), complete_bytes);
        assert_eq!(fs::read(&db_path).unwrap(), index_bytes);
        assert_eq!(read_cache(&key).unwrap().snapshot.scan_id, good_id);
        assert_eq!(
            history::records(&key, INDEX_POLICY_VERSION)
                .into_iter()
                .map(|r| r.summary.id)
                .collect::<Vec<_>>(),
            history_before
        );
        assert_eq!(
            read_cache_at(&checkpoint_file(&key).unwrap(), &key, true)
                .unwrap()
                .snapshot
                .freshness,
            "partial"
        );
        fs::remove_file(db_path).unwrap();
        history_fixture_cleanup(&root, saved);
    }
    #[test]
    fn measurement_gap_is_retained_after_issue_detail_limit_without_an_index_recorder() {
        let root = fixture();
        let run = fixture_run(&root);
        for _ in 0..MAX_ISSUES + 1 {
            issue(
                &mut run.snapshot.lock().unwrap(),
                &root,
                "cloud-excluded",
                "Known scope exclusion".into(),
            );
        }
        record_exclusion(
            &run,
            &root.join("late-error"),
            "unreadable",
            "Metadata disappeared",
        );
        scan_roots(run.clone(), vec![root.clone()]);
        assert_eq!(run.snapshot.lock().unwrap().issues.len(), MAX_ISSUES);
        assert_eq!(run.snapshot.lock().unwrap().freshness, "partial");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn complete_file_rejects_explicit_partial_freshness_but_accepts_legacy_absence() {
        let root = fixture();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let key = cache_key(std::slice::from_ref(&root), true);
        let mut value = serde_json::to_value(cache_value(&key, &run)).unwrap();
        let path = root.join("freshness-cache.json");
        for freshness in ["partial", "updating"] {
            value["snapshot"]["freshness"] = freshness.into();
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(read_cache_at(&path, &key, false).is_none());
        }
        value["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("freshness");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_cache_at(&path, &key, false).is_some());
        fs::remove_dir_all(root).unwrap();
    }
    fn comparison_fixture(rows: &[(&str, u64, u64, bool)]) -> Cache {
        let run = fixture_run(Path::new("/comparison-fixture"));
        run.snapshot.lock().unwrap().status = "complete".into();
        run.snapshot.lock().unwrap().freshness = "verified".into();
        let mut cache = cache_value("comparison", &run);
        for (path, bytes, directories, complete) in rows {
            let mut row = new_entry(Path::new(path), true);
            row.allocated_bytes = *bytes;
            row.directory_count = *directories;
            row.complete = *complete;
            cache.index.insert(path.to_string(), row);
        }
        cache
    }
    fn compare_fixture(before: &Cache, after: &Cache, scope: Option<&Path>) -> DiskScanComparison {
        let mut result = DiskScanComparison {
            status: String::new(),
            refreshing: false,
            baseline_id: None,
            current_id: None,
            baseline_started_at_ms: None,
            baseline_updated_at_ms: None,
            current_started_at_ms: 0,
            current_updated_at_ms: 0,
            roots: after.snapshot.roots.clone(),
            entries: vec![],
            growth_bytes: 0,
            reduction_bytes: 0,
            net_bytes: 0,
            limited: false,
        };
        compare_archives(before, after, scope, &mut result);
        result
    }
    #[test]
    fn comparison_propagates_absence_through_new_and_deleted_deep_directories() {
        let empty = comparison_fixture(&[("/comparison-fixture", 0, 1, true)]);
        let deep = comparison_fixture(&[
            ("/comparison-fixture", 80, 4, true),
            ("/comparison-fixture/a", 80, 3, true),
            ("/comparison-fixture/a/b", 80, 2, true),
            ("/comparison-fixture/a/b/c", 80, 1, true),
        ]);
        for (before, after, growth, reduction) in [(&empty, &deep, 80, 0), (&deep, &empty, 0, 80)] {
            let result = compare_fixture(before, after, Some(Path::new("/comparison-fixture/a/b")));
            assert_eq!(
                (result.growth_bytes, result.reduction_bytes),
                (growth, reduction)
            );
            assert_eq!(result.entries.len(), 1);
            assert_eq!(result.entries[0].path, "/comparison-fixture/a/b/c");
        }
        // A retained descendant cannot bridge an omitted intermediate directory.
        let mut gap = deep.clone();
        gap.index.remove("/comparison-fixture/a/b");
        let result = compare_fixture(&empty, &gap, None);
        assert!(!result.entries.iter().any(|row| row.path.ends_with("/c")));
        let mut unreadable = empty;
        unreadable
            .index
            .get_mut("/comparison-fixture")
            .unwrap()
            .complete = false;
        assert_eq!(
            compare_fixture(&unreadable, &deep, None).status,
            "unavailable"
        );
    }

    #[test]
    fn historical_small_files_survive_top_sample_displacement_restart_and_net_reduction() {
        let root = fixture();
        for index in 0..120 {
            fs::write(root.join(format!("large-{index}")), vec![7; 65_536]).unwrap();
        }
        let before_run = fixture_run(&root);
        scan_roots(before_run.clone(), vec![root.clone()]);
        let before = cache_value("fixture", &before_run);
        fs::create_dir_all(root.join("new/a/b")).unwrap();
        let direct = root.join("tiny");
        let deep = root.join("new/a/b/tiny");
        fs::write(&direct, vec![1; 4096]).unwrap();
        fs::write(&deep, vec![2; 4096]).unwrap();
        for index in 0..3 {
            fs::remove_file(root.join(format!("large-{index}"))).unwrap();
        }
        let after_run = fixture_run(&root);
        scan_roots(after_run.clone(), vec![root.clone()]);
        let after = cache_value("fixture", &after_run);
        assert!(!after
            .snapshot
            .entries
            .iter()
            .any(|row| row.path == string(&direct)));
        // Exercise the durable format and cache->Run cloning, with the source
        // filesystem gone: historical comparison must never read it again.
        let before: Cache = serde_json::from_slice(&serde_json::to_vec(&before).unwrap()).unwrap();
        let after: Cache = serde_json::from_slice(&serde_json::to_vec(&after).unwrap()).unwrap();
        let restored = run_from_cache(after.clone(), true);
        assert_eq!(
            restored.file_details.lock().unwrap().rows.len(),
            after.file_details.rows.len()
        );
        let expected_direct = allocated(&fs::symlink_metadata(&direct).unwrap());
        let expected_deep = allocated(&fs::symlink_metadata(&deep).unwrap());
        fs::remove_dir_all(&root).unwrap();
        let result = compare_fixture(&before, &after, None);
        for (path, expected) in [(direct, expected_direct), (deep.clone(), expected_deep)] {
            let row = result
                .entries
                .iter()
                .find(|row| row.path == string(&path))
                .unwrap();
            assert_eq!(
                (row.kind.as_str(), row.before_bytes, row.after_bytes),
                ("file", 0, expected)
            );
        }
        assert!(result
            .entries
            .iter()
            .any(|row| row.path.ends_with("/large-0") && row.reduction_bytes > 0));
        assert!(result.growth_bytes >= expected_direct + expected_deep);
        assert!(result.reduction_bytes > result.growth_bytes);
        assert!(result.net_bytes < 0);
        let scoped = compare_fixture(&before, &after, deep.parent());
        assert_eq!(
            scoped
                .entries
                .iter()
                .find(|row| row.kind == "file")
                .unwrap()
                .path,
            string(&deep)
        );
    }

    #[test]
    fn historical_missing_samples_and_detail_budget_never_imply_zero() {
        let root = fixture();
        let old = root.join("nested/child/data");
        let before_run = fixture_run(&root);
        scan_roots(before_run.clone(), vec![root.clone()]);
        let mut before = cache_value("fixture", &before_run);
        fs::write(root.join("nested/child/new"), vec![1; 8192]).unwrap();
        fs::remove_file(&old).unwrap();
        let after_run = fixture_run(&root);
        scan_roots(after_run.clone(), vec![root.clone()]);
        let after = cache_value("fixture", &after_run);
        // Old archives have no file_details; their missing top sample is not
        // evidence that the now-visible file was absent.
        let mut legacy = serde_json::to_value(&before).unwrap();
        legacy.as_object_mut().unwrap().remove("file_details");
        before = serde_json::from_value(legacy).unwrap();
        before.snapshot.entries.retain(|row| row.kind != "file");
        let result = compare_fixture(&before, &after, None);
        assert_eq!(result.status, "ready");
        assert!(result.limited);
        assert!(!result.entries.iter().any(|row| row.kind == "file"));
        // Publication pressure discards optional detail before directories.
        let mut pressured = after.clone();
        pressured.file_details.bytes = 0;
        let without_detail = Cache {
            file_details: FileDetails::default(),
            ..after.clone()
        };
        let limit = serde_json::to_vec(&without_detail).unwrap().len() as u64;
        let original_directories = pressured.index.len();
        assert!(cache_bytes(&mut pressured, limit).is_some());
        assert!(pressured.file_details.rows.is_empty());
        assert_eq!(pressured.index.len(), original_directories);
        assert_eq!(pressured.index_complete, after.index_complete);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_file_directory_replacement_preserves_descendants_and_conservation() {
        let root = fixture();
        let target = root.join("replacement");
        fs::write(&target, vec![1; 65_536]).unwrap();
        let old = fixture_run(&root);
        scan_roots(old.clone(), vec![root.clone()]);
        let before = cache_value("fixture", &old);
        fs::remove_file(&target).unwrap();
        fs::create_dir_all(target.join("deep")).unwrap();
        let child = target.join("deep/tiny");
        fs::write(&child, vec![2; 8192]).unwrap();
        let new = fixture_run(&root);
        scan_roots(new.clone(), vec![root.clone()]);
        let after = cache_value("fixture", &new);
        for (a, b) in [(&before, &after), (&after, &before)] {
            let result = compare_fixture(a, b, None);
            assert!(result
                .entries
                .iter()
                .any(|row| row.path == string(&child) && row.kind == "file"));
            assert!(result
                .entries
                .iter()
                .any(|row| row.path == string(&target) && row.kind == "directory"));
            assert_eq!(
                result.net_bytes,
                comparison_net(a.snapshot.allocated_bytes, b.snapshot.allocated_bytes)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_file_detail_allowance_is_bounded_and_independent() {
        let mut details = FileDetails::default();
        for index in 0..40_000 {
            details.retain(Path::new(&format!("/detail-budget/{index}")), 1, 1, 64);
        }
        assert!(details.rows.len() < 25_000);
        assert!(details.bytes <= 4 * MIB as usize);
        let rows = details.rows.len();
        details.retain(Path::new("/detail-budget/after-exhaustion"), 1, 1, 64);
        assert_eq!(details.rows.len(), rows);
        let root = fixture();
        let run = fixture_run(&root);
        run.file_details.lock().unwrap().bytes = 16 * MIB as usize;
        scan_roots(run.clone(), vec![root.clone()]);
        assert!(run.file_details.lock().unwrap().rows.is_empty());
        assert!(run.file_details_full.load(Ordering::Relaxed));
        // Once full, traversal does not allocate pending paths or take the
        // detail lock again, even while another operation owns that lock.
        let held = run.file_details.lock().unwrap();
        let mut pending = Progress::default();
        for index in 0..1000 {
            record_file_detail(
                &run,
                &mut pending,
                Path::new(&format!("/after-full/{index}")),
                1,
                1,
            );
        }
        assert!(pending.file_details.is_empty());
        assert_eq!(pending.file_detail_bytes, 0);
        drop(held);
        assert!(run.index_complete.load(Ordering::Relaxed));
        assert!(run
            .index
            .lock()
            .unwrap()
            .contains_key(&string(&root.join("nested/child"))));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_file_details_normalize_all_hardlink_names() {
        let root = fixture();
        let canonical = root.join("a-link");
        let first = root.join("z-link");
        fs::write(&first, vec![1; 8192]).unwrap();
        fs::hard_link(&first, &canonical).unwrap();
        let run = fixture_run(&root);
        let owners = Mutex::new(HardlinkOwners::new());
        let mut progress = Progress::default();
        for path in [&first, &canonical] {
            let meta = InventoryMeta::read(path).unwrap();
            let (bytes, logical) = attributed_bytes(&meta, path, &owners);
            record_file_detail(&run, &mut progress, path, bytes, logical);
        }
        flush_file_details(&run, &mut progress);
        normalize_hardlink_ownership(&run, &owners, std::slice::from_ref(&root));
        let details = run.file_details.lock().unwrap();
        assert_eq!(details.rows[&string(&first)], (0, 0));
        assert_eq!(
            details.rows[&string(&canonical)].0,
            allocated(&fs::symlink_metadata(&canonical).unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_preserves_growth_and_reduction_without_parent_cancellation() {
        let before = comparison_fixture(&[
            ("/comparison-fixture", 200, 3, true),
            ("/comparison-fixture/a", 10, 1, true),
            ("/comparison-fixture/b", 100, 1, true),
        ]);
        let after = comparison_fixture(&[
            ("/comparison-fixture", 220, 3, true),
            ("/comparison-fixture/a", 110, 1, true),
            ("/comparison-fixture/b", 20, 1, true),
        ]);
        let result = compare_fixture(&before, &after, None);
        assert_eq!(
            (
                result.growth_bytes,
                result.reduction_bytes,
                result.net_bytes
            ),
            (100, 80, 20)
        );
        assert_eq!(result.entries.len(), 2);
        let scoped = compare_fixture(&before, &after, Some(Path::new("/comparison-fixture/b")));
        assert_eq!((scoped.growth_bytes, scoped.reduction_bytes), (0, 80));
    }
    #[test]
    fn comparison_proves_new_deleted_directories_without_zero_filling_index_gaps() {
        let before = comparison_fixture(&[
            ("/comparison-fixture", 120, 2, true),
            ("/comparison-fixture/old", 100, 1, true),
        ]);
        let after = comparison_fixture(&[
            ("/comparison-fixture", 70, 2, true),
            ("/comparison-fixture/new", 50, 1, true),
        ]);
        let result = compare_fixture(&before, &after, None);
        assert_eq!((result.growth_bytes, result.reduction_bytes), (50, 100));
        let mut missing_index = after.clone();
        missing_index.index.remove("/comparison-fixture/new");
        // This retained row is not evidence that the other snapshot lacks it:
        // current root.directory_count proves its direct index is incomplete.
        let result = compare_fixture(&before, &missing_index, None);
        assert!(!result.entries.iter().any(|row| row.path.ends_with("/old")));
        assert_eq!((result.growth_bytes, result.reduction_bytes), (0, 50));
        assert!(result.limited);
    }
    #[test]
    fn cloud_boundary_check_keeps_nested_exclusions_and_ordinary_names() {
        for path in [
            "/fixture/Library/CloudStorage",
            "/fixture/a/Library/Mobile Documents",
            "/fixture/CloudStorage",
            "/fixture/Library/ordinary",
            "/fixture/a/CloudStorage/file",
        ] {
            let path = Path::new(path);
            assert_eq!(excluded_child(path), excluded(path));
        }
        let root = fixture();
        let cloud = root.join("Library/CloudStorage/nested");
        fs::create_dir_all(&cloud).unwrap();
        fs::write(cloud.join("excluded"), [1; 4096]).unwrap();
        let scan = fixture_run(&cloud);
        scan_roots(scan.clone(), vec![cloud]);
        let snapshot = scan.snapshot.lock().unwrap();
        assert_eq!(
            (snapshot.scanned_files, snapshot.scanned_directories),
            (0, 0)
        );
        assert!(snapshot
            .issues
            .iter()
            .any(|issue| issue.kind == "cloud-excluded"));
        drop(snapshot);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn running_comparison_keeps_only_archived_pair_and_invalidates_on_eviction_or_completion() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let key = cache_key(std::slice::from_ref(&root), true);
        let first = fixture_run(&root);
        let first_id = unique_id(&first, "refresh-pair-first");
        scan_roots(first.clone(), vec![root.clone()]);
        let saved_active = registry().lock().unwrap().active.replace(first_id);
        write_cache(&key, &first, false);
        fs::write(root.join("growth"), [1; 8192]).unwrap();
        let second = fixture_run(&root);
        let second_id = unique_id(&second, "refresh-pair-second");
        scan_roots(second.clone(), vec![root.clone()]);
        registry().lock().unwrap().active = Some(second_id);
        write_cache(&key, &second, false);
        let third = fixture_run(&root);
        let third_id = unique_id(&third, "refresh-pair-running");
        // Partial counters must never enter the comparison result.
        third.snapshot.lock().unwrap().allocated_bytes = u64::MAX / 2;
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(third_id.clone(), third.clone());
            reg.active = Some(third_id.clone());
        }
        let result = get_disk_scan_comparison(third_id.clone(), Some(string(&root))).unwrap();
        assert_eq!(result.status, "ready");
        assert!(result.refreshing);
        assert_eq!(
            result.current_started_at_ms,
            second.snapshot.lock().unwrap().started_at_ms
        );
        assert_eq!(result.growth_bytes, 8192);
        assert_eq!(
            get_disk_scan_comparison(third_id.clone(), Some(string(&root)))
                .unwrap()
                .growth_bytes,
            8192
        );
        // Eviction invalidates an otherwise memoized result.
        fs::remove_file(previous_cache_file(&key).unwrap()).unwrap();
        assert_eq!(
            get_disk_scan_comparison(third_id.clone(), Some(string(&root)))
                .unwrap()
                .status,
            "scanning"
        );
        let mut value = cache_value(&key, &first);
        fs::write(
            previous_cache_file(&key).unwrap(),
            cache_bytes(&mut value, MAX_CACHE_FILE_BYTES).unwrap(),
        )
        .unwrap();
        // A single complete archive is not a usable growth pair.
        fs::remove_file(cache_file(&key).unwrap()).unwrap();
        assert_eq!(
            get_disk_scan_comparison(third_id.clone(), Some(string(&root)))
                .unwrap()
                .status,
            "scanning"
        );
        let mut value = cache_value(&key, &second);
        fs::write(
            cache_file(&key).unwrap(),
            cache_bytes(&mut value, MAX_CACHE_FILE_BYTES).unwrap(),
        )
        .unwrap();
        // Once the successor completes, its archived comparison replaces the old pair.
        third.snapshot.lock().unwrap().allocated_bytes = 0;
        scan_roots(third.clone(), vec![root.clone()]);
        let fresh = get_disk_scan_comparison(third_id.clone(), Some(string(&root))).unwrap();
        assert_eq!(fresh.status, "ready");
        assert!(!fresh.refreshing);
        assert_eq!(
            fresh.current_started_at_ms,
            third.snapshot.lock().unwrap().started_at_ms
        );
        assert_eq!(fresh.growth_bytes, 0);
        let mut reg = registry().lock().unwrap();
        reg.active = saved_active;
        reg.runs.remove(&third_id);
        reg.cached.remove(&key);
        reg.comparisons.retain(|entry| entry.scan_id != third_id);
        drop(reg);
        for file in [
            cache_file(&key),
            previous_cache_file(&key),
            checkpoint_file(&key),
        ]
        .into_iter()
        .flatten()
        {
            let _ = fs::remove_file(file);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_generation_tracks_replacement_not_recency_and_rejects_raced_retention() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let key = format!("comparison-generation-{}-{}", std::process::id(), now_ms());
        let current = cache_file(&key).unwrap();
        let previous = previous_cache_file(&key).unwrap();
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, [1, 2]).unwrap();
        fs::write(&previous, [3, 4]).unwrap();
        let generation = comparison_generation(&key);
        assert!(generation.is_some());
        touch_cache(&current);
        touch_cache(&previous);
        #[cfg(unix)]
        assert!(generation == comparison_generation(&key));
        let replacement = current.with_extension("replace-tmp");
        fs::write(&replacement, [5, 6]).unwrap();
        fs::rename(replacement, &current).unwrap();
        assert!(generation != comparison_generation(&key));
        let cache = comparison_fixture(&[("/comparison-fixture", 100, 1, true)]);
        let result = compare_fixture(&cache, &cache, None);
        retain_comparison("generation-race", None, &key, generation, &result);
        assert!(!registry()
            .lock()
            .unwrap()
            .comparisons
            .iter()
            .any(|entry| entry.scan_id == "generation-race"));
        fs::remove_file(current).unwrap();
        fs::remove_file(previous).unwrap();
    }

    #[test]
    fn comparison_memoization_has_bounded_scope_and_memory_budgets() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let saved = std::mem::take(&mut registry().lock().unwrap().comparisons);
        let cache = comparison_fixture(&[("/comparison-fixture", 100, 1, true)]);
        let result = compare_fixture(&cache, &cache, None);
        for index in 0..MAX_COMPARISON_SCOPES * 3 {
            retain_comparison(
                "budget-fixture",
                Some(format!("/scope-{index}")),
                "fixture",
                None,
                &result,
            );
        }
        let mut reg = registry().lock().unwrap();
        assert_eq!(reg.comparisons.len(), MAX_COMPARISON_SCOPES);
        assert!(
            reg.comparisons
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                <= MAX_COMPARISON_MEMORY_BYTES
        );
        reg.comparisons = saved;
    }

    #[test]
    fn comparison_skips_unreadable_ancestors_but_keeps_known_common_subtrees() {
        let before = comparison_fixture(&[
            ("/comparison-fixture", 100, 3, false),
            ("/comparison-fixture/known", 30, 1, true),
            ("/comparison-fixture/private", 70, 1, true),
        ]);
        let after = comparison_fixture(&[
            ("/comparison-fixture", 50, 2, false),
            ("/comparison-fixture/known", 50, 1, true),
        ]);
        let result = compare_fixture(&before, &after, None);
        assert_eq!((result.growth_bytes, result.reduction_bytes), (20, 0));
        assert!(result.limited);
        let unavailable = compare_fixture(
            &before,
            &after,
            Some(Path::new("/comparison-fixture/private")),
        );
        assert_eq!(unavailable.status, "unavailable");
    }
    #[test]
    fn comparison_sample_churn_is_aggregate_and_matched_files_preserve_opposite_changes() {
        let mut before = comparison_fixture(&[("/comparison-fixture", 300, 1, true)]);
        let mut after = comparison_fixture(&[("/comparison-fixture", 320, 1, true)]);
        for (cache, rows) in [
            (
                &mut before,
                vec![("growing", 10), ("shrinking", 100), ("old-sample", 90)],
            ),
            (
                &mut after,
                vec![("growing", 110), ("shrinking", 20), ("new-sample", 90)],
            ),
        ] {
            for (name, bytes) in rows {
                let mut row = new_entry(&Path::new("/comparison-fixture").join(name), false);
                row.allocated_bytes = bytes;
                row.complete = true;
                cache.snapshot.entries.push(row);
            }
        }
        let result = compare_fixture(&before, &after, None);
        assert_eq!((result.growth_bytes, result.reduction_bytes), (100, 80));
        assert!(result
            .entries
            .iter()
            .all(|row| !row.path.contains("sample")));
        assert!(result.entries.iter().all(|row| row.kind == "file"));
    }
    #[test]
    fn comparison_nearest_common_forest_handles_missing_intermediates_and_prefix_siblings() {
        let before = comparison_fixture(&[
            ("/comparison-fixture", 120, 5, true),
            ("/comparison-fixture/a", 80, 3, true),
            ("/comparison-fixture/a/deep/leaf", 20, 1, true),
            ("/comparison-fixture/a-sibling", 40, 1, true),
        ]);
        let after = comparison_fixture(&[
            ("/comparison-fixture", 140, 5, true),
            ("/comparison-fixture/a", 100, 3, true),
            ("/comparison-fixture/a/deep/leaf", 40, 1, true),
            ("/comparison-fixture/a-sibling", 40, 1, true),
        ]);
        let result = compare_fixture(&before, &after, None);
        assert_eq!((result.growth_bytes, result.reduction_bytes), (20, 0));
        assert_eq!(result.entries.len(), 1);
        assert!(result.entries[0].path.ends_with("/leaf"));
    }
    #[test]
    fn comparison_rejects_incompatible_or_legacy_scan_index_policies() {
        let current = comparison_fixture(&[("/comparison-fixture", 100, 1, true)]);
        let mut baseline = current.clone();
        baseline.snapshot.index_policy_version = 0;
        assert_eq!(
            compare_fixture(&baseline, &current, None).status,
            "unavailable"
        );
        baseline.snapshot.index_policy_version = INDEX_POLICY_VERSION + 1;
        assert_eq!(
            compare_fixture(&baseline, &current, None).status,
            "unavailable"
        );
    }
    #[test]
    fn comparison_baseline_rotation_survives_restart_and_ignores_same_id_checkpoints_and_stale_writers(
    ) {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let key = cache_key(std::slice::from_ref(&root), true);
        let first = fixture_run(&root);
        let first_id = unique_id(&first, "baseline-first");
        scan_roots(first.clone(), vec![root.clone()]);
        let second = fixture_run(&root);
        let second_id = unique_id(&second, "baseline-second");
        scan_roots(second.clone(), vec![root.clone()]);
        let previous_active = registry().lock().unwrap().active.replace(first_id.clone());
        write_cache(&key, &first, false);
        assert!(!previous_cache_file(&key).unwrap().exists());
        registry().lock().unwrap().active = Some(second_id.clone());
        write_cache(&key, &second, true);
        assert!(!previous_cache_file(&key).unwrap().exists());
        write_cache(&key, &second, false);
        let previous = previous_cache_file(&key).unwrap();
        assert_eq!(
            read_cache_at(&previous, &key, false)
                .unwrap()
                .snapshot
                .scan_id,
            first_id
        );
        // Registry-free disk reload simulates a restarted sidecar.
        registry().lock().unwrap().cached.remove(&key);
        let restored = run_from_cache(
            read_cache_at(&cache_file(&key).unwrap(), &key, false).unwrap(),
            true,
        );
        assert_eq!(restored.snapshot.lock().unwrap().scan_id, second_id);
        write_cache(&key, &second, false);
        write_cache(&key, &second, true);
        write_cache(&key, &first, false);
        assert_eq!(
            read_cache_at(&previous, &key, false)
                .unwrap()
                .snapshot
                .scan_id,
            first_id
        );
        assert_eq!(
            read_cache_at(&cache_file(&key).unwrap(), &key, false)
                .unwrap()
                .snapshot
                .scan_id,
            second_id
        );
        assert!(cache_files(previous.parent().unwrap(), false)
            .unwrap()
            .iter()
            .any(|file| file.path == previous));
        fs::remove_dir_all(&root).unwrap();
        registry()
            .lock()
            .unwrap()
            .runs
            .insert(second_id.clone(), restored.clone());
        let from_archives =
            get_disk_scan_comparison(second_id.clone(), Some(string(&root))).unwrap();
        assert_eq!(from_archives.status, "ready");
        assert_eq!(
            from_archives.baseline_started_at_ms,
            Some(first.snapshot.lock().unwrap().started_at_ms)
        );
        assert_eq!(from_archives.growth_bytes, 0);
        restored.snapshot.lock().unwrap().status = "running".into();
        assert_eq!(
            get_disk_scan_comparison(second_id.clone(), None)
                .unwrap()
                .status,
            "scanning"
        );
        restored.snapshot.lock().unwrap().status = "cancelled".into();
        let after_stop = get_disk_scan_comparison(second_id.clone(), None).unwrap();
        assert_eq!(after_stop.status, "ready");
        assert_eq!(after_stop.current_id, from_archives.current_id);
        assert_eq!(after_stop.baseline_id, from_archives.baseline_id);
        restored.snapshot.lock().unwrap().status = "complete".into();
        restored.snapshot.lock().unwrap().freshness = "verified".into();
        fs::remove_file(&previous).unwrap();
        assert_eq!(
            get_disk_scan_comparison(second_id.clone(), None)
                .unwrap()
                .status,
            "no-baseline"
        );
        let mut registry = registry().lock().unwrap();
        registry.active = previous_active;
        registry.cached.remove(&key);
        registry.runs.remove(&second_id);
        drop(registry);
        for path in [
            cache_file(&key),
            previous_cache_file(&key),
            checkpoint_file(&key),
        ]
        .into_iter()
        .flatten()
        {
            let _ = fs::remove_file(path);
        }
    }
    #[test]
    fn accounts_allocated_bytes_and_nested_rows() {
        let root = fixture();
        let s = run_fixture(&root);
        assert_eq!(s.status, "complete");
        assert!(s.coverage_complete);
        assert_eq!(s.scanned_files, 1);
        assert!(
            s.allocated_bytes >= allocated(&fs::metadata(root.join("nested/child/data")).unwrap())
        );
        let row = s
            .entries
            .iter()
            .find(|e| e.path == string(&root.join("nested")))
            .unwrap();
        assert_eq!(row.logical_bytes, 16384);
        assert!(row.complete);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn never_follows_symlink_and_deduplicates_hardlinks() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let original = root.join("nested/child/data");
        fs::hard_link(&original, root.join("alias")).unwrap();
        symlink("/", root.join("escape")).unwrap();
        let s = run_fixture(&root);
        assert_eq!(s.scanned_files, 3);
        assert_eq!(s.logical_bytes, 16384 + 1);
        assert!(s.scanned_directories < 10);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn root_totals_remain_exact_when_largest_files_are_bounded() {
        let root = fixture();
        for n in 0..125 {
            fs::write(root.join(format!("file-{n}")), vec![1u8; 32]).unwrap();
        }
        let s = run_fixture(&root);
        assert_eq!(s.scanned_files, 126);
        assert_eq!(
            s.entries.iter().filter(|e| e.kind == "file").count(),
            MAX_LARGE_FILES
        );
        let summary = s.entries.iter().find(|e| e.path == string(&root)).unwrap();
        assert_eq!(summary.allocated_bytes, s.allocated_bytes);
        assert_eq!(summary.logical_bytes, s.logical_bytes);
        assert_eq!(summary.file_count, 126);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_outside_scope_and_relative_paths() {
        assert!(validated_roots(Some("/etc".into())).is_err());
        assert!(validated_roots(Some("relative".into())).is_err());
    }
    #[test]
    fn issue_details_are_bounded_but_count_is_exact() {
        let root = fixture();
        let mut s = run_fixture(&root);
        for _ in 0..123 {
            issue(&mut s, &root, "test", "gap".into());
        }
        assert_eq!(s.issue_count, 123);
        assert_eq!(s.issues.len(), MAX_ISSUES);
        fs::remove_dir_all(root).unwrap();
    }
    // Cache fixtures share registry, history catalog state and header-read counters.
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());
    fn cache_value(key: &str, run: &Run) -> Cache {
        let mut snapshot = run.snapshot.lock().unwrap().clone();
        snapshot.treemap_entries.clear();
        Cache {
            version: CACHE_VERSION,
            key: key.into(),
            snapshot,
            index: run.index.lock().unwrap().clone(),
            index_complete: run.index_complete.load(Ordering::Relaxed),
            file_details: run.file_details.lock().unwrap().clone(),
        }
    }
    fn unique_id(run: &Run, suffix: &str) -> String {
        let mut s = run.snapshot.lock().unwrap();
        s.scan_id = format!("fixture-{}-{}-{suffix}", std::process::id(), now_ms());
        s.scan_id.clone()
    }
    fn publish_history_fixture(root: &Path, label: &str, time: u64) -> (Arc<Run>, String) {
        let run = fixture_run(root);
        let id = unique_id(&run, label);
        run.snapshot.lock().unwrap().started_at_ms = time;
        scan_roots(run.clone(), vec![root.to_path_buf()]);
        run.snapshot.lock().unwrap().updated_at_ms = time + 20;
        let key = cache_key(&[root.to_path_buf()], true);
        {
            let mut reg = registry().lock().unwrap();
            reg.active = Some(id.clone());
            reg.runs.insert(id.clone(), run.clone());
        }
        write_cache(&key, &run, false);
        run.finalized.store(true, Ordering::Relaxed);
        (run, id)
    }
    fn history_fixture_cleanup(root: &Path, saved_active: Option<String>) {
        for protected in [false, true] {
            let key = cache_key(&[root.to_path_buf()], protected);
            for policy in [INDEX_POLICY_VERSION, INDEX_POLICY_VERSION + 1] {
                for row in history::records(&key, policy) {
                    if let Some(path) = row.path() {
                        let _ = fs::remove_file(path);
                    }
                }
            }
            for path in [
                cache_file(&key),
                previous_cache_file(&key),
                checkpoint_file(&key),
            ]
            .into_iter()
            .flatten()
            {
                let _ = fs::remove_file(path);
            }
            registry()
                .lock()
                .unwrap()
                .comparisons
                .retain(|entry| entry.key != key);
            registry().lock().unwrap().cached.remove(&key);
        }
        let mut reg = registry().lock().unwrap();
        reg.active = saved_active;
        reg.runs
            .retain(|_, run| run.snapshot.lock().unwrap().roots != vec![string(root)]);
        drop(reg);
        fs::remove_dir_all(root).unwrap();
        history::reset_memory();
    }
    fn local_history_noon(days_ago: i64) -> u64 {
        use chrono::TimeZone;
        let date = chrono::Local::now().date_naive() - chrono::Duration::days(days_ago);
        chrono::Local
            .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis() as u64
    }
    #[test]
    fn optional_index_and_checkpoint_pressure_keep_complete_history() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let (_, previous_id) = publish_history_fixture(&root, "pressure-previous", 100);
        let (_, current_id) = publish_history_fixture(&root, "pressure-current", 200);
        let key = cache_key(std::slice::from_ref(&root), true);
        let current = cache_file(&key).unwrap();
        let previous = previous_cache_file(&key).unwrap();
        let directory = current.parent().unwrap();
        {
            let _storage = cache_storage_lock().lock().unwrap();
            let full_budget = configured_cache_limit() * MIB - CATALOG_RESERVE_BYTES;
            assert!(reserve_incremental_bytes(
                &current.with_extension("index.sqlite"),
                full_budget
            )
            .is_err());
            let checkpoint = checkpoint_file(&key).unwrap();
            assert!(trim_cache_directory_with_policy(
                directory,
                Some(&checkpoint),
                0,
                usize::MAX,
                false,
                &[]
            )
            .is_err());
            assert_eq!(
                read_cache_at(&current, &key, false)
                    .unwrap()
                    .snapshot
                    .scan_id,
                current_id
            );
            assert_eq!(
                read_cache_at(&previous, &key, false)
                    .unwrap()
                    .snapshot
                    .scan_id,
                previous_id
            );
        }
        history_fixture_cleanup(&root, saved);
    }

    #[test]
    fn failed_history_publication_reports_unsaved_and_retry_rotates_old_endpoint() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let (_, original_id) = publish_history_fixture(&root, "write-original", 100);
        let original = list_disk_scan_history(original_id.clone())
            .unwrap()
            .current
            .unwrap();
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous = previous_cache_file(&key).unwrap();
        // A directory here forces the old-endpoint archival rename to fail.
        fs::create_dir(&previous).unwrap();
        fs::write(root.join("new-data"), [1; 4096]).unwrap();
        let failed = fixture_run(&root);
        let failed_id = unique_id(&failed, "write-failed");
        scan_roots(failed.clone(), vec![root.clone()]);
        {
            let mut reg = registry().lock().unwrap();
            reg.active = Some(failed_id.clone());
            reg.runs.insert(failed_id.clone(), failed);
        }
        let measured = get_disk_scan(failed_id.clone()).unwrap();
        assert_eq!(measured.status, "complete");
        assert_eq!(measured.freshness, "verified");
        assert_eq!(
            serde_json::to_value(measured).unwrap()["historySaveFailed"],
            true
        );
        assert_eq!(
            list_disk_scan_history(failed_id).unwrap().current,
            Some(original.clone())
        );
        assert_eq!(read_cache(&key).unwrap().snapshot.scan_id, original_id);

        fs::remove_dir(&previous).unwrap();
        let (_, retry_id) = publish_history_fixture(&root, "write-retry", 300);
        assert!(!get_disk_scan(retry_id.clone()).unwrap().history_save_failed);
        let history = list_disk_scan_history(retry_id.clone()).unwrap();
        assert_ne!(history.current, Some(original.clone()));
        assert!(history.snapshots.contains(&original));
        assert_eq!(
            get_disk_scan_comparison(retry_id, None)
                .unwrap()
                .growth_bytes,
            allocated(&fs::metadata(root.join("new-data")).unwrap())
        );
        history_fixture_cleanup(&root, saved);
    }

    #[test]
    fn stopped_and_failed_refresh_can_still_compare_the_saved_pair() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let (_, first_id) = publish_history_fixture(&root, "saved-start", 100);
        let baseline = list_disk_scan_history(first_id).unwrap().current.unwrap();
        fs::write(root.join("growth-data"), [1; 4096]).unwrap();
        let (_, second_id) = publish_history_fixture(&root, "saved-end", 200);
        let pair =
            get_disk_scan_comparison_with_baseline(second_id, None, Some(baseline.id.clone()))
                .unwrap();
        assert_eq!(pair.status, "ready");
        for status in ["cancelled", "failed"] {
            let attempt = fixture_run(&root);
            let attempt_id = unique_id(&attempt, status);
            {
                let mut snapshot = attempt.snapshot.lock().unwrap();
                snapshot.status = status.into();
                snapshot.freshness = "partial".into();
            }
            attempt.finalized.store(true, Ordering::Release);
            registry()
                .lock()
                .unwrap()
                .runs
                .insert(attempt_id.clone(), attempt);
            let comparison =
                get_disk_scan_comparison_with_baseline(attempt_id, None, Some(baseline.id.clone()))
                    .unwrap();
            assert_eq!(comparison.status, "ready");
            assert_eq!(comparison.current_id, pair.current_id);
            assert_eq!(comparison.baseline_id, pair.baseline_id);
            assert_eq!(comparison.growth_bytes, pair.growth_bytes);
            assert!(!comparison.refreshing);
        }
        history_fixture_cleanup(&root, saved);
    }
    #[test]
    fn history_keeps_cross_day_endpoints_instead_of_summing_intermediate_growth() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let a = root.join("a");
        let b = root.join("b");
        fs::write(&a, vec![1; 40 * 1024]).unwrap();
        fs::write(&b, vec![2; 60 * 1024]).unwrap();
        let a_before = allocated(&fs::metadata(&a).unwrap());
        let b_before = allocated(&fs::metadata(&b).unwrap());
        let (_, first_id) =
            publish_history_fixture(&root, "history-yesterday", local_history_noon(1));
        let first = list_disk_scan_history(first_id).unwrap();
        let baseline = first.current.unwrap();
        assert!(first.snapshots.is_empty());
        let mut latest = String::new();
        for (index, (a_size, b_size)) in [
            (70, 60),
            (50, 60),
            (100, 40),
            (100, 40),
            (100, 40),
            (100, 40),
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(&a, vec![1; a_size * 1024]).unwrap();
            fs::write(&b, vec![2; b_size * 1024]).unwrap();
            if index == 0 {
                fs::write(root.join("transient"), vec![3; 64 * 1024]).unwrap();
            }
            if index == 1 {
                fs::remove_file(root.join("transient")).unwrap();
            }
            latest = publish_history_fixture(
                &root,
                &format!("history-today-{index}"),
                local_history_noon(0) + index as u64 * 1000,
            )
            .1;
        }
        let listing = list_disk_scan_history(latest.clone()).unwrap();
        assert!(listing
            .snapshots
            .iter()
            .any(|summary| summary.id == baseline.id));
        let result =
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(baseline.id.clone()))
                .unwrap();
        assert_eq!(result.status, "ready");
        assert_eq!(result.baseline_id.as_deref(), Some(baseline.id.as_str()));
        assert_eq!(
            result.growth_bytes,
            allocated(&fs::metadata(&a).unwrap()) - a_before
        );
        assert_eq!(
            result.reduction_bytes,
            b_before - allocated(&fs::metadata(&b).unwrap())
        );
        assert_eq!(
            result.net_bytes,
            (result.growth_bytes - result.reduction_bytes) as i64
        );
        assert!(result
            .entries
            .iter()
            .all(|row| !row.path.ends_with("transient")));
        assert_eq!(
            get_disk_scan_comparison(latest.clone(), None)
                .unwrap()
                .growth_bytes,
            0
        );
        // A real process restart reloads only validated retained metadata and yields
        // the same endpoint result, rather than summing the sequence's positives.
        history::reset_memory();
        registry().lock().unwrap().comparisons.clear();
        let reloaded = list_disk_scan_history(latest.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&listing).unwrap(),
            serde_json::to_value(&reloaded).unwrap()
        );
        let again =
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(baseline.id.clone()))
                .unwrap();
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(&again).unwrap()
        );
        let running = fixture_run(&root);
        let running_id = unique_id(&running, "history-partial-successor");
        running.snapshot.lock().unwrap().allocated_bytes = u64::MAX / 2;
        registry()
            .lock()
            .unwrap()
            .runs
            .insert(running_id.clone(), running.clone());
        let running_list = list_disk_scan_history(running_id.clone()).unwrap();
        assert_eq!(
            running_list.current.unwrap().id,
            result.current_id.clone().unwrap()
        );
        let during = get_disk_scan_comparison_with_baseline(
            running_id.clone(),
            None,
            Some(baseline.id.clone()),
        )
        .unwrap();
        assert_eq!(during.status, "ready");
        assert!(during.refreshing);
        assert_eq!(
            (during.growth_bytes, during.reduction_bytes),
            (result.growth_bytes, result.reduction_bytes)
        );
        assert_eq!(during.current_id, result.current_id);
        running.snapshot.lock().unwrap().status = "cancelled".into();
        running.finalized.store(true, Ordering::Relaxed);
        assert_eq!(
            get_disk_scan_comparison_with_baseline(running_id, None, Some(baseline.id))
                .unwrap()
                .status,
            "ready"
        );
        history_fixture_cleanup(&root, saved);
    }
    #[test]
    fn history_retains_first_today_and_explicit_middle_selection_under_scan_count_pressure() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let key = cache_key(std::slice::from_ref(&root), true);
        let (_, first_id) =
            publish_history_fixture(&root, "history-first-today", local_history_noon(0));
        let first = list_disk_scan_history(first_id)
            .unwrap()
            .current
            .unwrap()
            .id;
        let mut latest = String::new();
        let mut pinned = String::new();
        for index in 1..28 {
            latest = publish_history_fixture(
                &root,
                &format!("history-pressure-{index}"),
                local_history_noon(0) + index * 1000,
            )
            .1;
            if index == 5 {
                pinned = list_disk_scan_history(latest.clone())
                    .unwrap()
                    .current
                    .unwrap()
                    .id;
            }
            if index == 6 {
                assert_eq!(
                    get_disk_scan_comparison_with_baseline(
                        latest.clone(),
                        None,
                        Some(pinned.clone())
                    )
                    .unwrap()
                    .status,
                    "ready"
                );
                history::reset_memory(); // The selection pin survives catalog reload.
            }
        }
        let listing = list_disk_scan_history(latest.clone()).unwrap();
        assert!(listing.snapshots.iter().any(|summary| summary.id == first));
        assert!(listing.snapshots.iter().any(|summary| summary.id == pinned));
        assert!(
            cache_files(&cache_directory().unwrap(), false)
                .unwrap()
                .iter()
                .filter(|file| file
                    .path
                    .extension()
                    .is_some_and(|extension| extension == "json"))
                .count()
                <= MAX_CACHE_FILES
        );
        assert_eq!(
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(pinned.clone()))
                .unwrap()
                .status,
            "ready"
        );
        // A tighter honest limit keeps selected + current when they fit, even
        // though other day representatives and adjacent history must be lost.
        let current = cache_file(&key).unwrap();
        trim_cache_directory(&cache_directory().unwrap(), Some(&current), 64 * MIB, 2).unwrap();
        history::reset_memory();
        assert_eq!(
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(pinned.clone()))
                .unwrap()
                .status,
            "ready"
        );
        assert_eq!(
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(first))
                .unwrap()
                .status,
            "baseline-unavailable"
        );
        let selected = history::records(&key, INDEX_POLICY_VERSION)
            .into_iter()
            .find(|row| row.summary.id == pinned)
            .unwrap();
        fs::remove_file(selected.path().unwrap()).unwrap();
        assert_eq!(
            get_disk_scan_comparison_with_baseline(latest, None, Some(pinned))
                .unwrap()
                .status,
            "baseline-unavailable"
        );
        history_fixture_cleanup(&root, saved);
    }
    #[test]
    fn history_catalog_loss_corruption_symlinks_and_scope_policy_never_fabricate_snapshots() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let key = cache_key(std::slice::from_ref(&root), true);
        let (_, first) =
            publish_history_fixture(&root, "history-recovery-first", local_history_noon(2));
        let baseline = list_disk_scan_history(first).unwrap().current.unwrap().id;
        let mut latest = String::new();
        for index in 0..3 {
            latest = publish_history_fixture(
                &root,
                &format!("history-recovery-{index}"),
                local_history_noon(0) + index * 1000,
            )
            .1;
        }
        let expected = list_disk_scan_history(latest.clone()).unwrap();
        let catalog = cache_directory().unwrap().join("history.catalog");
        fs::remove_file(&catalog).unwrap();
        history::reset_memory();
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(list_disk_scan_history(latest.clone()).unwrap()).unwrap()
        );
        fs::write(&catalog, b"corrupt catalog").unwrap();
        history::reset_memory();
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(list_disk_scan_history(latest.clone()).unwrap()).unwrap()
        );
        // A syntactically valid catalog cannot invent a timestamp or identity.
        let mut corrupt: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog).unwrap()).unwrap();
        corrupt["records"][0]["summary"]["updatedAtMs"] = serde_json::json!(u64::MAX);
        fs::write(&catalog, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        history::reset_memory();
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(list_disk_scan_history(latest.clone()).unwrap()).unwrap()
        );
        let selected = history::records(&key, INDEX_POLICY_VERSION)
            .into_iter()
            .find(|row| row.summary.id == baseline)
            .unwrap();
        let path = selected.path().unwrap();
        let backup = path.with_extension("fixture-backup");
        fs::rename(&path, &backup).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&backup, &path).unwrap();
            assert_eq!(
                get_disk_scan_comparison_with_baseline(
                    latest.clone(),
                    None,
                    Some(baseline.clone())
                )
                .unwrap()
                .status,
                "baseline-unavailable"
            );
            fs::remove_file(&path).unwrap();
        }
        fs::rename(backup, path).unwrap();
        assert_eq!(
            get_disk_scan_comparison_with_baseline(
                latest.clone(),
                None,
                Some("../../foreign.json".into())
            )
            .unwrap()
            .status,
            "baseline-unavailable"
        );
        let other = fixture();
        let (_, other_id) =
            publish_history_fixture(&other, "history-other-scope", local_history_noon(0));
        assert_eq!(
            get_disk_scan_comparison_with_baseline(other_id, None, Some(baseline.clone()))
                .unwrap()
                .status,
            "baseline-unavailable"
        );
        let other_saved = registry().lock().unwrap().active.clone();
        history_fixture_cleanup(&other, other_saved);
        let source = registry()
            .lock()
            .unwrap()
            .runs
            .get(&latest)
            .unwrap()
            .clone();
        let mut basic = cache_value(&cache_key(std::slice::from_ref(&root), false), &source);
        basic.snapshot.scan_id = format!("basic-{latest}");
        let basic = run_from_cache(basic, false);
        let basic_id = basic.snapshot.lock().unwrap().scan_id.clone();
        registry()
            .lock()
            .unwrap()
            .runs
            .insert(basic_id.clone(), basic);
        assert_eq!(
            get_disk_scan_comparison_with_baseline(basic_id.clone(), None, Some(baseline.clone()))
                .unwrap()
                .status,
            "baseline-unavailable"
        );
        assert_ne!(
            list_disk_scan_history(basic_id).unwrap().scope_id,
            expected.scope_id
        );
        source.snapshot.lock().unwrap().index_policy_version += 1;
        assert_eq!(
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(baseline.clone()))
                .unwrap()
                .status,
            "baseline-unavailable"
        );
        source.snapshot.lock().unwrap().index_policy_version = INDEX_POLICY_VERSION;
        source.snapshot.lock().unwrap().status = "cancelled".into();
        assert_eq!(
            get_disk_scan_comparison_with_baseline(latest, None, Some(baseline))
                .unwrap()
                .status,
            "ready"
        );
        history_fixture_cleanup(&root, saved);
    }
    #[test]
    fn history_missing_current_is_not_replaced_by_old_archive_and_corrupt_headers_are_not_reparsed()
    {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        // Indirect incremental fixtures also read the process-wide catalog.
        // Keep this counter interval exclusive; nested history calls are reentrant.
        let _storage = cache_storage_lock().lock().unwrap();
        let root = fixture();
        let saved = registry().lock().unwrap().active.clone();
        let key = cache_key(std::slice::from_ref(&root), true);
        let (_, first) =
            publish_history_fixture(&root, "history-current-first", local_history_noon(1));
        let baseline = list_disk_scan_history(first).unwrap().current.unwrap().id;
        let (_, latest) =
            publish_history_fixture(&root, "history-current-latest", local_history_noon(0));
        let expected = list_disk_scan_history(latest.clone())
            .unwrap()
            .current
            .unwrap();
        let current = cache_file(&key).unwrap();
        let backup = current.with_extension("fixture-backup");
        fs::rename(&current, &backup).unwrap();
        let missing = list_disk_scan_history(latest.clone()).unwrap();
        assert!(missing.current.is_none());
        assert!(missing
            .snapshots
            .iter()
            .any(|summary| summary.id == baseline));
        assert_ne!(
            get_disk_scan_comparison_with_baseline(latest.clone(), None, Some(baseline.clone()))
                .unwrap()
                .status,
            "ready"
        );
        // A corrupt multi-MiB file is read once per identity, not on every menu poll.
        fs::write(&current, vec![b' '; 2 * 1024 * 1024]).unwrap();
        history::reset_memory();
        assert!(list_disk_scan_history(latest.clone())
            .unwrap()
            .current
            .is_none());
        let reads = history::header_reads();
        for _ in 0..5 {
            assert!(list_disk_scan_history(latest.clone())
                .unwrap()
                .current
                .is_none());
        }
        assert_eq!(reads, history::header_reads());
        // Same-inode repair changes mtime and retries a negative cached header.
        let mut repaired = fs::read(&backup).unwrap();
        repaired.resize(2 * 1024 * 1024, b' ');
        fs::write(&current, repaired).unwrap();
        assert_eq!(
            list_disk_scan_history(latest.clone())
                .unwrap()
                .current
                .unwrap(),
            expected
        );
        let reads = history::header_reads();
        for _ in 0..5 {
            assert!(list_disk_scan_history(latest.clone())
                .unwrap()
                .current
                .is_some());
        }
        assert_eq!(reads, history::header_reads());
        fs::remove_file(backup).unwrap();
        history_fixture_cleanup(&root, saved);
    }
    #[test]
    fn cache_hardlink_accounting_cannot_double_charge_or_claim_linked_bytes_freed() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let directory = root.join("allocation-budget");
        fs::create_dir(&directory).unwrap();
        let first = directory.join("first.json");
        let second = directory.join("second.json");
        fs::write(&first, vec![1; 16 * 1024]).unwrap();
        fs::hard_link(&first, &second).unwrap();
        let allocated = allocated(&fs::metadata(&first).unwrap());
        assert_eq!(
            cache_allocated_bytes(&cache_files(&directory, false).unwrap()),
            allocated
        );
        assert_eq!(
            trim_cache_directory(&directory, Some(&second), allocated, 1).unwrap(),
            allocated
        );
        assert!(!first.exists() && second.exists());
        assert!(trim_cache_directory(&directory, Some(&second), allocated - 1, 1).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn deep_snapshot_navigation_uses_saved_index_after_fixture_is_removed() {
        let root = fixture();
        let deep = root.join("nested/child/a/b/c/d/e");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("payload"), [3u8; 8192]).unwrap();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        assert!(!run
            .snapshot
            .lock()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.path == string(&deep)));
        let key = cache_key(std::slice::from_ref(&root), true);
        let cache_path = root.join("saved.json");
        fs::write(
            &cache_path,
            serde_json::to_vec(&cache_value(&key, &run)).unwrap(),
        )
        .unwrap();
        let restored = run_from_cache(read_cache_at(&cache_path, &key, false).unwrap(), true);
        let original_updated = restored.snapshot.lock().unwrap().updated_at_ms;
        fs::remove_dir_all(&root).unwrap();
        let view = snapshot_view(&restored, &string(&deep)).unwrap();
        assert!(view.view_detail_complete);
        assert_eq!(view.logical_bytes, 8192);
        assert_eq!(view.scanned_files, 1);
        assert_eq!(view.scan_id, "fixture");
        assert_eq!(view.updated_at_ms, original_updated);
        assert_eq!(view.source_roots, Some(vec![string(&root)]));
        assert_eq!(view.roots, vec![string(&deep)]);
        assert!(view
            .entries
            .iter()
            .all(|e| e.path == string(&deep) || e.parent_path == string(&deep)));
        assert!(snapshot_view(&restored, &string(&root.join("not-indexed")))
            .unwrap_err()
            .starts_with("not-indexed:"));
        assert!(snapshot_view(&restored, &string(&root.join("nested/../escape"))).is_err());
        assert!(snapshot_view(&restored, "/outside-source").is_err());
        assert!(!restored.cancelled.load(Ordering::Relaxed));
    }
    #[test]
    #[cfg(unix)]
    fn snapshot_subdirectories_keep_source_hardlink_ownership() {
        let root = fixture();
        let first = root.join("first");
        let second = root.join("second/deeper");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("original"), [1u8; 12288]).unwrap();
        fs::hard_link(first.join("original"), second.join("alias")).unwrap();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let a = snapshot_view(&run, &string(&first)).unwrap();
        let b = snapshot_view(&run, &string(&second)).unwrap();
        assert_eq!(a.logical_bytes + b.logical_bytes, 12288);
        assert!(a.logical_bytes == 0 || b.logical_bytes == 0);
        assert_eq!(a.scanned_files, 1);
        assert_eq!(b.scanned_files, 1);
        assert_eq!(
            snapshot_view(&run, &string(&first)).unwrap().logical_bytes,
            a.logical_bytes
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn hardlink_directory_and_sample_ownership_is_independent_of_worker_encounter_order() {
        let root = fixture();
        let first = root.join("a-owner/deeper");
        let second = root.join("z-alias/deeper");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let canonical = first.join("payload");
        let alias = second.join("payload");
        fs::write(&canonical, [1u8; 12288]).unwrap();
        fs::hard_link(&canonical, &alias).unwrap();
        let metadata = InventoryMeta::read(&canonical).unwrap();
        let measure_in_order = |reverse: bool| {
            let run = fixture_run(&root);
            assert!(run.recorder.lock().unwrap().is_none());
            let seen = Mutex::new(HardlinkOwners::new());
            let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUED_JOBS);
            queue_root(&run, &root, &seen, &sender, 0);
            drop(sender);
            let mut jobs: Vec<_> = receiver.into_iter().collect();
            jobs.sort_by(|a, b| a.path.cmp(&b.path));
            if reverse {
                jobs.reverse();
            }
            for job in jobs {
                scan_queued_subtree(&run, &job, &root, &seen, true);
            }
            // The ledger retains one record for the linked identity, regardless
            // of encounter order or the ordinary files in the scope.
            assert_eq!(seen.lock().unwrap().len(), 1);
            let totals = {
                let snapshot = run.snapshot.lock().unwrap();
                (
                    snapshot.allocated_bytes,
                    snapshot.logical_bytes,
                    snapshot.scanned_files,
                    snapshot.scanned_directories,
                )
            };
            normalize_hardlink_ownership(&run, &seen, std::slice::from_ref(&root));
            {
                let snapshot = run.snapshot.lock().unwrap();
                assert_eq!(
                    totals,
                    (
                        snapshot.allocated_bytes,
                        snapshot.logical_bytes,
                        snapshot.scanned_files,
                        snapshot.scanned_directories
                    )
                );
                assert!(snapshot
                    .entries
                    .iter()
                    .any(|row| row.path == string(&canonical)
                        && row.allocated_bytes == metadata.allocated
                        && row.logical_bytes == 12288));
                assert!(!snapshot
                    .entries
                    .iter()
                    .any(|row| row.path == string(&alias)));
                // Mirror final root-row publication after the forced job order.
                let root_row = snapshot
                    .entries
                    .iter()
                    .find(|row| row.path == string(&root))
                    .unwrap()
                    .clone();
                run.index
                    .lock()
                    .unwrap()
                    .insert(root_row.path.clone(), root_row);
            }
            run
        };
        let forward = measure_in_order(false);
        let backward = measure_in_order(true);
        let measurements = |run: &Run| {
            run.index
                .lock()
                .unwrap()
                .iter()
                .map(|(path, row)| {
                    (
                        path.clone(),
                        (
                            row.allocated_bytes,
                            row.logical_bytes,
                            row.file_count,
                            row.directory_count,
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(measurements(&forward), measurements(&backward));
        assert_eq!(
            forward
                .index
                .lock()
                .unwrap()
                .get(&string(&first))
                .unwrap()
                .logical_bytes,
            12288
        );
        assert_eq!(
            forward
                .index
                .lock()
                .unwrap()
                .get(&string(&second))
                .unwrap()
                .logical_bytes,
            0
        );
        for workers in [1, 4] {
            let run = fixture_run(&root);
            scan_roots_with_workers(run.clone(), vec![root.clone()], workers);
            assert_eq!(measurements(&run), measurements(&forward));
        }
        // A focused scan has its own identity scope and can attribute the alias.
        let focused = fixture_run(&second);
        scan_roots_with_workers(focused.clone(), vec![second.clone()], 1);
        assert_eq!(focused.snapshot.lock().unwrap().logical_bytes, 12288);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn hardlinked_symlinks_keep_deduplication_and_remain_out_of_large_file_samples() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let first = root.join("a-symlink");
        let second = root.join("z-symlink");
        symlink("unread-target", &second).unwrap();
        fs::hard_link(&second, &first).unwrap();
        let run = fixture_run(&root);
        scan_roots_with_workers(run.clone(), vec![root.clone()], 2);
        let snapshot = run.snapshot.lock().unwrap();
        assert_eq!(snapshot.logical_bytes, 16384 + "unread-target".len() as u64);
        assert!(!snapshot
            .entries
            .iter()
            .any(|row| row.path == string(&first) || row.path == string(&second)));
        drop(snapshot);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn version_three_shallow_cache_is_compatible_and_reports_missing_detail() {
        let root = fixture();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let key = cache_key(std::slice::from_ref(&root), true);
        let mut old = serde_json::to_value(cache_value(&key, &run)).unwrap();
        old.as_object_mut().unwrap().remove("index");
        old.as_object_mut().unwrap().remove("index_complete");
        old["snapshot"]["entries"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["path"] != string(&root.join("nested/child")));
        for field in [
            "sourceRoots",
            "viewPath",
            "viewDetailComplete",
            "indexPolicyVersion",
        ] {
            old["snapshot"].as_object_mut().unwrap().remove(field);
        }
        let path = root.join("old-cache.json");
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let restored = run_from_cache(read_cache_at(&path, &key, false).unwrap(), true);
        let view = snapshot_view(&restored, &string(&root.join("nested"))).unwrap();
        assert!(!view.view_detail_complete);
        assert_eq!(view.index_policy_version, 0);
        assert_eq!(view.logical_bytes, 16384);
        assert_eq!(
            view.updated_at_ms,
            run.snapshot.lock().unwrap().updated_at_ms
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn exact_cache_is_readable_while_other_scope_runs_and_cancelled_run_is_not_reused() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-cache-fixture-{}-{}",
            std::process::id(),
            now_ms()
        ));
        // This nonexistent path proves the exact-cache path does no metadata query.
        let cached_run = fixture_run(&root);
        let cached_id = unique_id(&cached_run, "cached");
        cached_run.snapshot.lock().unwrap().status = "complete".into();
        cached_run.snapshot.lock().unwrap().freshness = "verified".into();
        let other = fixture_run(&root.join("other"));
        let other_id = unique_id(&other, "active");
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous_active;
        {
            let mut reg = registry().lock().unwrap();
            previous_active = reg.active.replace(other_id.clone());
            reg.runs.insert(other_id.clone(), other.clone());
            reg.cached
                .insert(key.clone(), cache_value(&key, &cached_run));
        }
        let hit = start_disk_scan_with_options(Some(string(&root)), false, true).unwrap();
        assert_eq!(hit.scan_id, cached_id);
        assert!(hit.cache_hit);
        assert_eq!(
            registry().lock().unwrap().active.as_deref(),
            Some(other_id.as_str())
        );
        assert!(!other.cancelled.load(Ordering::Relaxed));
        assert!(
            start_disk_scan_with_options(Some(string(&root.join("uncached"))), false, true)
                .unwrap_err()
                .contains("already running")
        );
        other.cancelled.store(true, Ordering::Relaxed);
        assert!(
            start_disk_scan_with_options(Some(string(&root.join("other"))), false, true)
                .unwrap_err()
                .contains("already running")
        );
        let mut reg = registry().lock().unwrap();
        reg.cached.remove(&key);
        reg.runs.remove(&cached_id);
        reg.runs.remove(&other_id);
        reg.active = previous_active;
    }
    #[test]
    fn checkpoint_roundtrip_preserves_last_good_and_generation_guards_publication() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let run = fixture_run(&root);
        let id = unique_id(&run, "persist");
        scan_roots(run.clone(), vec![root.clone()]);
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.active.replace(id.clone())
        };
        write_cache(&key, &run, false);
        let good = read_cache(&key).unwrap();
        assert!(!good.index.is_empty());
        let good_time = good.snapshot.updated_at_ms;
        {
            let mut s = run.snapshot.lock().unwrap();
            s.status = "running".into();
            s.scanned_files += 10;
            s.updated_at_ms += 123;
        }
        write_cache(&key, &run, true);
        assert_eq!(read_cache(&key).unwrap().snapshot.scanned_files, 1);
        assert_eq!(read_cache(&key).unwrap().snapshot.updated_at_ms, good_time);
        let checkpoint = read_cache_at(&checkpoint_file(&key).unwrap(), &key, true).unwrap();
        assert_eq!(checkpoint.snapshot.status, "cancelled");
        assert!(!checkpoint.snapshot.coverage_complete);
        assert_eq!(checkpoint.snapshot.scanned_files, 11);
        assert_eq!(checkpoint.snapshot.updated_at_ms, good_time + 123);
        fs::remove_file(cache_file(&key).unwrap()).unwrap();
        assert_eq!(read_cache(&key).unwrap().snapshot.scanned_files, 11);
        // A cancelled or failed refresh cannot delete or replace last good.
        run.snapshot.lock().unwrap().status = "complete".into();
        run.snapshot.lock().unwrap().freshness = "verified".into();
        write_cache(&key, &run, false);
        let before = fs::read(cache_file(&key).unwrap()).unwrap();
        registry().lock().unwrap().active = Some("newer-generation".into());
        run.snapshot.lock().unwrap().scanned_files += 1;
        write_cache(&key, &run, false);
        write_cache(&key, &run, true);
        assert_eq!(fs::read(cache_file(&key).unwrap()).unwrap(), before);
        clear_active(&id);
        assert_eq!(
            registry().lock().unwrap().active.as_deref(),
            Some("newer-generation")
        );
        clear_active("newer-generation");
        assert!(registry().lock().unwrap().active.is_none());
        {
            let mut reg = registry().lock().unwrap();
            reg.cached.remove(&key);
            reg.active = previous_active;
        }
        let _ = fs::remove_file(cache_file(&key).unwrap());
        let _ = fs::remove_file(checkpoint_file(&key).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn directory_layer_keeps_descriptor_scope_after_ancestor_rename_and_symlink_replacement() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = fixture();
        fs::write(outside.join("nested/child/data"), [9u8; 65536]).unwrap();
        fs::write(outside.join("nested/child/outside-only"), [9u8; 65536]).unwrap();
        let run = fixture_run(&root);
        let selected = root.join("nested/child");
        let mut reader = DirectoryFrame::open_layer(&selected, None).unwrap();
        // Deterministically replace an ancestor after its descriptor has opened.
        fs::rename(root.join("nested"), root.join("original-nested")).unwrap();
        symlink(outside.join("nested"), root.join("nested")).unwrap();
        let page = directory_page_from_reader(&run, &selected, None, 20, &mut reader).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "data");
        assert_eq!(page.entries[0].logical_bytes, Some(16384));
        // A child replaced with a symlink is inspected as a link inode, never
        // resolved to the outside file, even through the already-open reader.
        fs::remove_file(root.join("original-nested/child/data")).unwrap();
        symlink(
            outside.join("nested/child/data"),
            root.join("original-nested/child/data"),
        )
        .unwrap();
        let meta = reader.child_metadata(std::ffi::OsStr::new("data")).unwrap();
        assert!(meta.symlink);
        assert_ne!(meta.logical, 65536);
        assert!(DirectoryFrame::open_layer(&selected, None).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn recursive_missing_metadata_stays_relative_after_ancestor_replacement() {
        use std::os::unix::fs::symlink;
        for native in [false, true] {
            let root = fixture();
            let outside = fixture();
            fs::write(outside.join("nested/child/data"), [9u8; 65536]).unwrap();
            let selected = root.join("nested/child");
            let mut frame = DirectoryFrame::open(&selected, native).unwrap();
            let mut node = frame.next().unwrap().unwrap();
            // Exercise the fallback used by recursive traversal when a bulk
            // record omits required attributes, rather than the layer API.
            node.metadata = None;
            fs::rename(root.join("nested"), root.join("original-nested")).unwrap();
            symlink(outside.join("nested"), root.join("nested")).unwrap();
            assert_eq!(frame.node_metadata(&node).unwrap().logical, 16384);
            assert!(InventoryMeta::read(&node.path).is_err());
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
        }
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn root_open_rejects_directory_replacement_after_initial_metadata_check() {
        for native in [false, true] {
            let root = fixture();
            let expected = InventoryMeta::read(&root).unwrap();
            let original = root.with_extension("original-root");
            fs::rename(&root, &original).unwrap();
            fs::create_dir(&root).unwrap();
            fs::write(root.join("replacement-only"), [9u8; 65536]).unwrap();
            // The former unchecked read-then-open sequence opens this different
            // real directory successfully; the producer's verified helper fails
            // deterministically before any root counters or layer can publish.
            assert!(DirectoryFrame::open(&root, native).is_ok());
            let error = DirectoryFrame::open_verified(&root, native, &expected)
                .err()
                .unwrap();
            assert!(error.to_string().contains("identity changed"));
            assert!(DirectoryFrame::open_verified(&original, native, &expected).is_ok());
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(original).unwrap();
        }
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn queued_scan_keeps_original_scope_after_root_rename_and_symlink_replacement() {
        use std::os::unix::fs::symlink;
        for native in [false, true] {
            let root = fixture();
            let outside = fixture();
            fs::write(outside.join("nested/child/data"), [9u8; 65536]).unwrap();
            fs::write(outside.join("nested/child/outside-only"), [9u8; 65536]).unwrap();
            let original = root.with_extension("anchored-original");
            let run = fixture_run(&root);
            let seen = Mutex::new(HardlinkOwners::new());
            let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUED_JOBS);
            queue_root(&run, &root, &seen, &sender, 0);
            drop(sender);
            let jobs: Vec<_> = receiver.into_iter().collect();
            assert_eq!(jobs.len(), 1);
            // Jobs pin only the original root FD. No worker has opened or
            // pre-enumerated a child before this deterministic replacement.
            fs::rename(&root, &original).unwrap();
            symlink(&outside, &root).unwrap();
            for job in jobs {
                scan_queued_subtree(&run, &job, &root, &seen, native);
            }
            let snapshot = run.snapshot.lock().unwrap();
            assert_eq!(snapshot.logical_bytes, 16384);
            assert_eq!(snapshot.scanned_files, 1);
            assert_eq!(snapshot.scanned_directories, 3);
            assert_eq!(snapshot.issue_count, 0);
            assert!(!snapshot
                .entries
                .iter()
                .any(|row| row.name == "outside-only"));
            drop(snapshot);
            fs::remove_file(&root).unwrap();
            fs::remove_dir_all(original).unwrap();
            fs::remove_dir_all(outside).unwrap();
        }
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn queued_scan_rejects_child_symlink_or_inode_replacement_before_open() {
        use std::os::unix::fs::symlink;
        for native in [false, true] {
            for link in [false, true] {
                let root = fixture();
                let outside = fixture();
                let run = fixture_run(&root);
                let seen = Mutex::new(HardlinkOwners::new());
                let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUED_JOBS);
                queue_root(&run, &root, &seen, &sender, 0);
                drop(sender);
                let jobs: Vec<_> = receiver.into_iter().collect();
                fs::rename(root.join("nested"), root.join("original-nested")).unwrap();
                if link {
                    symlink(outside.join("nested"), root.join("nested")).unwrap();
                } else {
                    fs::create_dir(root.join("nested")).unwrap();
                    fs::write(root.join("nested/replacement"), [9u8; 65536]).unwrap();
                }
                for job in jobs {
                    scan_queued_subtree(&run, &job, &root, &seen, native);
                }
                let snapshot = run.snapshot.lock().unwrap();
                assert_eq!(snapshot.logical_bytes, 0);
                assert_eq!(snapshot.scanned_files, 0);
                assert!(snapshot
                    .issues
                    .iter()
                    .any(|issue| issue.path == string(&root.join("nested"))
                        && issue.kind == "unreadable"));
                drop(snapshot);
                fs::remove_dir_all(root).unwrap();
                fs::remove_dir_all(outside).unwrap();
            }
        }
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn nested_directory_open_stays_relative_and_checks_enumerated_identity() {
        use std::os::unix::fs::symlink;
        for native in [false, true] {
            let root = fixture();
            let outside = fixture();
            fs::write(outside.join("nested/child/data"), [9u8; 65536]).unwrap();
            let selected = root.join("nested");
            let mut parent = DirectoryFrame::open(&selected, native).unwrap();
            let node = parent.next().unwrap().unwrap();
            let expected = parent.node_metadata(&node).unwrap();
            fs::rename(&selected, root.join("original-nested")).unwrap();
            symlink(outside.join("nested"), &selected).unwrap();
            let mut child = parent.open_child(&node.path, native, &expected).unwrap();
            let payload = child.next().unwrap().unwrap();
            assert_eq!(child.node_metadata(&payload).unwrap().logical, 16384);
            // Replacement of the direct child is rejected even through the
            // trusted descriptor; a directory with a different inode is unsafe.
            fs::rename(
                root.join("original-nested/child"),
                root.join("original-nested/original-child"),
            )
            .unwrap();
            fs::create_dir(root.join("original-nested/child")).unwrap();
            assert!(parent.open_child(&node.path, native, &expected).is_err());
            fs::remove_dir(root.join("original-nested/child")).unwrap();
            symlink(
                outside.join("nested/child"),
                root.join("original-nested/child"),
            )
            .unwrap();
            assert!(parent.open_child(&node.path, native, &expected).is_err());
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
        }
    }
    #[test]
    fn durable_directory_layer_restores_missing_index_without_live_paths_or_workers() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        fs::hard_link(root.join("nested/child/data"), root.join("alias")).unwrap();
        fs::create_dir_all(root.join("nested/child/second/third")).unwrap();
        fs::write(root.join("nested/child/second/third/payload"), [3u8; 4096]).unwrap();
        let run = fixture_run(&root);
        let id = unique_id(&run, "durable-directory-view");
        let roots = vec![root.clone()];
        let key = cache_key(&roots, true);
        let db = cache_file(&key).unwrap().with_extension("index.sqlite");
        let mut session = incremental::Session::begin_audit(
            &db,
            &format!("{INDEX_POLICY_VERSION}\n{key}"),
            &roots,
            true,
            128 * MIB,
            false,
        )
        .unwrap();
        *run.recorder.lock().unwrap() = Some(session.recorder());
        scan_roots(run.clone(), roots);
        *run.recorder.lock().unwrap() = None;
        assert!(session
            .finish_audit(&run)
            .unwrap()
            .unwrap()
            .persistence_error
            .is_none());
        let target = root.join("nested");
        let expected = snapshot_view(&run, &string(&target)).unwrap();
        assert_eq!(
            expected.logical_bytes, 4096,
            "the root alias owns the linked file's bytes"
        );
        run.index.lock().unwrap().clear();
        run.snapshot
            .lock()
            .unwrap()
            .entries
            .retain(|row| row.path == string(&root));
        fs::remove_dir_all(&root).unwrap();
        let active_before = {
            let mut reg = registry().lock().unwrap();
            let active = reg.active.clone();
            reg.runs.insert(id.clone(), run.clone());
            active
        };
        let restored = get_disk_scan_view(id.clone(), string(&target)).unwrap();
        assert_eq!(
            (
                restored.allocated_bytes,
                restored.logical_bytes,
                restored.scanned_files,
                restored.scanned_directories
            ),
            (
                expected.allocated_bytes,
                expected.logical_bytes,
                expected.scanned_files,
                expected.scanned_directories
            )
        );
        assert!(restored.view_detail_complete && restored.cache_hit);
        assert!(restored
            .treemap_entries
            .iter()
            .any(|row| row.path == string(&target.join("child/second/third"))));
        assert_eq!(
            serde_json::to_value(&restored.treemap_entries).unwrap(),
            serde_json::to_value(&expected.treemap_entries).unwrap()
        );
        assert_eq!(restored.scan_id, id);
        assert!(run.index.lock().unwrap().is_empty());
        assert_eq!(registry().lock().unwrap().active, active_before);
        assert!(get_disk_scan_view(id.clone(), string(&root.join("../outside"))).is_err());
        {
            let connection = rusqlite::Connection::open(&db).unwrap();
            connection
                .execute(
                    "UPDATE metadata SET value='different-generation' WHERE key='generation'",
                    [],
                )
                .unwrap();
        }
        assert!(get_disk_scan_view(id.clone(), string(&target))
            .unwrap_err()
            .starts_with("not-indexed:"));
        registry().lock().unwrap().runs.remove(&id);
        fs::remove_file(db).unwrap();
    }
    #[test]
    fn cache_tiers_retain_different_amounts_of_the_same_deep_working_set() {
        let root = Path::new("/tier-fixture");
        let working_set: Vec<_> = (0..50_000)
            .map(|n| {
                root.join("one/two")
                    .join(format!("{n:05}-{}", "long-name-".repeat(40)))
            })
            .collect();
        let retained: Vec<_> = [64, 256, 512]
            .into_iter()
            .map(|tier| {
                let mut budget = IndexBudget {
                    policy: cache_policy(tier),
                    ..IndexBudget::default()
                };
                working_set
                    .iter()
                    .filter(|path| budget.retain(path, root, false))
                    .count()
            })
            .collect();
        assert!(retained[0] > 0 && retained[0] < retained[1] && retained[1] < retained[2]);
    }
    #[test]
    fn cache_only_scope_lookup_has_no_worker_side_effects_on_miss_or_hit() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-cache-only-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let selected = root.join("never-created");
        let key = cache_key(std::slice::from_ref(&selected), true);
        let active = fixture_run(&root.join("other-active"));
        let active_id = unique_id(&active, "cache-only-worker");
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(active_id.clone(), active.clone());
            reg.active.replace(active_id.clone())
        };
        let before_count = registry().lock().unwrap().runs.len();
        assert!(get_cached_disk_scan(Some(string(&selected)), true, None)
            .unwrap()
            .is_none());
        assert_eq!(registry().lock().unwrap().runs.len(), before_count);
        assert_eq!(
            registry().lock().unwrap().active.as_deref(),
            Some(active_id.as_str())
        );
        assert!(!active.cancelled.load(Ordering::Relaxed));
        assert!(!active.finalized.load(Ordering::Relaxed));
        let cached = fixture_run(&selected);
        let cached_id = unique_id(&cached, "cache-only-exact");
        cached.snapshot.lock().unwrap().status = "complete".into();
        cached.snapshot.lock().unwrap().freshness = "verified".into();
        cached.index_complete.store(true, Ordering::Relaxed);
        registry()
            .lock()
            .unwrap()
            .cached
            .insert(key.clone(), cache_value(&key, &cached));
        let displayed = fixture_run(&root.join("displayed"));
        displayed.snapshot.lock().unwrap().status = "complete".into();
        displayed.snapshot.lock().unwrap().freshness = "verified".into();
        let displayed_id = unique_id(&displayed, "cache-only-displayed");
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(displayed_id.clone(), displayed.clone());
            for n in reg.runs.len()..MAX_RESIDENT_RUNS {
                let filler = fixture_run(&root.join(format!("cache-only-filler-{n}")));
                filler.snapshot.lock().unwrap().status = "complete".into();
                filler.snapshot.lock().unwrap().freshness = "verified".into();
                let id = unique_id(&filler, &format!("cache-only-filler-{n}"));
                reg.runs.insert(id, filler);
            }
        }
        let hit = get_cached_disk_scan(Some(string(&selected)), true, Some(displayed_id.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(hit.scan_id, cached_id);
        assert!(hit.cache_hit);
        assert_eq!(hit.roots, vec![string(&selected)]);
        assert!(Arc::ptr_eq(&source_run(&displayed_id).unwrap(), &displayed));
        assert!(registry().lock().unwrap().runs.len() <= MAX_RESIDENT_RUNS);
        assert_eq!(
            registry().lock().unwrap().active.as_deref(),
            Some(active_id.as_str())
        );
        assert!(!active.cancelled.load(Ordering::Relaxed));
        // A registry exact hit is also read-only and does not finalize a worker.
        let active_hit = get_cached_disk_scan(Some(string(&root.join("other-active"))), true, None)
            .unwrap()
            .unwrap();
        assert_eq!(active_hit.scan_id, active_id);
        assert_eq!(active_hit.status, "running");
        assert!(!active.finalized.load(Ordering::Relaxed));
        let mut reg = registry().lock().unwrap();
        reg.active = previous_active;
        reg.runs.remove(&active_id);
        reg.runs.remove(&cached_id);
        reg.runs.remove(&displayed_id);
        reg.cached.remove(&key);
    }
    #[test]
    fn current_directory_pages_cover_all_files_without_recursive_inventory_or_cache_growth() {
        let root = fixture();
        for n in 0..1103 {
            fs::write(root.join(format!("file-{n:04}")), [1u8; 8]).unwrap();
        }
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        // Reproduce a shallow old cache that knows the selected scope total but
        // does not know a child's recursive summary.
        run.index
            .lock()
            .unwrap()
            .retain(|path, _| path == &string(&root));
        run.snapshot
            .lock()
            .unwrap()
            .entries
            .retain(|e| e.path == string(&root) || e.kind == "file");
        let before = serde_json::to_vec(&cache_value("fixture", &run)).unwrap();
        let mut cursor = None;
        let mut found = Vec::new();
        loop {
            let page = directory_page(&run, &string(&root), cursor.as_deref(), 5000).unwrap();
            assert!(page.entries.len() <= MAX_DIRECTORY_PAGE);
            found.extend(page.entries);
            if !page.has_more {
                assert!(page.next_after_path.is_none());
                break;
            }
            cursor = page.next_after_path;
        }
        assert_eq!(found.len(), 1104);
        assert_eq!(found.iter().filter(|e| e.kind == "file").count(), 1103);
        assert_eq!(
            found.iter().map(|e| &e.path).collect::<HashSet<_>>().len(),
            1104
        );
        let unknown = found.iter().find(|e| e.name == "nested").unwrap();
        assert_eq!(unknown.measurement, "unknown");
        assert!(unknown.allocated_bytes.is_none());
        assert!(!unknown.complete);
        assert!(found
            .iter()
            .filter(|e| e.kind == "file")
            .all(|e| e.logical_bytes == Some(8)));
        assert_eq!(
            before,
            serde_json::to_vec(&cache_value("fixture", &run)).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn layer_file_identities_allow_hardlink_dedup_and_snapshot_totals_remain_conserved() {
        let root = fixture();
        fs::write(root.join("original"), [1u8; 32768]).unwrap();
        fs::hard_link(root.join("original"), root.join("alias")).unwrap();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let page = directory_page(&run, &string(&root), None, 500).unwrap();
        let mut inodes = HashSet::new();
        let mut bytes = allocated(&fs::symlink_metadata(&root).unwrap());
        let mut logical = 0;
        for item in &page.entries {
            if let Some(identity) = &item.hard_link_identity {
                if !inodes.insert(identity.clone()) {
                    continue;
                }
            }
            bytes += item.allocated_bytes.unwrap();
            logical += item.logical_bytes.unwrap();
        }
        let snapshot = run.snapshot.lock().unwrap();
        assert_eq!(logical, snapshot.logical_bytes);
        assert_eq!(bytes, snapshot.allocated_bytes);
        assert_eq!(snapshot.scanned_files, 3);
        assert_eq!(inodes.len(), 1);
        drop(snapshot);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn layer_listing_rejects_traversal_symlinks_cloud_and_protected_scopes() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        symlink(root.join("nested"), root.join("link")).unwrap();
        let run = fixture_run(&root);
        assert!(directory_page(&run, &string(&root.join("../escape")), None, 20).is_err());
        assert!(directory_page(&run, "/outside-source", None, 20)
            .unwrap_err()
            .contains("outside"));
        assert!(
            directory_page(&run, &string(&root.join("link/child")), None, 20)
                .unwrap_err()
                .contains("symbolic")
        );
        assert!(directory_page(
            &run,
            &string(&root.join("Library/CloudStorage/does-not-exist")),
            None,
            20
        )
        .unwrap_err()
        .starts_with("cloud-excluded:"));
        assert!(
            directory_page(&run, &string(&root), Some("/outside-cursor"), 20)
                .unwrap_err()
                .contains("cursor")
        );
        let page = directory_page(&run, &string(&root), None, 20).unwrap();
        let link = page.entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.kind, "symlink");
        assert!(link.allocated_bytes.is_none());
        let protected_root = home().unwrap();
        let mut basic = fixture_run(&protected_root);
        Arc::get_mut(&mut basic).unwrap().allow_protected = false;
        assert!(directory_page(
            &basic,
            &string(&protected_root.join("Documents/nonexistent-fixture")),
            None,
            20
        )
        .unwrap_err()
        .starts_with("permission-required:"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shallow_index_reserve_survives_exhausted_deep_directory_budget() {
        let root = fixture();
        let run = fixture_run(&root);
        run.index_budget.lock().unwrap().general.rows = usize::MAX;
        scan_roots(run.clone(), vec![root.clone()]);
        assert!(run
            .index
            .lock()
            .unwrap()
            .contains_key(&string(&root.join("nested/child"))));
        assert_eq!(run.snapshot.lock().unwrap().logical_bytes, 16384);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn inspect_reuses_exact_cache_without_path_io_and_keeps_other_active_scan() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-inspect-cache-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let selected = root.join("missing-on-disk");
        let source = fixture_run(&root);
        let source_id = unique_id(&source, "inspect-source");
        source.snapshot.lock().unwrap().status = "complete".into();
        source.snapshot.lock().unwrap().freshness = "verified".into();
        let exact = fixture_run(&selected);
        let exact_id = unique_id(&exact, "inspect-exact");
        {
            let mut snapshot = exact.snapshot.lock().unwrap();
            snapshot.status = "complete".into();
            snapshot.freshness = "verified".into();
        }
        // Deeper summaries were truncated. Reuse this exact scope anyway.
        assert!(!exact.index_complete.load(Ordering::Relaxed));
        let key = cache_key(std::slice::from_ref(&selected), true);
        let active = fixture_run(&root.join("unrelated"));
        let active_id = unique_id(&active, "inspect-other-worker");
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(source_id.clone(), source.clone());
            reg.runs.insert(active_id.clone(), active.clone());
            reg.cached.insert(key.clone(), cache_value(&key, &exact));
            reg.active.replace(active_id.clone())
        };
        let response = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        assert!(response.cache_hit);
        assert_eq!(response.scan_id, exact_id);
        assert_eq!(response.roots, vec![string(&selected)]);
        assert_eq!(response.source_scan_id.as_deref(), Some(source_id.as_str()));
        assert!(inspect_disk_directory(
            source_id.clone(),
            string(&root.join("another-missing-scope"))
        )
        .unwrap_err()
        .contains("Cannot inspect directory"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::create_dir(&root).unwrap();
            let replaced = root.join("replaced-by-link");
            symlink(&root, &replaced).unwrap();
            let error = inspect_disk_directory(source_id.clone(), string(&replaced)).unwrap_err();
            assert!(error.contains("symbolic links"));
            assert!(!error.contains("already running"));
            fs::remove_dir_all(&root).unwrap();
        }
        assert!(!active.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            registry().lock().unwrap().active.as_deref(),
            Some(active_id.as_str())
        );
        let mut reg = registry().lock().unwrap();
        reg.active = previous_active;
        for id in [source_id, active_id, exact_id] {
            reg.runs.remove(&id);
        }
        reg.cached.remove(&key);
    }
    #[test]
    fn inspect_missing_index_scans_only_selected_owned_scope_and_reuses_result() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-inspect-scope-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let selected = root.join("selected");
        fs::create_dir_all(selected.join("deep")).unwrap();
        fs::create_dir_all(root.join("unrelated")).unwrap();
        fs::write(selected.join("deep/only-file"), [1u8; 4096]).unwrap();
        fs::write(root.join("unrelated/not-selected"), [1u8; 32768]).unwrap();
        let source = fixture_run(&root);
        let source_id = unique_id(&source, "inspect-owned-source");
        source.snapshot.lock().unwrap().status = "complete".into();
        source.snapshot.lock().unwrap().freshness = "verified".into();
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(source_id.clone(), source.clone());
            reg.active.take()
        };
        let response = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let finished = loop {
            let current = get_disk_scan(response.scan_id.clone()).unwrap();
            if current.status != "running" {
                break current;
            }
            assert!(Instant::now() < deadline, "owned scope scan timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(finished.status, "complete");
        assert_eq!(finished.roots, vec![string(&selected)]);
        assert_eq!(finished.scanned_files, 1);
        assert_eq!(finished.logical_bytes, 4096);
        assert_eq!(finished.source_scan_id.as_deref(), Some(source_id.as_str()));
        let reused = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        assert_eq!(reused.scan_id, finished.scan_id);
        assert!(reused.cache_hit);
        let key = cache_key(std::slice::from_ref(&selected), true);
        {
            let mut reg = registry().lock().unwrap();
            reg.active = previous_active;
            reg.runs.remove(&source_id);
            reg.runs.remove(&finished.scan_id);
            reg.cached.remove(&key);
        }
        let _ = fs::remove_file(cache_file(&key).unwrap());
        let _ = fs::remove_file(checkpoint_file(&key).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn inspect_prefers_durable_last_good_over_newer_partial_cancelled_or_updating_exact() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-inspect-last-good-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let selected = root.join("never-created");
        let key = cache_key(std::slice::from_ref(&selected), true);
        let source = fixture_run(&root);
        let source_id = unique_id(&source, "inspect-last-good-source");
        source.snapshot.lock().unwrap().status = "complete".into();
        source.snapshot.lock().unwrap().freshness = "verified".into();
        let complete = fixture_run(&selected);
        let complete_id = unique_id(&complete, "inspect-last-good-complete");
        {
            let mut good = complete.snapshot.lock().unwrap();
            good.status = "complete".into();
            good.freshness = "verified".into();
            good.updated_at_ms = 1;
            good.index_policy_version = 0;
            good.scanned_directories = 1;
            let mut root_row = new_entry(&selected, true);
            root_row.directory_count = 1;
            root_row.complete = true;
            good.entries.push(root_row);
        }
        let file = cache_file(&key).unwrap();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            serde_json::to_vec(&cache_value(&key, &complete)).unwrap(),
        )
        .unwrap();
        let cancelled = fixture_run(&selected);
        let cancelled_id = unique_id(&cancelled, "inspect-last-good-cancelled");
        cancelled.snapshot.lock().unwrap().status = "cancelled".into();
        cancelled.cancelled.store(true, Ordering::Relaxed);
        let gap = fixture_run(&selected);
        let gap_id = unique_id(&gap, "inspect-last-good-gap");
        {
            let mut snapshot = gap.snapshot.lock().unwrap();
            snapshot.status = "complete".into();
            snapshot.freshness = "partial".into();
            snapshot.updated_at_ms = u64::MAX;
        }
        let active = fixture_run(&root.join("unrelated-worker"));
        let active_id = unique_id(&active, "inspect-last-good-active");
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(source_id.clone(), source.clone());
            reg.runs.insert(cancelled_id.clone(), cancelled);
            reg.runs.insert(gap_id.clone(), gap.clone());
            reg.runs.insert(active_id.clone(), active.clone());
            reg.active.replace(active_id.clone())
        };
        let back = get_cached_disk_scan(Some(string(&selected)), true, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            back.scan_id, gap_id,
            "Display-only lookup may explicitly show a partial result"
        );
        let response = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        assert_eq!(response.scan_id, complete_id);
        assert!(response.cache_hit);
        assert_eq!(response.status, "complete");
        assert_eq!(response.index_policy_version, 0);
        assert_eq!(
            registry().lock().unwrap().active.as_deref(),
            Some(active_id.as_str())
        );
        assert!(!active.cancelled.load(Ordering::Relaxed));
        gap.snapshot.lock().unwrap().freshness = "updating".into();
        let again = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        assert_eq!(again.scan_id, complete_id);
        {
            let mut reg = registry().lock().unwrap();
            reg.active = previous_active;
            for id in [source_id, active_id, cancelled_id, gap_id, complete_id] {
                reg.runs.remove(&id);
            }
        }
        fs::remove_file(file).unwrap();
    }
    #[test]
    fn legacy_exact_scope_with_missing_current_layer_migrates_only_when_selected_as_child() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-policy-migration-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let selected = root.join("selected");
        fs::create_dir_all(selected.join("known-missing-child")).unwrap();
        fs::write(selected.join("known-missing-child/data"), [1u8; 4096]).unwrap();
        let source = fixture_run(&root);
        let source_id = unique_id(&source, "policy-parent");
        source.snapshot.lock().unwrap().status = "complete".into();
        source.snapshot.lock().unwrap().freshness = "verified".into();
        let legacy = fixture_run(&selected);
        let legacy_id = unique_id(&legacy, "policy-legacy-exact");
        let key = cache_key(std::slice::from_ref(&selected), true);
        {
            let mut old = legacy.snapshot.lock().unwrap();
            old.status = "complete".into();
            old.freshness = "verified".into();
            old.index_policy_version = 0;
            old.scanned_directories = 2;
            let mut row = new_entry(&selected, true);
            row.directory_count = 2;
            row.complete = true;
            old.entries.push(row);
        }
        let (previous_active, previous_runs) = {
            let mut reg = registry().lock().unwrap();
            let previous_runs = std::mem::take(&mut reg.runs);
            reg.runs.insert(source_id.clone(), source.clone());
            reg.runs.insert(legacy_id.clone(), legacy.clone());
            (reg.active.take(), previous_runs)
        };
        assert!(
            !snapshot_view(&legacy, &string(&selected))
                .unwrap()
                .view_detail_complete
        );
        let same_scope = inspect_disk_directory(legacy_id.clone(), string(&selected)).unwrap();
        assert_eq!(same_scope.scan_id, legacy_id);
        assert_eq!(same_scope.index_policy_version, 0);
        assert!(registry().lock().unwrap().active.is_none());
        let file = cache_file(&key).unwrap();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            serde_json::to_vec(&cache_value(&key, &legacy)).unwrap(),
        )
        .unwrap();
        let other = fixture_run(&root.join("other-active"));
        let other_id = unique_id(&other, "policy-other-worker");
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.remove(&legacy_id);
            reg.runs.insert(other_id.clone(), other.clone());
            for n in 0..2 {
                let filler = fixture_run(&root.join(format!("unrelated-{n}")));
                filler.snapshot.lock().unwrap().status = "complete".into();
                filler.snapshot.lock().unwrap().freshness = "verified".into();
                let id = unique_id(&filler, &format!("policy-filler-{n}"));
                reg.runs.insert(id, filler);
            }
            reg.active = Some(other_id.clone());
            assert_eq!(reg.runs.len(), MAX_RESIDENT_RUNS);
        }
        let busy = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap_err();
        assert!(busy.contains("already running"));
        assert!(!other.cancelled.load(Ordering::Relaxed));
        {
            let mut reg = registry().lock().unwrap();
            assert!(reg.runs.contains_key(&source_id));
            assert!(reg.runs.contains_key(&legacy_id));
            assert!(reg.runs.len() <= MAX_RESIDENT_RUNS);
            assert_eq!(reg.active.as_deref(), Some(other_id.as_str()));
            // Simulate returning after the caller stopped the unrelated worker.
            reg.active = None;
            reg.runs.remove(&other_id);
        }
        let response = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        assert_ne!(response.scan_id, legacy_id);
        let deadline = Instant::now() + Duration::from_secs(5);
        let finished = loop {
            let current = get_disk_scan(response.scan_id.clone()).unwrap();
            if current.status != "running" {
                break current;
            }
            assert!(Instant::now() < deadline, "owned scope upgrade timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(finished.status, "complete");
        assert_eq!(finished.roots, vec![string(&selected)]);
        assert_eq!(finished.index_policy_version, INDEX_POLICY_VERSION);
        assert_eq!(finished.scanned_files, 1);
        // A second request that observed legacy just before this completion
        // cannot start a redundant upgrade of the same exact scope.
        let stale_upgrade =
            start_disk_scan_impl(Some(string(&selected)), true, true, Some(&source_id)).unwrap();
        assert_eq!(stale_upgrade.scan_id, finished.scan_id);
        assert!(stale_upgrade.cache_hit);
        assert!(registry().lock().unwrap().active.is_none());
        assert!(
            get_disk_scan_view(finished.scan_id.clone(), string(&selected))
                .unwrap()
                .view_detail_complete
        );
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.remove(&legacy_id);
            reg.runs.remove(&finished.scan_id);
        }
        let restored = inspect_disk_directory(source_id.clone(), string(&selected)).unwrap();
        assert_eq!(restored.scan_id, finished.scan_id);
        assert_eq!(restored.index_policy_version, INDEX_POLICY_VERSION);
        assert!(restored.cache_hit);
        {
            let mut reg = registry().lock().unwrap();
            reg.active = previous_active;
            reg.runs = previous_runs;
            reg.cached.remove(&key);
        }
        let _ = fs::remove_file(cache_file(&key).unwrap());
        let _ = fs::remove_file(checkpoint_file(&key).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn index_limit_preserves_exact_totals_and_marks_only_affected_layer_incomplete() {
        let root = fixture();
        fs::create_dir(root.join("nested/child/deepest")).unwrap();
        fs::create_dir_all(root.join("other/deep")).unwrap();
        let run = fixture_run(&root);
        run.index_budget.lock().unwrap().general.rows = usize::MAX;
        {
            let mut budget = run.index_budget.lock().unwrap();
            budget.shallow.rows = RESERVED_SHALLOW_DIRECTORIES;
            budget.direct.rows = RESERVED_DIRECT_DIRECTORIES;
        }
        scan_roots(run.clone(), vec![root.clone()]);
        let s = run.snapshot.lock().unwrap();
        assert_eq!(s.status, "complete");
        assert!(s.coverage_complete);
        assert!(s.issues.iter().any(|i| i.kind == "index-limit"));
        assert!(!run.index_complete.load(Ordering::Relaxed));
        assert_eq!(s.scanned_files, 1);
        assert_eq!(s.logical_bytes, 16384);
        assert_eq!(
            s.entries
                .iter()
                .find(|e| e.path == string(&root.join("nested")))
                .unwrap()
                .logical_bytes,
            16384
        );
        drop(s);
        let root_view = snapshot_view(&run, &string(&root)).unwrap();
        assert_eq!(root_view.scanned_directories, 6);
        assert_eq!(
            root_view
                .entries
                .iter()
                .filter(|e| e.kind != "file" && e.parent_path == string(&root))
                .map(|e| e.directory_count)
                .sum::<u64>(),
            5
        );
        assert!(root_view.view_detail_complete);
        let affected = snapshot_view(&run, &string(&root.join("nested/child"))).unwrap();
        assert_eq!(affected.scanned_directories, 2);
        assert!(!affected.view_detail_complete);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn oversized_index_falls_back_to_small_prioritized_cache() {
        let root = fixture();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let key = cache_key(std::slice::from_ref(&root), true);
        let mut value = cache_value(&key, &run);
        let shallow_bytes = serde_json::to_vec(&Cache {
            index: HashMap::new(),
            index_complete: false,
            ..value.clone()
        })
        .unwrap()
        .len() as u64;
        let limit = shallow_bytes + 128;
        let data = cache_bytes(&mut value, limit).unwrap();
        assert!(data.len() as u64 <= limit);
        assert!(value.index.contains_key(&string(&root)));
        assert!(!value
            .index
            .contains_key(&string(&root.join("nested/child"))));
        assert!(!value.index_complete);
        assert_eq!(value.snapshot.scanned_files, 1);
        assert_eq!(value.snapshot.logical_bytes, 16384);
        assert!(cache_bytes(&mut value, 1).is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shared_cache_budget_keeps_many_small_snapshots_when_space_remains() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let directory = root.join("many-snapshots");
        let settings = root.join("settings/disk-cache-limit");
        fs::create_dir(&directory).unwrap();
        for index in 0..160 {
            fs::write(directory.join(format!("scope-{index}.json")), b"{}").unwrap();
        }
        for tier in [64, 256, 512] {
            let response = set_disk_cache_limit_at(&directory, &settings, tier).unwrap();
            assert!(response.used_bytes < tier * MIB);
            assert_eq!(cache_files(&directory, false).unwrap().len(), 160);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn disk_cache_limit_is_persisted_and_prunes_allocated_snapshot_bytes() {
        use std::io::Write;
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let directory = root.join("snapshots");
        let settings = root.join("settings/disk-cache-limit");
        fs::create_dir(&directory).unwrap();
        assert_eq!(read_cache_limit(&settings), 256);
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, b"128\n").unwrap();
        assert_eq!(read_cache_limit(&settings), 256);
        assert!(!supported_cache_limit(128));
        assert!(supported_cache_limit(512));
        let names = ["old.json", "recent.json", "scope.partial.json"];
        let chunk = vec![0x5a; MIB as usize];
        for name in names {
            let mut file = fs::File::create(directory.join(name)).unwrap();
            for _ in 0..24 {
                file.write_all(&chunk).unwrap();
            }
            file.sync_all().unwrap();
        }
        let before: u64 = cache_files(&directory, false)
            .unwrap()
            .iter()
            .map(|file| file.bytes)
            .sum();
        assert!(before > 64 * MIB);
        assert!(before <= 128 * MIB);
        let response = set_disk_cache_limit_at(&directory, &settings, 64).unwrap();
        assert!(!directory.join("scope.partial.json").exists());
        assert!(directory.join("old.json").exists());
        assert!(directory.join("recent.json").exists());
        let measured: u64 = ["old.json", "recent.json"]
            .iter()
            .map(|name| allocated(&fs::metadata(directory.join(name)).unwrap()))
            .sum();
        assert_eq!(response.used_bytes, measured);
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::json!({"limitMiB": 64, "usedBytes": measured})
        );
        assert_eq!(read_cache_limit(&settings), 64);
        assert!(set_disk_cache_limit(96).is_err());
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(read_cache_limit(&settings), 64);
        assert!(cache_files(&directory, false).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn disk_cache_budget_counts_checkpoints_and_evicts_old_entries() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let directory = root.join("cache-budget");
        fs::create_dir(&directory).unwrap();
        let old = directory.join("old.json");
        let partial = directory.join("scope.partial.json");
        let keep = directory.join("recent.json");
        fs::write(&old, [0u8; 32]).unwrap();
        fs::write(&partial, [0u8; 32]).unwrap();
        fs::write(&keep, [0u8; 32]).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
        let block_bytes = allocated(&fs::metadata(&keep).unwrap());
        trim_cache_directory(&directory, Some(&keep), block_bytes * 2, 2).unwrap();
        assert!(!partial.exists());
        assert!(old.exists());
        assert!(keep.exists());
        trim_cache_directory(&directory, Some(&keep), block_bytes, 1).unwrap();
        assert!(!old.exists());
        assert!(keep.exists());
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .flatten()
                .map(|e| e.metadata().unwrap().len())
                .sum::<u64>(),
            32
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancellation_synchronously_saves_progress_and_finished_runs_keep_last_good() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let run = fixture_run(&root);
        let id = unique_id(&run, "cancel-api");
        scan_roots(run.clone(), vec![root.clone()]);
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(id.clone(), run.clone());
            reg.active.replace(id.clone())
        };
        write_cache(&key, &run, false);
        let good_bytes = fs::read(cache_file(&key).unwrap()).unwrap();
        // Cancelling a finished source publishes it without changing its status.
        let finished = cancel_disk_scan(id.clone()).unwrap();
        assert_eq!(finished.status, "complete");
        assert!(!run.cancelled.load(Ordering::Relaxed));
        assert!(!checkpoint_file(&key).unwrap().exists());
        run.snapshot.lock().unwrap().status = "running".into();
        run.finalized.store(false, Ordering::Relaxed);
        registry().lock().unwrap().active = Some(id.clone());
        cancel_disk_scan(id.clone()).unwrap();
        assert!(run.cancelled.load(Ordering::Relaxed));
        let partial = read_cache_at(&checkpoint_file(&key).unwrap(), &key, true).unwrap();
        assert_eq!(partial.snapshot.scanned_files, 1);
        assert_eq!(partial.snapshot.status, "cancelled");
        assert_eq!(fs::read(cache_file(&key).unwrap()).unwrap(), good_bytes);
        for status in ["cancelled", "failed"] {
            run.finalized.store(false, Ordering::Relaxed);
            registry().lock().unwrap().active = Some(id.clone());
            run.snapshot.lock().unwrap().status = status.into();
            finish_run(&key, &run);
            assert!(registry().lock().unwrap().active.is_none());
            assert_eq!(fs::read(cache_file(&key).unwrap()).unwrap(), good_bytes);
        }
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.remove(&id);
            reg.cached.remove(&key);
            reg.active = previous_active;
        }
        let _ = fs::remove_file(cache_file(&key).unwrap());
        let _ = fs::remove_file(checkpoint_file(&key).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn basic_snapshot_policy_rejects_protected_path_without_reading_it() {
        let root = home().unwrap();
        let mut run = fixture_run(&root);
        Arc::get_mut(&mut run).unwrap().allow_protected = false;
        assert!(
            snapshot_view(&run, &string(&root.join("Documents/nonexistent-fixture")))
                .unwrap_err()
                .starts_with("permission-required:")
        );
        assert!(
            snapshot_view(&run, &string(&root.join("projects/archive.photoslibrary")))
                .unwrap_err()
                .starts_with("permission-required:")
        );
    }
    #[test]
    #[ignore = "focused cache size benchmark; synthetic directory summaries only"]
    fn benchmark_compact_directory_cache_size() {
        let root = fixture();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let key = cache_key(std::slice::from_ref(&root), true);
        let mut value = cache_value(&key, &run);
        for n in 0..MAX_INDEX_DIRECTORIES.saturating_sub(value.index.len()) {
            let path = root.join(format!(
                "project-{}/target/debug/deps/directory-{n}",
                n / 100
            ));
            let mut row = new_entry(&path, true);
            row.allocated_bytes = 4096;
            row.file_count = 3;
            row.directory_count = 1;
            row.complete = true;
            value.index.insert(string(&path), row);
        }
        let uncompressed_entry_json_bytes = serde_json::to_vec(&value.index).unwrap().len();
        let started = Instant::now();
        let data = cache_bytes(&mut value, MAX_CACHE_FILE_BYTES).unwrap();
        let serialize_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let decoded: Cache = serde_json::from_slice(&data).unwrap();
        let deserialize_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(decoded.index.len(), MAX_INDEX_DIRECTORIES);
        println!(
            "{}",
            serde_json::json!({"directories": decoded.index.len(), "compactCacheBytes": data.len(),
            "fullEntryIndexJsonBytes": uncompressed_entry_json_bytes, "serializeMs": serialize_ms, "deserializeMs": deserialize_ms,
            "singleFileLimitBytes": MAX_CACHE_FILE_BYTES, "allCacheLimitBytes": DEFAULT_CACHE_LIMIT_MIB * MIB})
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn running_root_view_uses_current_shallow_progress_and_inflight_children() {
        let root = fixture();
        let run = fixture_run(&root);
        let mut stale_root = new_entry(&root, true);
        stale_root.allocated_bytes = 4096;
        run.index
            .lock()
            .unwrap()
            .insert(string(&root), stale_root.clone());
        stale_root.allocated_bytes = 32768;
        stale_root.logical_bytes = 16384;
        stale_root.file_count = 1;
        let mut inflight = new_entry(&root.join("nested"), true);
        inflight.allocated_bytes = 28672;
        run.snapshot.lock().unwrap().entries = vec![stale_root, inflight];
        let view = snapshot_view(&run, &string(&root)).unwrap();
        assert_eq!(view.allocated_bytes, 32768);
        assert_eq!(view.logical_bytes, 16384);
        assert_eq!(view.scanned_files, 1);
        assert!(view
            .entries
            .iter()
            .any(|e| e.path == string(&root.join("nested"))));
        assert!(!view.view_detail_complete);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn inflight_run_survives_its_own_checkpoint_restore_and_remains_cancellable() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-inflight-fixture-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let run = fixture_run(&root);
        let id = unique_id(&run, "inflight");
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(id.clone(), run.clone());
            reg.active.replace(id.clone())
        };
        write_cache(&key, &run, true);
        let response = start_disk_scan_with_options(Some(string(&root)), false, true).unwrap();
        assert_eq!(response.status, "running");
        assert!(!response.cache_hit);
        assert!(Arc::ptr_eq(
            registry().lock().unwrap().runs.get(&id).unwrap(),
            &run
        ));
        cancel_disk_scan(id.clone()).unwrap();
        assert!(run.cancelled.load(Ordering::Relaxed));
        // A checkpoint read after cancellation must still not replace the source Run.
        let _ = start_disk_scan_with_options(Some(string(&root)), false, true).unwrap();
        assert!(Arc::ptr_eq(
            registry().lock().unwrap().runs.get(&id).unwrap(),
            &run
        ));
        assert!(
            start_disk_scan_with_options(Some(string(&root.join("other"))), false, true)
                .unwrap_err()
                .contains("already running")
        );
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.remove(&id);
            reg.cached.remove(&key);
            reg.active = previous_active;
        }
        let _ = fs::remove_file(checkpoint_file(&key).unwrap());
    }
    #[test]
    fn coverage_gap_does_not_hide_complete_current_layer_detail() {
        let root = fixture();
        let run = fixture_run(&root);
        scan_roots(run.clone(), vec![root.clone()]);
        let path = string(&root.join("nested"));
        run.index.lock().unwrap().get_mut(&path).unwrap().complete = false;
        run.snapshot
            .lock()
            .unwrap()
            .entries
            .iter_mut()
            .find(|e| e.path == path)
            .unwrap()
            .complete = false;
        let view = snapshot_view(&run, &path).unwrap();
        assert!(view.view_detail_complete);
        assert!(!view.coverage_complete);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shallow_progress_and_checkpoint_stay_small_with_many_long_directory_names() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let run = fixture_run(&root);
        let id = unique_id(&run, "shallow-budget");
        let key = cache_key(std::slice::from_ref(&root), true);
        {
            let mut s = run.snapshot.lock().unwrap();
            let mut summary = new_entry(&root, true);
            summary.allocated_bytes = 4096;
            summary.directory_count = 1;
            s.entries.push(summary);
            s.allocated_bytes = 4096;
            s.scanned_directories = 1;
        }
        let mut p = Progress {
            root: root.clone(),
            ..Progress::default()
        };
        let mut active = HashMap::new();
        for n in 0..50_001 {
            let path = root.join(format!("{n:06}-{}", "x".repeat(220)));
            let mut row = new_entry(&path, true);
            row.allocated_bytes = 4096;
            row.directory_count = 1;
            active.insert(path.clone(), row);
            fold_directory(&path, &root, &mut active, &mut HashSet::new(), &mut p);
        }
        assert!(p.rows.len() <= MAX_SHALLOW_DIRECTORY_ROWS);
        assert!(p.row_bytes <= MAX_SHALLOW_BYTES);
        p.dirs = 50_001;
        p.bytes = 50_001 * 4096;
        flush(&run, &mut p);
        let snapshot = run.snapshot.lock().unwrap().clone();
        assert!(snapshot.entries.len() <= MAX_SHALLOW_DIRECTORY_ROWS);
        assert_eq!(snapshot.scanned_directories, 50_002);
        assert_eq!(snapshot.allocated_bytes, 50_002 * 4096);
        assert!(snapshot.issues.iter().any(|i| i.kind == "summary-limit"));
        assert!(!coverage_issues(&run, &snapshot));
        assert!(serde_json::to_vec(&snapshot).unwrap().len() < 2 * MAX_SHALLOW_BYTES);
        let previous_active = registry().lock().unwrap().active.replace(id);
        write_cache(&key, &run, true);
        let checkpoint = read_cache(&key).unwrap();
        assert_eq!(checkpoint.snapshot.scanned_directories, 50_002);
        assert_eq!(checkpoint.snapshot.allocated_bytes, 50_002 * 4096);
        registry().lock().unwrap().active = previous_active;
        let _ = fs::remove_file(checkpoint_file(&key).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn refresh_publishes_terminal_generation_before_attempting_new_scan() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-finalization-fixture-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let run = fixture_run(&root);
        let id = unique_id(&run, "finalization");
        {
            let mut s = run.snapshot.lock().unwrap();
            s.status = "complete".into();
            s.freshness = "verified".into();
            s.scanned_files = 17;
            s.logical_bytes = 1234;
        }
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous_active = {
            let mut reg = registry().lock().unwrap();
            reg.runs.insert(id.clone(), run.clone());
            reg.active.replace(id.clone())
        };
        // The fresh scan's nonexistent selected path fails validation. The completed
        // generation must already be durable when that new attempt is made.
        assert!(start_disk_scan_with_options(Some(string(&root)), true, true).is_err());
        assert!(run.finalized.load(Ordering::Relaxed));
        assert!(registry().lock().unwrap().active.is_none());
        let good = read_cache(&key).unwrap();
        assert_eq!(good.snapshot.scan_id, id);
        assert_eq!(good.snapshot.scanned_files, 17);
        assert_eq!(good.snapshot.logical_bytes, 1234);
        let bytes = fs::read(cache_file(&key).unwrap()).unwrap();
        finish_run(&key, &run);
        assert_eq!(fs::read(cache_file(&key).unwrap()).unwrap(), bytes);
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.remove(&id);
            reg.cached.remove(&key);
            reg.active = previous_active;
        }
        let _ = fs::remove_file(cache_file(&key).unwrap());
    }
    #[test]
    #[cfg(unix)]
    fn cache_reads_and_recency_updates_reject_symlinks_and_oversized_files() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let target = root.join("target.json");
        fs::write(&target, b"{}").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
        let link = root.join("link.json");
        symlink(&target, &link).unwrap();
        assert!(read_cache_bytes(&link).is_none());
        touch_cache(&link);
        assert_eq!(
            fs::metadata(&target).unwrap().modified().unwrap(),
            UNIX_EPOCH
        );
        let oversized = root.join("oversized.json");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_CACHE_FILE_BYTES + 1)
            .unwrap();
        assert!(read_cache_bytes(&oversized).is_none());
        assert!(read_cache_bytes(&root).is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn primary_application_lists_filter_helpers_before_applying_the_limit() {
        let root = fixture();
        let run = fixture_run(&root);
        {
            let mut snapshot = run.snapshot.lock().unwrap();
            // Put helpers first to prove they cannot exhaust the application budget.
            for n in 0..600 {
                snapshot.entries.push(new_entry(
                    &root.join(format!("Host.app/Contents/Helpers/Worker-{n}.app")),
                    true,
                ));
            }
            for n in 0..200 {
                snapshot
                    .entries
                    .push(new_entry(&root.join(format!("Installed-{n}.app")), true));
            }
        }
        let applications = application_attribution(&run);
        assert_eq!(applications.len(), 200);
        assert!(applications
            .iter()
            .all(|app| primary_application(Path::new(&app.path))));
        assert!(!shallow_directory(
            &root.join("Host.app/Contents/Helpers/Worker.app"),
            &root,
            false
        ));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn application_attribution_excludes_cache_usage_in_fresh_and_saved_scans() {
        let root = fixture();
        let run = fixture_run(&root);
        let library = home().unwrap().join("Library");
        let support = library.join("Application Support/Fixture");
        let cache = library.join("Caches/Fixture");
        let container = library.join("Containers/Fixture");
        {
            let mut snapshot = run.snapshot.lock().unwrap();
            snapshot.status = "complete".into();
            snapshot.freshness = "verified".into();
            snapshot
                .entries
                .push(new_entry(&root.join("Fixture.app"), true));
            for (path, bytes) in [(&support, 20), (&cache, 80), (&container, 10)] {
                let mut row = new_entry(path, true);
                row.allocated_bytes = bytes;
                snapshot.entries.push(row);
            }
        }
        let applications = application_attribution(&run);
        assert_eq!(applications.len(), 1);
        let related = vec![string(&support), string(&container)];
        assert_eq!(applications[0].support_bytes, 30);
        assert_eq!(applications[0].associated_paths, related);
        run.snapshot.lock().unwrap().applications = applications;

        let key = cache_key(std::slice::from_ref(&root), true);
        let mut old = serde_json::to_value(cache_value(&key, &run)).unwrap();
        let old_app = &mut old["snapshot"]["applications"][0];
        old_app["cacheBytes"] = serde_json::json!(80);
        old_app["associatedPaths"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(string(&cache)));
        let file = root.join("saved-scan.json");
        fs::write(&file, serde_json::to_vec(&old).unwrap()).unwrap();
        let restored = read_cache_at(&file, &key, false).unwrap();
        let restored_app = &restored.snapshot.applications[0];
        assert_eq!(restored_app.support_bytes, 30);
        assert_eq!(restored_app.associated_paths, related);
        assert!(serde_json::to_value(restored_app)
            .unwrap()
            .get("cacheBytes")
            .is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn application_progress_excludes_nested_helper_bundles() {
        let root = fixture();
        let app = root.join("Applications/Host.app");
        fs::create_dir_all(app.join("Contents/Helpers/Worker.app")).unwrap();
        fs::write(app.join("Contents/Helpers/Worker.app/helper"), [1u8; 4096]).unwrap();
        let run = fixture_run(&root);
        scan_job(&run, &app, &root, &Mutex::new(HardlinkOwners::new()), true);
        let snapshot = run.snapshot.lock().unwrap();
        assert_eq!(snapshot.applications.len(), 1);
        assert_eq!(snapshot.applications[0].path, string(&app));
        drop(snapshot);
        let applications = application_attribution(&run);
        assert_eq!(applications.len(), 1);
        assert_eq!(applications[0].path, string(&app));
        assert!(run
            .index
            .lock()
            .unwrap()
            .contains_key(&string(&app.join("Contents/Helpers/Worker.app"))));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn app_progress_reserves_install_slots_while_reporting_other_copies() {
        let root = fixture();
        let run = fixture_run(&root);
        let user_applications = root.join("Applications");
        let mut p = Progress {
            user_applications: Some(user_applications.clone()),
            ..Progress::default()
        };
        for n in 0..=MAX_APPLICATIONS {
            let path = root.join(format!("Build-{n}.app"));
            p.app_candidates
                .push(DiskApplication::from(&new_entry(&path, true)));
        }
        let app = user_applications.join("Installed.app");
        p.app_candidates
            .push(DiskApplication::from(&new_entry(&app, true)));
        flush(&run, &mut p);
        let snapshot = run.snapshot.lock().unwrap();
        assert_eq!(snapshot.applications.len(), MAX_APPLICATIONS + 1);
        assert!(snapshot.application_copies_truncated);
        assert_eq!(snapshot.applications.last().unwrap().path, string(&app));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn old_app_cache_recovers_installs_from_the_directory_index() {
        let root = fixture();
        let run = fixture_run(&root);
        let mut snapshot = run.snapshot.lock().unwrap().clone();
        let app = new_entry(Path::new("/Applications/Fixture.app"), true);
        let copy = new_entry(&root.join("Fixture.app"), true);
        snapshot.applications = vec![DiskApplication::from(&copy); MAX_APPLICATIONS];
        let index = HashMap::from([(app.path.clone(), app.clone())]);
        restore_cached_applications(&mut snapshot, &index);
        restore_cached_applications(&mut snapshot, &index);
        assert_eq!(snapshot.applications.len(), MAX_APPLICATIONS + 1);
        assert_eq!(snapshot.applications.last().unwrap().path, app.path);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn nonrefresh_cache_read_publishes_new_terminal_instead_of_returning_old_last_good() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = home().unwrap().join(format!(
            ".macdisk-exact-terminal-fixture-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let baseline = fixture_run(&root);
        let old_id = unique_id(&baseline, "old-good");
        baseline.snapshot.lock().unwrap().status = "complete".into();
        baseline.snapshot.lock().unwrap().freshness = "verified".into();
        let key = cache_key(std::slice::from_ref(&root), true);
        let previous_active = registry().lock().unwrap().active.replace(old_id.clone());
        write_cache(&key, &baseline, false);
        let new_run = fixture_run(&root);
        let new_id = unique_id(&new_run, "new-terminal");
        {
            let mut s = new_run.snapshot.lock().unwrap();
            s.status = "complete".into();
            s.freshness = "verified".into();
            s.scanned_files = 234;
            s.logical_bytes = 3456;
        }
        {
            let mut reg = registry().lock().unwrap();
            reg.active = Some(new_id.clone());
            reg.runs.insert(new_id.clone(), new_run.clone());
        }
        let exact = start_disk_scan_with_options(Some(string(&root)), false, true).unwrap();
        assert_eq!(exact.scan_id, new_id);
        assert_eq!(exact.scanned_files, 234);
        assert!(new_run.finalized.load(Ordering::Relaxed));
        assert!(registry().lock().unwrap().active.is_none());
        // Disk-only lookup simulates a fresh core process without in-memory cache state.
        registry().lock().unwrap().cached.remove(&key);
        let disk_only = read_cache(&key).unwrap();
        assert_eq!(disk_only.snapshot.scan_id, new_id);
        assert_eq!(disk_only.snapshot.logical_bytes, 3456);
        {
            let mut reg = registry().lock().unwrap();
            reg.runs.remove(&new_id);
            reg.active = previous_active;
        }
        let _ = fs::remove_file(cache_file(&key).unwrap());
    }
    #[test]
    fn terminal_poll_and_cancel_join_cache_publication_before_returning() {
        let _guard = CACHE_TEST_LOCK.lock().unwrap();
        let root = fixture();
        let previous_active = registry().lock().unwrap().active.clone();
        let key = cache_key(std::slice::from_ref(&root), true);
        for cancel in [false, true] {
            let run = fixture_run(&root);
            let id = unique_id(
                &run,
                if cancel {
                    "terminal-cancel"
                } else {
                    "terminal-poll"
                },
            );
            scan_roots(run.clone(), vec![root.clone()]);
            {
                let mut reg = registry().lock().unwrap();
                reg.active = Some(id.clone());
                reg.runs.insert(id.clone(), run.clone());
            }
            let terminal = if cancel {
                cancel_disk_scan(id.clone()).unwrap()
            } else {
                get_disk_scan(id.clone()).unwrap()
            };
            assert_eq!(terminal.status, "complete");
            assert!(run.finalized.load(Ordering::Relaxed));
            let persisted = read_cache_at(&cache_file(&key).unwrap(), &key, false).unwrap();
            assert_eq!(persisted.snapshot.scan_id, id);
            assert!(!persisted.index.is_empty());
            assert!(!run.cancelled.load(Ordering::Relaxed));
            let mut reg = registry().lock().unwrap();
            assert!(reg.active.is_none());
            reg.runs.remove(&id);
            reg.cached.remove(&key);
        }
        registry().lock().unwrap().active = previous_active;
        let _ = fs::remove_file(cache_file(&key).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
