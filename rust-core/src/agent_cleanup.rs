//! Bounded, asynchronous cleanup of explicitly selected rebuildable caches.
//!
//! This module never derives deletion authority from a scan. Plans stay in this
//! process; journals are evidence and recovery maps, never executable requests.
mod git;
mod goal;
mod incoming_refs;
mod process_refs;

use crate::processes::{bounded_output, ps_command};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 100_000;
const MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_JOBS: usize = 32;
const MAX_ACTIVE: usize = 4;
const MAX_ISSUES: usize = 32;
const PREVIEW_SECONDS: u64 = 30;
const EXECUTE_SECONDS: u64 = 120;
const PLAN_SECONDS: u64 = 600;
const BATCH: usize = 256;
const BATCH_ALLOCATED_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardDisposition {
    ObservedActive,
    InspectionUnavailable,
}
impl GuardDisposition {
    fn name(self) -> &'static str {
        match self {
            Self::ObservedActive => "observed_active",
            Self::InspectionUnavailable => "inspection_unavailable",
        }
    }
}
#[derive(Debug)]
struct GuardFailure {
    disposition: GuardDisposition,
    message: String,
}
impl From<String> for GuardFailure {
    fn from(message: String) -> Self {
        Self {
            disposition: GuardDisposition::InspectionUnavailable,
            message,
        }
    }
}
impl From<&str> for GuardFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}
impl Identity {
    fn metadata(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        }
    }
    fn stat(m: &libc::stat) -> Self {
        #[cfg(target_os = "macos")]
        let (mtime, mtime_nsec, ctime, ctime_nsec) =
            (m.st_mtime, m.st_mtime_nsec, m.st_ctime, m.st_ctime_nsec);
        #[cfg(not(target_os = "macos"))]
        let (mtime, mtime_nsec, ctime, ctime_nsec) =
            (m.st_mtime, m.st_mtime_nsec, m.st_ctime, m.st_ctime_nsec);
        Self {
            dev: m.st_dev as u64,
            ino: m.st_ino,
            len: m.st_size as u64,
            mtime,
            mtime_nsec,
            ctime,
            ctime_nsec,
        }
    }
    fn same_object(&self, other: &Self) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
    // rename updates ctime on supported filesystems. Verify the pre-rename full
    // fingerprint, then all fields unaffected by the rename afterwards.
    fn same_after_rename(&self, other: &Self) -> bool {
        self.same_object(other)
            && self.len == other.len
            && self.mtime == other.mtime
            && self.mtime_nsec == other.mtime_nsec
    }
}

#[derive(Clone, Serialize)]
struct Entry {
    relative: PathBuf,
    identity: Identity,
    allocated: u64,
}
// Retention events per phase, never a whole-repository inventory or a sum of
// unique protected files across repeated phases.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetainedObservations {
    hard_links: u64,
    symlinks: u64,
    symlink_referents: u64,
    source_boundaries: u64,
    other_retentions: u64,
    git_tracked_entries: u64,
}
#[derive(Clone, Default, Serialize)]
struct ProtectionPhase {
    git: git::Observation,
    retained: RetainedObservations,
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevalidationObservations {
    attempts: u64,
    metadata_binding_checks: u64,
    successful_reloads: u64,
    failures: u64,
}
#[derive(Clone, Default, Serialize)]
struct ProtectionObservations {
    plan: ProtectionPhase,
    execution: ProtectionPhase,
    revalidation: RevalidationObservations,
}
#[derive(Clone, Serialize)]
struct Plan {
    id: String,
    root: PathBuf,
    kind: String,
    root_identity: Identity,
    directories: HashMap<PathBuf, Identity>,
    entries: Vec<Entry>,
    allocated: u64,
    logical: u64,
    skipped: u64,
    coordination_lock_files: u64,
    git_tracked_files: u64,
    protection: ProtectionPhase,
    issues: Vec<Value>,
    #[serde(skip)]
    created: Option<Instant>,
}
struct Job {
    value: Value,
    cancel: Arc<AtomicBool>,
}
#[derive(Default)]
struct Registry {
    jobs: HashMap<String, Job>,
    order: Vec<String>,
    plans: HashMap<String, Arc<Plan>>,
    execution: HashMap<String, String>,
}
#[cfg(test)]
type BatchHook = Arc<dyn Fn(usize) -> Result<(), String> + Send + Sync>;
struct Inner {
    journal: PathBuf,
    session: String,
    _session_lock: File,
    allow_cleanup: bool,
    registry: Mutex<Registry>,
    goals: Mutex<HashMap<String, String>>,
    #[cfg(test)]
    process_guard: Mutex<Option<Result<(), String>>>,
    #[cfg(test)]
    batch_hook: Mutex<Option<BatchHook>>,
}
#[derive(Clone)]
pub struct AgentCleanup {
    inner: Arc<Inner>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn terminal(v: &Value) -> bool {
    matches!(
        v["state"].as_str(),
        Some("ready" | "completed" | "partial" | "cancelled" | "failed" | "interrupted")
    )
}
fn issue(issues: &mut Vec<Value>, path: &Path, reason: impl AsRef<str>) {
    if issues.len() < MAX_ISSUES {
        issues.push(json!({"path":path.to_string_lossy(),"reason":reason.as_ref()}));
    }
}
fn cstr(s: &OsStr) -> Result<CString, String> {
    CString::new(s.as_bytes()).map_err(|_| "NUL in path".into())
}
fn os_error() -> String {
    std::io::Error::last_os_error().to_string()
}
fn open_child(parent: &File, name: &OsStr) -> Result<File, String> {
    let name = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn stat_child(parent: &File, name: &OsStr) -> Result<libc::stat, String> {
    let name = cstr(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(os_error());
    }
    Ok(unsafe { stat.assume_init() })
}
fn rename_child(from: &File, name: &OsStr, to: &File, target: &OsStr) -> Result<(), String> {
    let name = cstr(name)?;
    let target = cstr(target)?;
    if unsafe {
        libc::renameat(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(os_error())
    }
}
fn unlink_child(parent: &File, name: &OsStr, directory: bool) -> Result<(), String> {
    let name = cstr(name)?;
    if unsafe {
        libc::unlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    } == 0
    {
        Ok(())
    } else {
        Err(os_error())
    }
}

fn cloud_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.contains("/Library/CloudStorage/")
        || text.ends_with("/Library/CloudStorage")
        || text.contains("/Library/Mobile Documents/")
        || text.ends_with("/Library/Mobile Documents")
        || path.components().any(|p| {
            p.as_os_str() == "Dropbox" || p.as_os_str().to_string_lossy().starts_with("OneDrive")
        })
}
fn placeholder(metadata: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::darwin::fs::MetadataExt;
        metadata.st_flags() & 0x40000000 != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = metadata;
        false
    }
}
fn safe_root(path: &Path) -> Result<(PathBuf, File), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("Cleanup path must be absolute without . or .. components".into());
    }
    if cloud_path(path) {
        return Err("Cloud storage is retained; cleanup must not hydrate placeholders".into());
    }
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(|e| e.to_string())?;
    let mut prefix = PathBuf::from("/");
    for component in path.components() {
        if let Component::Normal(name) = component {
            dir = open_child(&dir, name)?;
            prefix.push(name);
            let metadata = dir.metadata().map_err(|e| e.to_string())?;
            if placeholder(&metadata) {
                return Err("Dataless/cloud path is retained".into());
            }
        }
    }
    Ok((prefix, dir))
}
fn validate_kind(kind: &str, root: &Path, handle: &File) -> Result<PathBuf, String> {
    validate_kind_observed(kind, root, handle, None)
}
fn validate_kind_observed(
    kind: &str,
    root: &Path,
    handle: &File,
    mut source_events: Option<&mut u64>,
) -> Result<PathBuf, String> {
    let cache_name = match kind {
        "rust_target" => "target",
        "go_build_cache" => "go-build",
        _ => return Err("Supported cleanup kinds are rust_target and go_build_cache".into()),
    };
    let cache = root
        .ancestors()
        .find(|p| p.file_name() == Some(OsStr::new(cache_name)))
        .ok_or_else(|| format!("{kind} requires a {cache_name} directory or recognized subtree"))?;
    let prefix = root
        .strip_prefix(cache)
        .map_err(|e| e.to_string())?
        .to_path_buf();
    if !prefix.as_os_str().is_empty() && !eligible(kind, &prefix, true) {
        return Err(
            "Unrecognized cache subtree retained; select a recognized rebuildable scope".into(),
        );
    }
    if kind == "rust_target" {
        let parent = cache.parent().ok_or("Missing project parent")?;
        let (_, parent_handle) = safe_root(parent)?;
        let manifest = stat_child(&parent_handle, OsStr::new("Cargo.toml"))?;
        if manifest.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err("Adjacent Cargo.toml must be an ordinary file".into());
        }
        for directory in root.ancestors().take_while(|p| p.starts_with(cache)) {
            let (_, dir) = safe_root(directory)?;
            if stat_child(&dir, OsStr::new("Cargo.toml")).is_ok()
                || stat_child(&dir, OsStr::new(".git")).is_ok()
            {
                if let Some(events) = source_events.as_deref_mut() {
                    *events += 1;
                }
                return Err("Source manifest/Git boundary inside cache retained".into());
            }
        }
    }
    if stat_child(handle, OsStr::new("Cargo.toml")).is_ok() {
        if let Some(events) = source_events {
            *events += 1;
        }
        return Err("Source manifest inside selected scope retained".into());
    }
    Ok(prefix)
}

fn go_hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn eligible(kind: &str, relative: &Path, is_dir: bool) -> bool {
    let parts: Vec<_> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    if parts
        .iter()
        .any(|p| p.starts_with(".msm-cleanup-") || matches!(p.as_ref(), ".git" | ".svn" | ".hg"))
    {
        return false;
    }
    if kind == "go_build_cache" {
        if is_dir {
            return parts.len() == 1 && go_hex(&parts[0], 2);
        }
        if parts.len() != 2 || !go_hex(&parts[0], 2) {
            return false;
        }
        let name = &parts[1];
        return name.len() == 66
            && name.is_ascii()
            && go_hex(&name[..64], 64)
            && (name.ends_with("-a") || name.ends_with("-d"))
            && name.starts_with(parts[0].as_ref());
    }
    if parts.is_empty() {
        return true;
    }
    if parts.len() == 1 && !is_dir {
        return matches!(parts[0].as_ref(), ".rustc_info.json" | "CACHEDIR.TAG");
    }
    let arch = parts[0].split('-').next().unwrap_or_default();
    matches!(parts[0].as_ref(), "debug" | "release")
        || (parts[0].split('-').count() >= 3
            && matches!(
                arch,
                "aarch64"
                    | "x86_64"
                    | "i686"
                    | "arm"
                    | "armv7"
                    | "armv7s"
                    | "thumbv6m"
                    | "thumbv7m"
                    | "thumbv7em"
                    | "thumbv8m.base"
                    | "thumbv8m.main"
                    | "riscv32gc"
                    | "riscv32imac"
                    | "riscv32imc"
                    | "riscv64gc"
                    | "riscv64imac"
                    | "powerpc"
                    | "powerpc64"
                    | "powerpc64le"
                    | "s390x"
                    | "wasm32"
                    | "wasm64"
                    | "loongarch64"
                    | "mips"
                    | "mips64"
                    | "sparc64"
                    | "nvptx64"
            ))
}

fn check_budget(cancel: &AtomicBool, start: Instant, seconds: u64) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    if start.elapsed() >= Duration::from_secs(seconds) {
        return Err(format!(
            "deadline exceeded ({seconds}s); partial work retained"
        ));
    }
    Ok(())
}

impl AgentCleanup {
    pub(crate) fn has_background_work(&self) -> bool {
        // Cleanup and verification workers own a service clone until they exit.
        Arc::strong_count(&self.inner) > 1
    }

    pub fn default_journal_dir() -> Result<PathBuf, String> {
        if cfg!(any(debug_assertions, feature = "benchmark")) {
            if let Some(root) = std::env::var_os("MSM_DISK_CACHE_ROOT") {
                return Ok(PathBuf::from(root).join("mac-storage-manager/agent-cleanup"));
            }
        }
        dirs::data_local_dir()
            .map(|p| p.join("mac-storage-manager/agent-cleanup"))
            .ok_or_else(|| "Cannot locate application support directory".into())
    }
    pub fn new(journal_dir: PathBuf, allow_cleanup: bool) -> Result<Self, String> {
        // The journal is owned by this service and is deliberately separate from
        // scanner caches. Never accept a caller-supplied journal via MCP tools.
        if !journal_dir.is_absolute() || journal_dir.to_str().is_none() {
            return Err("Journal directory must be absolute".into());
        }
        fs::create_dir_all(&journal_dir)
            .map_err(|e| format!("Cannot create cleanup journal: {e}"))?;
        let (journal_dir, _) = safe_root(&journal_dir)?;
        fs::set_permissions(&journal_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let session = id()?;
        let session_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(journal_dir.join(format!(".session-{session}.lock")))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(session_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(os_error());
        }
        let result = Self {
            inner: Arc::new(Inner {
                journal: journal_dir,
                session,
                _session_lock: session_lock,
                allow_cleanup,
                registry: Mutex::new(Registry::default()),
                goals: Mutex::new(HashMap::new()),
                #[cfg(test)]
                process_guard: Mutex::new(None),
                #[cfg(test)]
                batch_hook: Mutex::new(None),
            }),
        };
        result.restore_receipts()?;
        Ok(result)
    }
    pub fn preview(&self, params: &Value) -> Result<Value, String> {
        let kind = params
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("kind is required")?
            .to_string();
        if !matches!(kind.as_str(), "rust_target" | "go_build_cache") {
            return Err("Supported cleanup kinds are rust_target and go_build_cache".into());
        }
        let path = PathBuf::from(
            params
                .get("path")
                .and_then(Value::as_str)
                .ok_or("path is required")?,
        );
        if !path.is_absolute() {
            return Err("path must be absolute".into());
        }
        let job_id = id()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let value = json!({"jobId":job_id,"operation":"preview","state":"planning","path":path,"kind":kind,"startedAtMs":now_ms(),"cleanupEnabled":self.inner.allow_cleanup});
        self.insert(&job_id, value.clone(), cancel.clone())?;
        let service = self.clone();
        let worker_id = job_id.clone();
        std::thread::spawn(move || {
            match service.build_plan(&worker_id, &kind, &path, &cancel) {
                Ok(plan) => {
                    let value = json!({"jobId":worker_id,"planId":worker_id,"operation":"preview","state":"ready","path":plan.root,"kind":plan.kind,
                        "estimatedAllocatedBytes":plan.allocated,"estimatedLogicalBytes":plan.logical,"plannedFiles":plan.entries.len(),"retainedEntries":plan.skipped,"coordinationLockFiles":plan.coordination_lock_files,"gitTrackedFiles":plan.git_tracked_files,
                        "issues":plan.issues,"protectionEvidence":{"plan":plan.protection},"issueSampleLimit":MAX_ISSUES,"expiresAfterSeconds":PLAN_SECONDS,"maxEntries":MAX_ENTRIES,"previewDeadlineSeconds":PREVIEW_SECONDS,"executionDeadlineSeconds":EXECUTE_SECONDS,"cleanupEnabled":service.inner.allow_cleanup,
                        "warning":"Permanent deletion of selected rebuildable artifacts. Future builds must regenerate them and may download dependencies. Allocation is an estimate; only filesystem receipts measure the observed free-space change.","completedAtMs":now_ms()});
                    if let Err(error)=service.retain_plan(&worker_id,plan) {
                        service.publish(&worker_id,json!({"jobId":worker_id,"operation":"preview","path":path,"state":"failed","error":error}));return;
                    }
                    service.publish(&worker_id, value);
                }
                Err(error) => service.publish(&worker_id, json!({"jobId":worker_id,"operation":"preview","path":path,"state":if error=="cancelled" {"cancelled"} else {"failed"},"error":error,"completedAtMs":now_ms()})),
            }
        });
        Ok(value)
    }
    pub fn status(&self, job_id: &str) -> Result<Value, String> {
        let mut value = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Cleanup registry unavailable")?
            .jobs
            .get(job_id)
            .map(|j| j.value.clone())
            .ok_or("Unknown or expired cleanup job")?;
        if !terminal(&value)
            && value["ownerSession"]
                .as_str()
                .is_some_and(|owner| owner != self.inner.session)
        {
            // Foreign sessions publish small atomic receipts. Never inspect their
            // manifests or run their jobs when observing their current state.
            if let Ok(current) =
                read_receipt(&self.inner.journal.join(format!("{job_id}.json")), job_id)
            {
                value = current;
            }
            if !terminal(&value)
                && !value["ownerSession"]
                    .as_str()
                    .is_some_and(|owner| self.session_live(owner))
            {
                value["state"] = json!("interrupted");
                value["error"] = json!("Owning service stopped; operation was not replayed");
            }
        }
        Ok(value)
    }
    pub fn cancel(&self, job_id: &str) -> Result<Value, String> {
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Cleanup registry unavailable")?;
        let job = registry
            .jobs
            .get_mut(job_id)
            .ok_or("Unknown or expired cleanup job")?;
        if !terminal(&job.value)
            && job.value["ownerSession"]
                .as_str()
                .is_some_and(|owner| owner != self.inner.session)
        {
            return Err("Job belongs to another service; cancel it in its owning session".into());
        }
        if !terminal(&job.value) {
            job.cancel.store(true, Ordering::Relaxed);
            job.value["cancellationRequested"] = json!(true);
        }
        Ok(job.value.clone())
    }
    pub fn execute(&self, plan_id: &str) -> Result<Value, String> {
        self.execute_with_before(plan_id, || Ok(()))
    }
    /// Run coordination on the worker before any disk sample or mutation. The
    /// caller can settle overlapping audits without blocking the MCP request.
    pub fn execute_with_before<F, G>(&self, plan_id: &str, before: F) -> Result<Value, String>
    where
        F: FnOnce() -> Result<G, String> + Send + 'static,
        G: Send + 'static,
    {
        if !self.inner.allow_cleanup {
            return Err("Cleanup is read-only. Start the MCP server with --allow-cleanup to enable execution".into());
        }
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Cleanup registry unavailable")?;
        if let Some(existing) = registry.execution.get(plan_id) {
            return registry
                .jobs
                .get(existing)
                .map(|j| j.value.clone())
                .ok_or_else(|| "Execution receipt expired; this plan cannot be replayed".into());
        }
        let plan = registry
            .plans
            .get(plan_id)
            .cloned()
            .ok_or("Unknown plan; create a fresh cleanup preview")?;
        if plan
            .created
            .is_none_or(|t| t.elapsed() > Duration::from_secs(PLAN_SECONDS))
        {
            return Err("Cleanup plan expired; create a fresh preview".into());
        }
        if registry
            .jobs
            .values()
            .filter(|j| {
                !terminal(&j.value)
                    && j.value["ownerSession"]
                        .as_str()
                        .is_none_or(|owner| owner == self.inner.session)
            })
            .count()
            >= MAX_ACTIVE
        {
            return Err("Cleanup worker limit reached; poll an existing job".into());
        }
        let job_id = id()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let value = json!({"jobId":job_id,"planId":plan_id,"operation":"execute","state":"running","path":plan.root,"startedAtMs":now_ms(),"deletedFiles":0,"deletedLogicalBytes":0,"deletedAllocatedBytes":0,"estimatedAllocatedBytes":plan.allocated});
        registry
            .execution
            .insert(plan_id.to_string(), job_id.clone());
        trim_registry(&mut registry);
        registry.order.push(job_id.clone());
        registry.jobs.insert(
            job_id.clone(),
            Job {
                value: value.clone(),
                cancel: cancel.clone(),
            },
        );
        drop(registry);
        let service = self.clone();
        let worker_id = job_id.clone();
        std::thread::spawn(move || {
            service.run_execution(&worker_id, &plan, &cancel, before);
        });
        Ok(value)
    }
    fn insert(&self, job_id: &str, value: Value, cancel: Arc<AtomicBool>) -> Result<(), String> {
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Cleanup registry unavailable")?;
        if registry
            .jobs
            .values()
            .filter(|j| {
                !terminal(&j.value)
                    && j.value["ownerSession"]
                        .as_str()
                        .is_none_or(|owner| owner == self.inner.session)
            })
            .count()
            >= MAX_ACTIVE
        {
            return Err("Cleanup worker limit reached; poll an existing job".into());
        }
        trim_registry(&mut registry);
        registry.order.push(job_id.to_string());
        registry
            .jobs
            .insert(job_id.to_string(), Job { value, cancel });
        Ok(())
    }
    fn publish(&self, job_id: &str, mut value: Value) {
        value["ownerSession"] = json!(self.inner.session);
        if let Ok(registry) = self.inner.registry.lock() {
            if let Some(previous) = registry
                .jobs
                .get(job_id)
                .and_then(|job| job.value.get("journalError"))
            {
                value["journalError"] = previous.clone();
                value["durabilityIncomplete"] = json!(true);
            }
        }
        if let Err(error) = self.save_json(&format!("{job_id}.json"), &value) {
            value["journalError"] = json!(error);
        }
        if let Ok(mut registry) = self.inner.registry.lock() {
            if let Some(job) = registry.jobs.get_mut(job_id) {
                job.value = value;
            }
        }
    }
    fn retain_plan(&self, job_id: &str, plan: Plan) -> Result<(), String> {
        const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
        let incoming = plan_memory(&plan);
        if incoming > MEMORY_LIMIT {
            return Err("Preview retained-memory budget exceeded; select a smaller recognized cache subtree".into());
        }
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Cleanup registry unavailable")?;
        while registry
            .plans
            .values()
            .map(|p| plan_memory(p))
            .sum::<usize>()
            .saturating_add(incoming)
            > MEMORY_LIMIT
        {
            let Some(old) = registry
                .order
                .iter()
                .find(|id| {
                    registry.plans.contains_key(*id) && !registry.execution.contains_key(*id)
                })
                .cloned()
            else {
                return Err("Cleanup plan-memory budget in use; wait for an active operation before previewing another scope".into());
            };
            registry.plans.remove(&old);
            if let Some(job) = registry.jobs.get_mut(&old) {
                job.value["state"] = json!("failed");
                job.value["error"] =
                    json!("Unused preview expired to release memory; create a fresh preview");
            }
        }
        registry.plans.insert(job_id.to_string(), Arc::new(plan));
        Ok(())
    }
    fn session_live(&self, session: &str) -> bool {
        if !valid_id(session) {
            return false;
        }
        if session == self.inner.session {
            return true;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.inner.journal.join(format!(".session-{session}.lock")))
        else {
            return false;
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            false
        } else {
            true
        }
    }
    fn save_json(&self, name: &str, value: &impl Serialize) -> Result<(), String> {
        let stem = name
            .strip_suffix(".plan.json")
            .or_else(|| name.strip_suffix(".coordination.json"))
            .or_else(|| name.strip_suffix(".json"))
            .ok_or("Invalid journal name")?;
        if !valid_id(stem) {
            return Err("Invalid journal identifier".into());
        }
        let mut counter = JsonCounter(0);
        serde_json::to_writer(&mut counter, value).map_err(|e| {
            format!("Journal manifest exceeds budget; select a smaller cache subtree: {e}")
        })?;
        let _lock = self.journal_lock()?;
        self.ensure_capacity(counter.0 + 1, name)?;
        let temp = self.inner.journal.join(format!(".{}-{}.tmp", name, id()?));
        let result = (|| {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)
                .map_err(|e| e.to_string())?;
            let mut writer = BufWriter::with_capacity(64 * 1024, file);
            serde_json::to_writer(&mut writer, value).map_err(|e| e.to_string())?;
            writer
                .write_all(b"\n")
                .and_then(|_| writer.flush())
                .and_then(|_| writer.get_ref().sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(&temp, self.inner.journal.join(name)).map_err(|e| e.to_string())?;
            File::open(&self.inner.journal)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    fn journal_lock(&self) -> Result<File, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.inner.journal.join(".journal.lock"))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(os_error());
        }
        Ok(file)
    }
    fn ensure_capacity(&self, required: u64, current_name: &str) -> Result<(), String> {
        const LIMIT: u64 = 64 * 1024 * 1024;
        let required = required.div_ceil(4096) * 4096 + 128 * 1024;
        let mut entries = Vec::new();
        let mut used = 0u64;
        for e in fs::read_dir(&self.inner.journal).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let m = e.path().symlink_metadata().map_err(|e| e.to_string())?;
            used = used.saturating_add(m.blocks() * 512);
            entries.push((e, m));
        }
        entries.sort_by_key(|(_, m)| m.modified().ok());
        for (entry, metadata) in entries {
            if used.saturating_add(required) <= LIMIT {
                return Ok(());
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if !valid_id(stem)
                || name == current_name
                || !metadata.is_file()
                || metadata.len() > 128 * 1024
            {
                continue;
            }
            let Ok(value) = read_receipt(&entry.path(), stem) else {
                continue;
            };
            if !matches!(
                value["state"].as_str(),
                Some("ready" | "completed" | "cancelled" | "failed")
            ) || value["recoveryFiles"].as_u64().unwrap_or(0) > 0
            {
                continue;
            }
            if value["ownerSession"]
                .as_str()
                .is_some_and(|s| s != self.inner.session && self.session_live(s))
            {
                continue;
            }
            // Names are derived from the validated filename, never journal JSON.
            let manifest = self.inner.journal.join(format!("{stem}.plan.json"));
            if let Ok(m) = manifest.symlink_metadata() {
                if m.is_file() && fs::remove_file(&manifest).is_ok() {
                    used = used.saturating_sub(m.blocks() * 512);
                }
            }
            let coordination = self.inner.journal.join(format!("{stem}.coordination.json"));
            if let Ok(m) = coordination.symlink_metadata() {
                if m.is_file() && fs::remove_file(&coordination).is_ok() {
                    used = used.saturating_sub(m.blocks() * 512);
                }
            }
            if fs::remove_file(entry.path()).is_ok() {
                used = used.saturating_sub(metadata.blocks() * 512);
            }
        }
        if used.saturating_add(required) > LIMIT {
            Err("Cleanup journal 64 MiB budget reached; recovery/live operation maps are retained. Review saved receipts before new cleanup.".into())
        } else {
            Ok(())
        }
    }
    fn restore_receipts(&self) -> Result<(), String> {
        let _lock = self.journal_lock()?;
        let mut files: Vec<_> = fs::read_dir(&self.inner.journal)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".json"))
                    .is_some_and(valid_id)
            })
            .collect();
        files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        for entry in files.into_iter().rev().take(MAX_JOBS).rev() {
            let stem = entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".json")
                .to_string();
            let Ok(mut value) = read_receipt(&entry.path(), &stem) else {
                continue;
            };
            if !terminal(&value)
                && !value["ownerSession"]
                    .as_str()
                    .is_some_and(|session| self.session_live(session))
            {
                value["state"] = json!("interrupted");
                value["error"]=json!("Service restarted. No operation is replayed; review the journal/recovery directory before a fresh preview.");
            }
            // Restored foreign jobs are read-only snapshots, not local workers.
            let mut registry = self
                .inner
                .registry
                .lock()
                .map_err(|_| "Cleanup registry unavailable")?;
            registry.order.push(stem.clone());
            registry.jobs.insert(
                stem.clone(),
                Job {
                    value: value.clone(),
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
            if value["operation"] == "execute" {
                if let Some(plan) = value["planId"].as_str().filter(|id| valid_id(id)) {
                    registry.execution.insert(plan.to_string(), stem);
                }
            }
        }
        Ok(())
    }
    fn build_plan(
        &self,
        plan_id: &str,
        kind: &str,
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<Plan, String> {
        self.build_plan_context(plan_id, kind, path, cancel, None)
    }
    fn build_plan_context(
        &self,
        plan_id: &str,
        kind: &str,
        path: &Path,
        cancel: &AtomicBool,
        goal_started: Option<Instant>,
    ) -> Result<Plan, String> {
        let start = Instant::now();
        let (root, handle) = safe_root(path)?;
        let scope_prefix = validate_kind(kind, &root, &handle)?;
        let root_metadata = handle.metadata().map_err(|e| e.to_string())?;
        let mut plan = Plan {
            id: plan_id.into(),
            root: root.clone(),
            kind: kind.into(),
            root_identity: Identity::metadata(&root_metadata),
            directories: HashMap::new(),
            entries: Vec::new(),
            allocated: 0,
            logical: 0,
            skipped: 0,
            coordination_lock_files: 0,
            git_tracked_files: 0,
            protection: ProtectionPhase::default(),
            issues: Vec::new(),
            created: Some(Instant::now()),
        };
        plan.directories
            .insert(PathBuf::new(), plan.root_identity.clone());
        let mut stack = vec![PathBuf::new()];
        let mut visited = 0usize;
        let mut path_bytes = 0usize;
        while let Some(relative) = stack.pop() {
            check_budget(cancel, start, PREVIEW_SECONDS)?;
            if let Some(started) = goal_started {
                check_budget(cancel, started, EXECUTE_SECONDS)?;
            }
            let directory = root.join(&relative);
            // Symlink-safe directory descriptors are used for validation; read_dir
            // is metadata-only and every planned deletion uses anchored handles.
            let directory_handle =
                open_planned_directory(&handle, &relative, &plan.directories, false, None)?;
            if !relative.as_os_str().is_empty()
                && (stat_child(&directory_handle, OsStr::new("Cargo.toml")).is_ok()
                    || stat_child(&directory_handle, OsStr::new(".git")).is_ok())
            {
                plan.skipped += 1;
                plan.protection.retained.source_boundaries += 1;
                issue(
                    &mut plan.issues,
                    &directory,
                    "Nested source manifest/Git boundary retained",
                );
                continue;
            }
            let entries = fs::read_dir(&directory)
                .map_err(|e| format!("Cannot enumerate cache; no executable partial plan: {e}"))?;
            for entry in entries {
                check_budget(cancel, start, PREVIEW_SECONDS)?;
                if let Some(started) = goal_started {
                    check_budget(cancel, started, EXECUTE_SECONDS)?;
                }
                visited += 1;
                if visited > MAX_ENTRIES {
                    return Err(format!(
                        "Preview exceeds {MAX_ENTRIES} entries; select a smaller rebuildable scope"
                    ));
                }
                let entry = entry.map_err(|e| e.to_string())?;
                let child_relative = relative.join(entry.file_name());
                path_bytes = path_bytes.saturating_add(child_relative.as_os_str().as_bytes().len());
                if path_bytes > MAX_PATH_BYTES {
                    return Err(
                        "Preview path-memory budget exceeded; no partial plan is executable".into(),
                    );
                }
                let child = root.join(&child_relative);
                if cargo_lock_name(entry.file_name().as_os_str()) {
                    plan.coordination_lock_files += 1;
                    continue;
                }
                let metadata = fs::symlink_metadata(&child).map_err(|e| e.to_string())?;
                if child_relative.to_str().is_none()
                    || metadata.file_type().is_symlink()
                    || placeholder(&metadata)
                    || metadata.dev() != plan.root_identity.dev
                    || cloud_path(&child)
                    || (!metadata.is_file() && !metadata.is_dir())
                    || !eligible(kind, &scope_prefix.join(&child_relative), metadata.is_dir())
                {
                    plan.skipped += 1;
                    if metadata.file_type().is_symlink() {
                        plan.protection.retained.symlinks += 1;
                    } else {
                        plan.protection.retained.other_retentions += 1;
                    }
                    issue(
                        &mut plan.issues,
                        &child,
                        "Symlink, cloud, mount, special or unrecognized boundary retained",
                    );
                    continue;
                }
                if metadata.is_dir() {
                    plan.directories
                        .insert(child_relative.clone(), Identity::metadata(&metadata));
                    stack.push(child_relative);
                } else if metadata.nlink() != 1 {
                    plan.skipped += 1;
                    plan.protection.retained.hard_links += 1;
                    issue(&mut plan.issues, &child, "Shared hard link retained");
                } else {
                    let allocated = metadata.blocks().saturating_mul(512);
                    plan.allocated = plan.allocated.saturating_add(allocated);
                    plan.logical = plan.logical.saturating_add(metadata.len());
                    plan.entries.push(Entry {
                        relative: child_relative,
                        identity: Identity::metadata(&metadata),
                        allocated,
                    });
                }
            }
        }
        let tracked = git::Tracked::load(&root)?;
        plan.protection.git = tracked.observation();
        plan.entries.retain(|entry| {
            if tracked.contains(&entry.relative, &entry.identity) {
                plan.git_tracked_files += 1;
                plan.protection.retained.git_tracked_entries += 1;
                plan.allocated = plan.allocated.saturating_sub(entry.allocated);
                plan.logical = plan.logical.saturating_sub(entry.identity.len);
                false
            } else {
                true
            }
        });
        Ok(plan)
    }
    #[cfg(test)]
    fn process_check(&self, root: &Path) -> Result<(), String> {
        self.process_check_detail(root)
            .map(|_| ())
            .map_err(|failure| failure.message)
    }
    fn execution_process_check(&self, root: &Path, receipt: &mut Value) -> Result<(), String> {
        self.process_check_detail(root)
            .map(|observation| {
                receipt["processReferenceCoverage"] = json!(observation);
                receipt["processReferenceCoverage"]["state"] = json!("scoped_process_observation");
            })
            .map_err(|failure| {
                receipt["guardDisposition"] = json!(failure.disposition.name());
                receipt["processReferenceCoverage"] =
                    json!({"state":failure.disposition.name(),"complete":false});
                failure.message
            })
    }
    fn execution_reference_check(
        &self,
        plan: &Plan,
        authorization_scope: &Path,
        receipt: &mut Value,
    ) -> Result<incoming_refs::Snapshot, String> {
        receipt["referenceInspectionAttempts"] =
            json!(receipt["referenceInspectionAttempts"].as_u64().unwrap_or(0) + 1);
        let checked = (|| {
            let (_, current) = safe_root(&plan.root)?;
            if !plan.root_identity.same_object(&Identity::metadata(
                &current.metadata().map_err(|e| e.to_string())?,
            )) {
                return Err("Selected cache namespace changed; remaining files retained".into());
            }
            let snapshot =
                incoming_refs::inspect(&plan.kind, &plan.root, Some(authorization_scope))?;
            self.execution_process_check(&plan.root, receipt)?;
            snapshot.revalidate()?;
            let (_, current) = safe_root(&plan.root)?;
            if !plan.root_identity.same_object(&Identity::metadata(
                &current.metadata().map_err(|e| e.to_string())?,
            )) {
                return Err(
                    "Selected cache namespace changed during reference inspection; retained".into(),
                );
            }
            Ok(snapshot)
        })();
        match checked {
            Ok(snapshot) => {
                receipt["incomingReferenceCoverage"] = json!(snapshot.observation());
                receipt["successfulReferenceChecks"] =
                    json!(receipt["successfulReferenceChecks"].as_u64().unwrap_or(0) + 1);
                Ok(snapshot)
            }
            Err(error) => {
                if receipt["guardDisposition"] != GuardDisposition::ObservedActive.name() {
                    receipt["guardDisposition"] =
                        json!(GuardDisposition::InspectionUnavailable.name());
                }
                receipt["incomingReferenceCoverage"] =
                    json!(incoming_refs::Observation::unavailable());
                Err(error)
            }
        }
    }
    fn process_check_detail(&self, root: &Path) -> Result<process_refs::Observation, GuardFailure> {
        #[cfg(test)]
        if let Some(result) = self
            .inner
            .process_guard
            .lock()
            .map_err(|_| "Process guard unavailable")?
            .clone()
        {
            return result
                .map(|_| process_refs::Observation::default())
                .map_err(GuardFailure::from);
        }
        let ps = bounded_output(
            ps_command()?.args(["-axo", "pid=,uid=,ruid=,svuid=,command="]),
            Duration::from_secs(3),
        )
        .map_err(|e| format!("Process inspection failed; retained: {e}"))?;
        if !ps.status.success() {
            return Err("Process inspection failed; retained".into());
        }
        let root_text = root
            .to_str()
            .ok_or("Non-UTF8 cache roots are not executable")?;
        for line in String::from_utf8_lossy(&ps.stdout).lines() {
            let trimmed = line.trim_start();
            let Some(split) = trimmed.find(char::is_whitespace) else {
                continue;
            };
            if trimmed[..split].parse::<u32>().ok() == Some(std::process::id()) {
                continue;
            }
            if trimmed[split..].contains(root_text) {
                return Err(GuardFailure {
                    disposition: GuardDisposition::ObservedActive,
                    message: "Live process command references the selected cache; retained".into(),
                });
            }
        }
        let output = bounded_output(
            Command::new("/usr/sbin/lsof").args(["-nP", "-Fpn"]),
            Duration::from_secs(8),
        )
        .map_err(|e| format!("Open-path inspection failed; retained: {e}"))?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(format!(
                "Open-path inspection incomplete; retained: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(300)
                    .collect::<String>()
            )
            .into());
        }
        let mut own = false;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(pid) = line.strip_prefix('p') {
                own = pid.parse::<u32>().ok() == Some(std::process::id());
            }
            if !own {
                if let Some(path) = line.strip_prefix('n') {
                    if Path::new(path).starts_with(root) {
                        return Err(GuardFailure {
                            disposition: GuardDisposition::ObservedActive,
                            message:
                                "Live process has an open path in the selected cache; retained"
                                    .into(),
                        });
                    }
                }
            }
        }
        process_refs::check(root, &ps.stdout)
    }
    fn run_execution<F, G>(&self, job_id: &str, plan: &Plan, cancel: &AtomicBool, before: F)
    where
        F: FnOnce() -> Result<G, String>,
    {
        self.run_execution_context(job_id, plan, cancel, before, ExecutionContext::default());
    }
    fn run_execution_context<F, G>(
        &self,
        job_id: &str,
        plan: &Plan,
        cancel: &AtomicBool,
        before: F,
        context: ExecutionContext,
    ) where
        F: FnOnce() -> Result<G, String>,
    {
        let started = context.started.unwrap_or_else(Instant::now);
        let mut receipt = self
            .status(job_id)
            .unwrap_or_else(|_| json!({"jobId":job_id}));
        receipt["ownerSession"] = json!(self.inner.session);
        if let Some(parent) = context.parent_goal.as_ref() {
            receipt["parentOperationId"] = json!(parent);
        }
        receipt["coordinationLocksCreated"] = json!(0);
        receipt["coordinationCreatedBindings"] = json!([]);
        receipt["coordinationLocksHeld"] = json!(0);
        receipt["coordinationDetailsComplete"] = json!(true);
        receipt["successfulReferenceChecks"] = json!(0);
        receipt["referenceInspectionAttempts"] = json!(0);
        receipt["processReferenceCoverage"] = json!({"state":"not_reached"});
        receipt["incomingReferenceCoverage"] = json!(incoming_refs::Observation::default());
        let mut protection = ProtectionObservations {
            plan: plan.protection.clone(),
            ..ProtectionObservations::default()
        };
        receipt["protectionEvidence"] = json!(protection);
        receipt["retainedEntries"] = json!(plan.skipped);
        receipt["issues"] = json!(plan.issues);
        receipt["issueSampleLimit"] = json!(MAX_ISSUES);
        receipt["journalPath"] = json!(self.inner.journal.join(format!("{job_id}.plan.json")));
        receipt["deletedPaths"] = json!([]);
        receipt["filesystemDeltaNote"]=json!("Observed signed free-space change includes unrelated concurrent writes, APFS snapshots, clones and delayed reclamation; it is not the estimated allocation or deleted logical bytes.");
        let mut issues = plan.issues.clone();
        let mut removed = 0u64;
        let mut logical = 0u64;
        let mut allocated = 0u64;
        let mut retained = plan.skipped;
        let mut recovery_files = 0u64;
        let mut batch_allocated = 0u64;
        let mut max_batch_allocated = 0u64;
        let mut largest_deleted_file = 0u64;
        let mut protected_git_files = plan.git_tracked_files;
        let mut coordination = None;
        let mut mutation_lock = None;
        let outcome = (|| -> Result<(), String> {
            check_budget(cancel, started, EXECUTE_SECONDS)?;
            if !context.locks_held {
                mutation_lock = Some(self.execution_lock(cancel, started)?);
            }
            coordination = Some(before()?);
            if plan
                .created
                .is_none_or(|at| at.elapsed() > Duration::from_secs(PLAN_SECONDS))
            {
                return Err("Cleanup plan expired while queued; fresh preview required".into());
            }
            check_budget(cancel, started, EXECUTE_SECONDS)?;
            let (_, root) = safe_root(&plan.root)?;
            if !plan.root_identity.same_object(&Identity::metadata(
                &root.metadata().map_err(|e| e.to_string())?,
            )) {
                return Err("Selected cache directory was replaced; retained".into());
            }
            validate_kind_observed(
                &plan.kind,
                &plan.root,
                &root,
                Some(&mut protection.execution.retained.source_boundaries),
            )?;
            // Refuse known active inputs before creating coordination metadata;
            // recheck after holding Cargo locks to cover intervening writers.
            self.execution_process_check(&plan.root, &mut receipt)?;
            let authorized_scope = context.authorization_scope.as_deref().unwrap_or(&plan.root);
            let cargo_paths = cargo_coordination_paths(plan)?;
            let quarantine_name = format!(".msm-cleanup-{job_id}");
            receipt["recoveryDirectory"] = json!(plan.root.join(&quarantine_name));
            receipt["coordinationLockFiles"] = json!(plan.coordination_lock_files);
            // Exact recovery map and lock-creation intent are durable BEFORE any
            // coordination inode or quarantine directory is created.
            self.save_json(&format!("{job_id}.plan.json"),&Manifest {job_id,plan_id:&plan.id,root:&plan.root,quarantine:&quarantine_name,entries:&plan.entries,coordination_scope:authorized_scope,coordination_locks:&cargo_paths,policy:"Never replay. Quarantine names are decimal entry indices. Empty .cargo-lock may be created only inside the authorized scope and is always retained."})?;
            self.save_json(&format!("{job_id}.json"), &receipt)?;
            let mut created_records = Vec::new();
            let acquired =
                CargoLocks::acquire(&cargo_paths, authorized_scope, &mut created_records);
            // Count the actual EXCL creations even if a later lock acquisition
            // fails before any artifact can be deleted.
            receipt["coordinationLocksCreated"] = json!(created_records.len());
            receipt["coordinationCreatedBindings"] = json!(created_records);
            receipt["coordinationDetailsComplete"] = json!(created_records
                .iter()
                .all(|row| row["identityRecorded"] == true));
            let mut detail_error = None;
            if acquired.is_ok() && !cargo_paths.is_empty() || !created_records.is_empty() {
                let details = (|| -> Result<(), String> {
                    let (_, authorized) = safe_root(authorized_scope)?;
                    let records = acquired
                        .as_ref()
                        .map(|locks| locks.records())
                        .unwrap_or_else(|_| created_records.clone());
                    self.save_json(&format!("{job_id}.coordination.json"),&json!({"jobId":job_id,"planId":plan.id,"parentOperationId":context.parent_goal,"root":plan.root,"rootIdentity":plan.root_identity,"authorizedScope":authorized_scope,"authorizedScopeIdentity":Identity::metadata(&authorized.metadata().map_err(|e|e.to_string())?),"acquisitionComplete":acquired.is_ok(),"locks":records}))?;
                    receipt["coordinationJournalPath"] = json!(self
                        .inner
                        .journal
                        .join(format!("{job_id}.coordination.json")));
                    Ok(())
                })();
                if let Err(error) = details {
                    receipt["coordinationDetailsComplete"] = json!(false);
                    receipt["coordinationEvidenceError"] =
                        json!(error.chars().take(180).collect::<String>());
                    detail_error = Some(error);
                }
            }
            let _cargo_locks = acquired.map_err(|failure| {
                receipt["guardDisposition"] = json!(failure.disposition.name());
                failure.message
            })?;
            if let Some(error) = detail_error {
                return Err(error);
            }
            receipt["coordinationLocksHeld"] = json!(cargo_paths.len());
            self.execution_process_check(&plan.root, &mut receipt)?;
            let mut tracked = git::Tracked::load(&plan.root).inspect_err(|_| {
                protection.execution.git = git::Observation::unavailable();
            })?;
            protection.execution.git = tracked.observation();
            receipt["diskBefore"] = disk_sample(&plan.root);
            receipt["protectionEvidence"] = json!(protection);
            self.publish(job_id, receipt.clone());
            let qname = cstr(OsStr::new(&quarantine_name))?;
            if unsafe { libc::mkdirat(root.as_raw_fd(), qname.as_ptr(), 0o700) } != 0 {
                return Err(format!(
                    "Cannot create private cleanup quarantine: {}",
                    os_error()
                ));
            }
            let quarantine = open_child(&root, OsStr::new(&quarantine_name))?;
            let mut last_check = Instant::now();
            let mut incoming_snapshot = None;
            for (index, entry) in plan.entries.iter().enumerate() {
                check_budget(cancel, started, EXECUTE_SECONDS)?;
                if index > 0 && index % BATCH == 0 {
                    update_counts(
                        &mut receipt,
                        removed,
                        logical,
                        allocated,
                        retained,
                        recovery_files,
                    );
                    receipt["issues"] = json!(issues);
                    receipt["protectionEvidence"] = json!(protection);
                    self.publish(job_id, receipt.clone());
                }
                if index > 0 && last_check.elapsed() > Duration::from_secs(2) {
                    let (_, current_root) = safe_root(&plan.root)?;
                    if !plan.root_identity.same_object(&Identity::metadata(
                        &current_root.metadata().map_err(|e| e.to_string())?,
                    )) {
                        return Err("Selected cache directory was replaced during execution; remaining files retained".into());
                    }
                    incoming_snapshot = Some(self.execution_reference_check(
                        plan,
                        authorized_scope,
                        &mut receipt,
                    )?);
                    check_budget(cancel, started, EXECUTE_SECONDS)?;
                    last_check = Instant::now();
                }
                if index % BATCH == 0
                    || batch_allocated >= BATCH_ALLOCATED_BYTES
                    || (batch_allocated > 0
                        && entry.allocated > BATCH_ALLOCATED_BYTES.saturating_sub(batch_allocated))
                {
                    #[cfg(test)]
                    if index > 0 {
                        let hook = self
                            .inner
                            .batch_hook
                            .lock()
                            .map_err(|_| "Batch test hook unavailable")?
                            .clone();
                        if let Some(hook) = hook {
                            hook(index)?;
                        }
                    }
                    _cargo_locks.revalidate()?;
                    validate_kind_observed(
                        &plan.kind,
                        &plan.root,
                        &root,
                        Some(&mut protection.execution.retained.source_boundaries),
                    )?;
                    protection.revalidation.attempts += 1;
                    if tracked.observation().state == git::ObservationState::RepositoryInspected {
                        protection.revalidation.metadata_binding_checks += 1;
                    }
                    let reloaded = tracked.revalidate().inspect_err(|_| {
                        protection.revalidation.failures += 1;
                    })?;
                    protection.revalidation.successful_reloads += u64::from(reloaded);
                    max_batch_allocated = max_batch_allocated.max(batch_allocated);
                    batch_allocated = 0;
                    incoming_snapshot = Some(self.execution_reference_check(
                        plan,
                        authorized_scope,
                        &mut receipt,
                    )?);
                    check_budget(cancel, started, EXECUTE_SECONDS)?;
                    last_check = Instant::now();
                    if let Some(goal) = &context.goal {
                        let available = goal.available()?;
                        receipt["goalAvailableBytes"] = json!(available);
                        if available >= goal.target {
                            receipt["goalReached"] = json!(true);
                            receipt["unprocessedFiles"] = json!(plan.entries.len() - index);
                            break;
                        }
                    }
                }
                if tracked.contains(&entry.relative, &entry.identity) {
                    protection.execution.retained.git_tracked_entries += 1;
                    protected_git_files += 1;
                    retained += 1;
                    issue(&mut issues, &entry.relative, "Git-tracked input retained");
                    continue;
                }
                if incoming_snapshot
                    .as_ref()
                    .ok_or("Fresh incoming-reference inspection missing; remaining files retained")?
                    .protects(&plan.root.join(&entry.relative), &entry.identity)
                {
                    protection.execution.retained.symlink_referents += 1;
                    retained += 1;
                    issue(
                        &mut issues,
                        &entry.relative,
                        "Incoming symbolic reference retained",
                    );
                    continue;
                }
                let parent_relative = entry.relative.parent().unwrap_or(Path::new(""));
                let source_events = protection.execution.retained.source_boundaries;
                let parent = match open_planned_directory(
                    &root,
                    parent_relative,
                    &plan.directories,
                    true,
                    Some(&mut protection.execution.retained.source_boundaries),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        if source_events == protection.execution.retained.source_boundaries {
                            protection.execution.retained.other_retentions += 1;
                        }
                        retained += 1;
                        issue(
                            &mut issues,
                            &entry.relative,
                            format!("Ancestor changed or unavailable: {e}"),
                        );
                        continue;
                    }
                };
                let name = entry
                    .relative
                    .file_name()
                    .ok_or("Missing planned filename")?;
                let current = match stat_child(&parent, name) {
                    Ok(s) => s,
                    Err(e) => {
                        protection.execution.retained.other_retentions += 1;
                        retained += 1;
                        issue(
                            &mut issues,
                            &entry.relative,
                            format!("Missing/unreadable before mutation: {e}"),
                        );
                        continue;
                    }
                };
                if current.st_mode & libc::S_IFMT != libc::S_IFREG
                    || current.st_nlink != 1
                    || entry.identity != Identity::stat(&current)
                {
                    if current.st_mode & libc::S_IFMT == libc::S_IFLNK {
                        protection.execution.retained.symlinks += 1;
                    } else if current.st_mode & libc::S_IFMT == libc::S_IFREG
                        && current.st_nlink != 1
                    {
                        protection.execution.retained.hard_links += 1;
                    } else {
                        protection.execution.retained.other_retentions += 1;
                    }
                    retained += 1;
                    issue(
                        &mut issues,
                        &entry.relative,
                        "Changed file retained; create a fresh preview",
                    );
                    continue;
                }
                let staged_name = index.to_string();
                check_budget(cancel, started, EXECUTE_SECONDS)?;
                if let Err(e) = rename_child(&parent, name, &quarantine, OsStr::new(&staged_name)) {
                    protection.execution.retained.other_retentions += 1;
                    retained += 1;
                    issue(
                        &mut issues,
                        &entry.relative,
                        format!("Cannot stage file: {e}"),
                    );
                    continue;
                }
                // The quarantine is private and each name is used exactly once.
                let staged = stat_child(&quarantine, OsStr::new(&staged_name));
                if !staged.as_ref().is_ok_and(|s| {
                    s.st_mode & libc::S_IFMT == libc::S_IFREG
                        && s.st_nlink == 1
                        && entry.identity.same_after_rename(&Identity::stat(s))
                }) {
                    protection.execution.retained.other_retentions += 1;
                    retained += 1;
                    recovery_files += 1;
                    // Do not overwrite a fresh arrival when restoring a replacement.
                    if restore_exclusive(&quarantine, OsStr::new(&staged_name), &parent, name)
                        .is_ok()
                    {
                        recovery_files -= 1;
                    }
                    issue(&mut issues,&entry.relative,"File changed during staging; preserved/restored or retained in recovery directory");
                    continue;
                }
                if let Err(error) = unlink_child(&quarantine, OsStr::new(&staged_name), false) {
                    protection.execution.retained.other_retentions += 1;
                    retained += 1;
                    recovery_files += 1;
                    if restore_exclusive(&quarantine, OsStr::new(&staged_name), &parent, name)
                        .is_ok()
                    {
                        recovery_files -= 1;
                    }
                    issue(
                        &mut issues,
                        &entry.relative,
                        format!(
                            "Deletion failed; preserved/restored or retained for recovery: {error}"
                        ),
                    );
                    continue;
                }
                removed += 1;
                logical = logical.saturating_add(entry.identity.len);
                allocated = allocated.saturating_add(entry.allocated);
                batch_allocated = batch_allocated.saturating_add(entry.allocated);
                largest_deleted_file = largest_deleted_file.max(entry.allocated);
            }
            let _ = unlink_child(&root, OsStr::new(&quarantine_name), true);
            Ok(())
        })();
        update_counts(
            &mut receipt,
            removed,
            logical,
            allocated,
            retained,
            recovery_files,
        );
        receipt["issues"] = json!(issues);
        receipt["protectionEvidence"] = json!(protection);
        receipt["maxBatchAllocatedBytes"] = json!(max_batch_allocated.max(batch_allocated));
        receipt["largestDeletedFileAllocatedBytes"] = json!(largest_deleted_file);
        receipt["gitTrackedFiles"] = json!(protected_git_files);
        receipt["batchAllocatedLimitBytes"] = json!(BATCH_ALLOCATED_BYTES);
        receipt["batchFileLimit"] = json!(BATCH);
        if removed > 0 {
            receipt["deletedPaths"] = json!([plan.root]);
        }
        receipt["state"] = json!("measuring");
        self.publish(job_id, receipt.clone());
        receipt["diskAfter"] = disk_sample(&plan.root);
        // This delay is on the background worker, never the MCP response path.
        std::thread::sleep(Duration::from_millis(200));
        receipt["diskFollowup"] = disk_sample(&plan.root);
        receipt["availableBytesDelta"] =
            signed_delta(&receipt["diskBefore"], &receipt["diskAfter"]);
        receipt["followupAvailableBytesDelta"] =
            signed_delta(&receipt["diskBefore"], &receipt["diskFollowup"]);
        receipt["state"] = json!(match outcome.as_ref() {
            Ok(()) if retained > 0 || recovery_files > 0 => "partial",
            Ok(()) => "completed",
            Err(e) if e == "cancelled" => "cancelled",
            Err(_) if removed > 0 => "partial",
            Err(_) => "failed",
        });
        if let Err(error) = outcome {
            receipt["failureDisposition"] = receipt
                .get("guardDisposition")
                .cloned()
                .unwrap_or(json!("execution_issue"));
            receipt["error"] = json!(error);
        }
        receipt["completedAtMs"] = json!(now_ms());
        receipt["elapsedMs"] = json!(started.elapsed().as_millis());
        self.publish(job_id, receipt);
        if let Ok(mut registry) = self.inner.registry.lock() {
            registry.plans.remove(&plan.id);
        }
        drop(coordination);
        drop(mutation_lock);
    }
    /// Background verification waits for publication, mutation and audit lease
    /// release; this method must never be called on the MCP request thread.
    pub(crate) fn verification_publication_barrier(&self) -> Result<(), String> {
        let guard = self.execution_lock(&AtomicBool::new(false), Instant::now())?;
        drop(guard);
        Ok(())
    }
    fn execution_lock(&self, cancel: &AtomicBool, started: Instant) -> Result<File, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.inner.journal.join(".execution.lock"))
            .map_err(|e| e.to_string())?;
        loop {
            check_budget(cancel, started, EXECUTE_SECONDS)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(file);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.to_string());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
fn update_counts(
    value: &mut Value,
    removed: u64,
    logical: u64,
    allocated: u64,
    retained: u64,
    recovery: u64,
) {
    value["deletedFiles"] = json!(removed);
    value["deletedLogicalBytes"] = json!(logical);
    value["deletedAllocatedBytes"] = json!(allocated);
    value["retainedEntries"] = json!(retained);
    value["recoveryFiles"] = json!(recovery);
}
fn open_planned_directory(
    root: &File,
    relative: &Path,
    directories: &HashMap<PathBuf, Identity>,
    protect_sources: bool,
    mut source_events: Option<&mut u64>,
) -> Result<File, String> {
    let mut dir = root.try_clone().map_err(|e| e.to_string())?;
    let mut prefix = PathBuf::new();
    if protect_sources && source_marker(&dir) {
        if let Some(events) = source_events.as_deref_mut() {
            *events += 1;
        }
        return Err("Source/Git boundary appeared in planned root; retained".into());
    }
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("Invalid planned relative path".into());
        };
        prefix.push(name);
        dir = open_child(&dir, name)?;
        if protect_sources && source_marker(&dir) {
            if let Some(events) = source_events.as_deref_mut() {
                *events += 1;
            }
            return Err("Source/Git boundary appeared in planned ancestor; retained".into());
        }
        let expected = directories
            .get(&prefix)
            .ok_or("Unplanned directory boundary")?;
        if !expected.same_object(&Identity::metadata(
            &dir.metadata().map_err(|e| e.to_string())?,
        )) {
            return Err("Directory identity changed".into());
        }
    }
    Ok(dir)
}
fn source_marker(directory: &File) -> bool {
    stat_child(directory, OsStr::new("Cargo.toml")).is_ok()
        || stat_child(directory, OsStr::new(".git")).is_ok()
}

fn restore_exclusive(from: &File, name: &OsStr, to: &File, target: &OsStr) -> Result<(), String> {
    let name = cstr(name)?;
    let target = cstr(target)?;
    #[cfg(target_os = "macos")]
    let status = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let status = -1;
    if status == 0 {
        Ok(())
    } else {
        Err(os_error())
    }
}
fn disk_sample(root: &Path) -> Value {
    let sampled = now_ms();
    match bounded_output(
        Command::new("/bin/df").arg("-kP").arg(root),
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout).into_owned();
            let parsed = raw.lines().skip(1).find_map(|line| {
                let columns: Vec<_> = line.split_whitespace().collect();
                if columns.len() < 6 {
                    return None;
                }
                Some((
                    columns[1].parse::<u64>().ok()?.checked_mul(1024)?,
                    columns[2].parse::<u64>().ok()?.checked_mul(1024)?,
                    columns[3].parse::<u64>().ok()?.checked_mul(1024)?,
                ))
            });
            match parsed {
                Some((total, used, available)) => {
                    json!({"sampledAtMs":sampled,"totalBytes":total,"usedBytes":used,"availableBytes":available,"source":"/bin/df -kP","raw":raw})
                }
                None => json!({"sampledAtMs":sampled,"error":"Cannot parse df output","raw":raw}),
            }
        }
        Ok(output) => {
            json!({"sampledAtMs":sampled,"error":String::from_utf8_lossy(&output.stderr)})
        }
        Err(error) => json!({"sampledAtMs":sampled,"error":error}),
    }
}
fn signed_delta(before: &Value, after: &Value) -> Value {
    match (
        before["availableBytes"].as_u64(),
        after["availableBytes"].as_u64(),
    ) {
        (Some(a), Some(b)) => {
            json!((b as i128 - a as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let base = fs::canonicalize(std::env::temp_dir()).unwrap();
            let root = base.join(format!("msm-agent-cleanup-test-{}", id().unwrap()));
            fs::create_dir(&root).unwrap();
            Self { root }
        }
        fn service(&self, allow: bool) -> AgentCleanup {
            let s = AgentCleanup::new(self.root.join("journal"), allow).unwrap();
            *s.inner.process_guard.lock().unwrap() = Some(Ok(()));
            s
        }
        fn target(&self) -> PathBuf {
            fs::write(
                self.root.join("Cargo.toml"),
                b"[package]\nname='synthetic'\nversion='0.0.0'\n",
            )
            .unwrap();
            let target = self.root.join("target");
            fs::create_dir_all(target.join("debug/deps")).unwrap();
            target
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn wait(s: &AgentCleanup, id: &str) -> Value {
        let started = Instant::now();
        loop {
            let status = s.status(id).unwrap();
            if terminal(&status) {
                return status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "job did not finish: {status}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn preview(s: &AgentCleanup, root: &Path, kind: &str) -> Value {
        let response = s.preview(&json!({"path":root,"kind":kind})).unwrap();
        let ready = wait(s, response["jobId"].as_str().unwrap());
        assert_eq!(ready["state"], "ready", "{ready}");
        ready
    }
    fn execute(s: &AgentCleanup, p: &Value) -> Value {
        let response = s.execute(p["planId"].as_str().unwrap()).unwrap();
        wait(s, response["jobId"].as_str().unwrap())
    }

    #[test]
    fn asynchronous_batch_cleanup_has_real_disk_receipts_and_replay_is_idempotent() {
        let f = Fixture::new();
        let target = f.target();
        for i in 0..600 {
            fs::write(
                target.join(format!("debug/deps/artifact-{i}.rlib")),
                vec![i as u8; 4096],
            )
            .unwrap();
        }
        let s = f.service(true);
        let ready = preview(&s, &target, "rust_target");
        assert_eq!(ready["plannedFiles"], 600);
        let started = Instant::now();
        let response = s.execute(ready["planId"].as_str().unwrap()).unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        let result = wait(&s, response["jobId"].as_str().unwrap());
        assert_eq!(result["state"], "completed", "{result}");
        assert_eq!(result["deletedFiles"], 600);
        assert_eq!(result["deletedLogicalBytes"], 600 * 4096);
        assert_eq!(
            result["protectionEvidence"]["plan"]["git"]["state"],
            "no_marker_in_bounded_ancestry"
        );
        assert_eq!(
            result["protectionEvidence"]["execution"]["git"]["state"],
            "no_marker_in_bounded_ancestry"
        );
        assert_eq!(result["protectionEvidence"]["revalidation"]["attempts"], 3);
        assert_eq!(
            result["protectionEvidence"]["revalidation"]["metadataBindingChecks"],
            0
        );
        assert_eq!(
            result["protectionEvidence"]["revalidation"]["successfulReloads"],
            3
        );
        assert_eq!(fs::read_dir(target.join("debug/deps")).unwrap().count(), 0);
        assert!(result["diskBefore"]["availableBytes"].is_u64(), "{result}");
        assert!(result["diskAfter"]["availableBytes"].is_u64());
        assert!(result["diskFollowup"]["availableBytes"].is_u64());
        assert!(result["availableBytesDelta"].is_i64());
        assert_eq!(result["deletedPaths"], json!([target]));
        let replay = s.execute(ready["planId"].as_str().unwrap()).unwrap();
        assert_eq!(replay["jobId"], result["jobId"]);
        assert!(Path::new(result["journalPath"].as_str().unwrap()).exists());
        let restored = AgentCleanup::new(f.root.join("journal"), true).unwrap();
        assert_eq!(
            restored.status(result["jobId"].as_str().unwrap()).unwrap()["deletedFiles"],
            600
        );
        assert_eq!(
            restored.execute(ready["planId"].as_str().unwrap()).unwrap()["jobId"],
            result["jobId"]
        );
    }
    #[test]
    fn read_only_is_the_default_authority_boundary() {
        let f = Fixture::new();
        let target = f.target();
        let artifact = target.join("debug/file");
        fs::write(&artifact, b"cache").unwrap();
        let s = f.service(false);
        let plan = preview(&s, &target, "rust_target");
        assert!(s
            .execute(plan["planId"].as_str().unwrap())
            .unwrap_err()
            .contains("--allow-cleanup"));
        assert!(artifact.exists());
    }
    #[test]
    fn replacements_new_arrivals_symlinks_and_source_boundaries_survive() {
        let f = Fixture::new();
        let target = f.target();
        let planned = target.join("debug/deps/replaced");
        fs::write(&planned, b"old").unwrap();
        fs::create_dir_all(target.join("debug/source")).unwrap();
        fs::write(target.join("debug/source/Cargo.toml"), b"source").unwrap();
        fs::write(target.join("debug/source/main.rs"), b"precious").unwrap();
        fs::create_dir_all(target.join("application-data")).unwrap();
        fs::write(target.join("application-data/db"), b"runtime").unwrap();
        fs::write(f.root.join("source.rs"), b"preserve").unwrap();
        symlink(f.root.join("source.rs"), target.join("debug/link")).unwrap();
        let s = f.service(true);
        let plan = preview(&s, &target, "rust_target");
        assert_eq!(plan["plannedFiles"], 1);
        fs::remove_file(&planned).unwrap();
        fs::write(&planned, b"new replacement").unwrap();
        let arrival = target.join("debug/new-arrival");
        fs::write(&arrival, b"arrived").unwrap();
        let result = execute(&s, &plan);
        assert_eq!(result["deletedFiles"], 0, "{result}");
        assert_eq!(fs::read(&planned).unwrap(), b"new replacement");
        assert!(arrival.exists());
        assert!(target
            .join("debug/link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.join("debug/source/main.rs").exists());
        assert!(target.join("application-data/db").exists());
    }
    #[test]
    fn actual_missing_and_permission_denied_staging_are_typed_retention_events() {
        struct RestoreDirectoryMode(PathBuf);
        impl Drop for RestoreDirectoryMode {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let f = Fixture::new();
        let target = f.target();
        let parent = target.join("debug/deps");
        let missing = parent.join("missing.o");
        let staged = parent.join("stage-denied.o");
        fs::write(&missing, b"owned missing after preview").unwrap();
        fs::write(&staged, b"owned staging control").unwrap();
        let service = f.service(true);
        let plan = preview(&service, &target, "rust_target");
        fs::remove_file(&missing).unwrap();
        let restore = RestoreDirectoryMode(parent.clone());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let result = execute(&service, &plan);
        drop(restore);
        // Privileged test users can rename in a read-only directory. They still
        // exercise the real missing-entry branch, without pretending EACCES.
        let expected = if unsafe { libc::geteuid() } == 0 {
            1
        } else {
            2
        };
        assert_eq!(
            result["protectionEvidence"]["execution"]["retained"]["otherRetentions"], expected,
            "{result}"
        );
        assert_eq!(result["retainedEntries"], expected);
        if expected == 2 {
            assert_eq!(result["deletedFiles"], 0);
            assert_eq!(fs::read(&staged).unwrap(), b"owned staging control");
            assert!(result["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["reason"]
                    .as_str()
                    .unwrap()
                    .starts_with("Cannot stage file:")));
        }
    }
    #[test]
    fn ancestor_symlink_swap_cannot_redirect_deletion() {
        let f = Fixture::new();
        let target = f.target();
        fs::write(target.join("debug/deps/keep"), b"old").unwrap();
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"source").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &target, "rust_target");
        fs::rename(target.join("debug/deps"), target.join("debug/original")).unwrap();
        symlink(&outside, target.join("debug/deps")).unwrap();
        let result = execute(&s, &plan);
        assert_eq!(result["deletedFiles"], 0);
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"source");
        assert!(target.join("debug/original/keep").exists());
    }
    #[test]
    fn process_inspection_failure_retains_all_files() {
        let f = Fixture::new();
        let target = f.target();
        let file = target.join("debug/cache");
        fs::write(&file, b"important").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &target, "rust_target");
        *s.inner.process_guard.lock().unwrap() = Some(Err("lsof timed out; retained".into()));
        let result = execute(&s, &plan);
        assert_eq!(result["state"], "failed");
        assert_eq!(result["deletedFiles"], 0);
        assert!(result["error"].as_str().unwrap().contains("lsof"));
        assert!(file.exists());
    }
    #[test]
    fn go_cache_layout_retains_unknown_and_fuzz_data() {
        let f = Fixture::new();
        fs::write(f.root.join("go.mod"), b"module synthetic\ngo 1.20\n").unwrap();
        let root = f.root.join("go-build");
        fs::create_dir_all(root.join("ab")).unwrap();
        fs::create_dir_all(root.join("fuzz")).unwrap();
        let artifact = root.join("ab").join(format!("ab{}-d", "1".repeat(62)));
        fs::write(&artifact, b"artifact").unwrap();
        fs::write(root.join("ab/source.go"), b"source").unwrap();
        fs::write(root.join("fuzz/corpus"), b"inputs").unwrap();
        fs::write(root.join("README"), b"cache metadata").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &root, "go_build_cache");
        assert_eq!(plan["plannedFiles"], 1);
        let result = execute(&s, &plan);
        assert_eq!(result["deletedFiles"], 1);
        assert!(!artifact.exists());
        assert!(root.join("ab/source.go").exists());
        assert!(root.join("fuzz/corpus").exists());
        assert!(root.join("README").exists());
    }
    #[test]
    fn incoming_reference_created_after_preview_retains_the_referent() {
        let f = Fixture::new();
        let target = f.target();
        let artifact = target.join("debug/deps/input.o");
        fs::write(&artifact, b"owned input").unwrap();
        let service = f.service(true);
        let plan = preview(&service, &target, "rust_target");
        assert_eq!(plan["plannedFiles"], 1);
        symlink(&artifact, f.root.join("incoming")).unwrap();
        let receipt = execute(&service, &plan);
        assert_eq!(receipt["deletedFiles"], 0, "{receipt}");
        assert_eq!(fs::read(&artifact).unwrap(), b"owned input");
        assert_eq!(receipt["incomingReferenceCoverage"]["complete"], true);
        assert_eq!(
            receipt["protectionEvidence"]["execution"]["retained"]["symlinkReferents"],
            1
        );
    }
    #[test]
    fn unknown_incoming_closure_fails_closed_before_artifact_mutation() {
        let f = Fixture::new();
        let target = f.target();
        let artifact = target.join("debug/deps/input.o");
        fs::write(&artifact, b"owned input").unwrap();
        let service = f.service(true);
        let plan = preview(&service, &target, "rust_target");
        symlink("missing-reference", f.root.join("unresolved")).unwrap();
        let receipt = execute(&service, &plan);
        assert_eq!(receipt["deletedFiles"], 0, "{receipt}");
        assert_eq!(receipt["failureDisposition"], "inspection_unavailable");
        assert_eq!(receipt["incomingReferenceCoverage"]["complete"], false);
        assert_eq!(fs::read(&artifact).unwrap(), b"owned input");
    }
    #[test]
    fn new_or_retargeted_links_and_hardlinks_are_rechecked_between_batches() {
        for mode in ["new", "retarget", "hardlink"] {
            let f = Fixture::new();
            let target = f.target();
            for index in 0..BATCH + 2 {
                fs::write(target.join(format!("debug/deps/{index}.o")), b"owned cache").unwrap();
            }
            let source = f.root.join("source.txt");
            fs::write(&source, b"source input").unwrap();
            let link = f.root.join("reference");
            if mode == "retarget" {
                symlink(&source, &link).unwrap();
            }
            let service = f.service(true);
            let plan = preview(&service, &target, "rust_target");
            let relative = service.inner.registry.lock().unwrap().plans
                [plan["planId"].as_str().unwrap()]
            .entries[BATCH]
                .relative
                .clone();
            let protected = target.join(relative);
            let selected = protected.clone();
            *service.inner.batch_hook.lock().unwrap() = Some(Arc::new(move |index| {
                if index == BATCH {
                    if mode == "retarget" {
                        fs::remove_file(&link).map_err(|e| e.to_string())?;
                    }
                    if mode == "hardlink" {
                        fs::hard_link(&selected, &link).map_err(|e| e.to_string())?;
                    } else {
                        symlink(&selected, &link).map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            }));
            let receipt = execute(&service, &plan);
            assert_eq!(receipt["state"], "partial", "{mode}: {receipt}");
            assert_eq!(receipt["deletedFiles"], BATCH + 1, "{mode}: {receipt}");
            assert_eq!(fs::read(&protected).unwrap(), b"owned cache");
            assert_eq!(fs::read(&source).unwrap(), b"source input");
            assert!(receipt["successfulReferenceChecks"].as_u64().unwrap() >= 2);
        }
    }
    #[test]
    fn cancellation_at_batch_reference_acquisition_prevents_the_next_mutation() {
        let f = Fixture::new();
        let target = f.target();
        for index in 0..BATCH + 2 {
            fs::write(target.join(format!("debug/deps/{index}.o")), b"owned cache").unwrap();
        }
        let service = f.service(true);
        let previewed = preview(&service, &target, "rust_target");
        let plan = service.inner.registry.lock().unwrap().plans
            [previewed["planId"].as_str().unwrap()]
        .clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_hook = cancel.clone();
        *service.inner.batch_hook.lock().unwrap() = Some(Arc::new(move |index| {
            if index == BATCH {
                cancel_for_hook.store(true, Ordering::SeqCst);
            }
            Ok(())
        }));
        let job = id().unwrap();
        service
            .insert(
                &job,
                json!({"jobId":job,"state":"executing"}),
                cancel.clone(),
            )
            .unwrap();
        service.run_execution(&job, &plan, &cancel, || Ok(()));
        let receipt = wait(&service, &job);
        assert_eq!(receipt["state"], "cancelled", "{receipt}");
        assert_eq!(receipt["deletedFiles"], BATCH);
        assert!(target.join(&plan.entries[BATCH].relative).exists());
        assert!(target.join(&plan.entries[BATCH + 1].relative).exists());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn unopened_relative_consumer_arriving_between_batches_stops_remaining_mutation() {
        use std::io::BufRead;
        use std::process::Stdio;
        struct ConsumerGuard(Arc<Mutex<Option<std::process::Child>>>);
        impl Drop for ConsumerGuard {
            fn drop(&mut self) {
                if let Some(mut child) = self
                    .0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let f = Fixture::new();
        let target = f.target();
        for index in 0..BATCH + 2 {
            fs::write(target.join(format!("debug/deps/{index}.o")), b"owned cache").unwrap();
        }
        let service = f.service(true);
        let previewed = preview(&service, &target, "rust_target");
        let plan = service.inner.registry.lock().unwrap().plans
            [previewed["planId"].as_str().unwrap()]
        .clone();
        let operand = Path::new("target").join(&plan.entries[BATCH].relative);
        let consumer = Arc::new(Mutex::new(None));
        let _guard = ConsumerGuard(consumer.clone());
        let consumer_for_hook = consumer.clone();
        let weak = Arc::downgrade(&service.inner);
        let project = f.root.clone();
        *service.inner.batch_hook.lock().unwrap() = Some(Arc::new(move |index| {
            if index == BATCH {
                let mut child = Command::new("/usr/bin/python3")
                    .args([
                        "-I",
                        "-S",
                        "-c",
                        "import time;print('READY',flush=True);time.sleep(30)",
                    ])
                    .arg(&operand)
                    .current_dir(&project)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                let stdout = child.stdout.take().ok_or("Consumer stdout unavailable")?;
                *consumer_for_hook
                    .lock()
                    .map_err(|_| "Consumer ownership unavailable")? = Some(child);
                let mut ready = String::new();
                std::io::BufReader::new(stdout)
                    .read_line(&mut ready)
                    .map_err(|e| e.to_string())?;
                if ready.trim() != "READY" {
                    return Err("Owned consumer did not become ready".into());
                }
                let inner = weak.upgrade().ok_or("Service unavailable")?;
                *inner
                    .process_guard
                    .lock()
                    .map_err(|_| "Process override unavailable")? = None;
            }
            Ok(())
        }));
        let receipt = execute(&service, &previewed);
        assert_eq!(receipt["state"], "partial", "{receipt}");
        assert_eq!(
            receipt["failureDisposition"], "observed_active",
            "{receipt}"
        );
        assert_eq!(receipt["deletedFiles"], BATCH);
        assert!(target.join(&plan.entries[BATCH].relative).exists());
        assert!(target.join(&plan.entries[BATCH + 1].relative).exists());
        assert!(consumer
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none());
    }
    #[test]
    fn preview_requires_real_manifest_and_rejects_alias_or_cloud_paths() {
        let f = Fixture::new();
        let s = f.service(true);
        let missing = f.root.join("target");
        fs::create_dir(&missing).unwrap();
        let result = s
            .preview(&json!({"path":missing,"kind":"rust_target"}))
            .unwrap();
        assert_eq!(
            wait(&s, result["jobId"].as_str().unwrap())["state"],
            "failed"
        );
        let actual = f.target();
        let alias = f.root.join("alias");
        symlink(&actual, &alias).unwrap();
        assert!(safe_root(&alias).is_err());
        assert!(safe_root(Path::new("/x/Library/CloudStorage/target")).is_err());
    }
    #[test]
    fn cancellation_and_deadlines_do_not_create_executable_partial_plans() {
        let f = Fixture::new();
        let target = f.target();
        let s = f.service(true);
        let cancel = AtomicBool::new(true);
        assert_eq!(
            s.build_plan("synthetic", "rust_target", &target, &cancel)
                .err()
                .unwrap(),
            "cancelled"
        );
        assert!(check_budget(
            &AtomicBool::new(false),
            Instant::now() - Duration::from_secs(31),
            30
        )
        .is_err());
        let plan = preview(&s, &target, "rust_target");
        let registry = s.inner.registry.lock().unwrap();
        assert!(registry
            .plans
            .contains_key(plan["planId"].as_str().unwrap()));
    }
    #[test]
    fn restart_marks_interrupted_operation_without_replay() {
        let f = Fixture::new();
        let s = f.service(true);
        let job = id().unwrap();
        s.save_json(&format!("{job}.json"),&json!({"jobId":job,"planId":"11111111111111111111111111111111","operation":"execute","state":"running","deletedFiles":2,"recoveryDirectory":f.root.join("quarantine")})).unwrap();
        drop(s);
        let resumed = AgentCleanup::new(f.root.join("journal"), true).unwrap();
        let status = resumed.status(&job).unwrap();
        assert_eq!(status["state"], "interrupted");
        assert_eq!(status["deletedFiles"], 2);
        assert_eq!(
            resumed.execute("11111111111111111111111111111111").unwrap()["jobId"],
            job
        );
    }
    #[test]
    fn recovery_never_overwrites_a_fresh_arrival() {
        let f = Fixture::new();
        fs::create_dir(f.root.join("q")).unwrap();
        fs::write(f.root.join("q/staged"), b"preserved").unwrap();
        fs::write(f.root.join("new"), b"arrival").unwrap();
        let (_, root) = safe_root(&f.root).unwrap();
        let q = open_child(&root, OsStr::new("q")).unwrap();
        assert!(restore_exclusive(&q, OsStr::new("staged"), &root, OsStr::new("new")).is_err());
        assert_eq!(fs::read(f.root.join("new")).unwrap(), b"arrival");
        assert!(f.root.join("q/staged").exists());
    }
    #[test]
    fn signed_disk_delta_is_not_clamped_to_claim_savings() {
        assert_eq!(
            signed_delta(
                &json!({"availableBytes":100}),
                &json!({"availableBytes":60})
            ),
            json!(-40)
        );
        assert_eq!(
            signed_delta(&Value::Null, &json!({"availableBytes":60})),
            Value::Null
        );
        assert!(!eligible(
            "go_build_cache",
            Path::new(&format!("ab/{}-d", "é".repeat(32))),
            false
        ));
    }
    #[test]
    fn inspection_failure_text_cannot_impersonate_an_observed_active_guard() {
        let f = Fixture::new();
        let target = f.target();
        let artifact = target.join("debug/deps/owned.o");
        fs::write(&artifact, b"owned artifact").unwrap();
        let service = f.service(true);
        *service.inner.process_guard.lock().unwrap() = Some(Err(
            "Live process lookalike text from an unavailable inspection".into(),
        ));
        let plan = preview(&service, &target, "rust_target");
        let started = service.execute(plan["planId"].as_str().unwrap()).unwrap();
        let receipt = wait(&service, started["jobId"].as_str().unwrap());
        assert_eq!(receipt["failureDisposition"], "inspection_unavailable");
        assert_eq!(receipt["deletedFiles"], 0);
        assert!(artifact.exists());
        assert!(!target.join("debug/.cargo-lock").exists());
    }

    #[test]
    fn native_process_guard_checks_real_open_paths_on_synthetic_cache() {
        let f = Fixture::new();
        let target = f.target();
        let file = target.join("debug/open-file");
        fs::write(&file, b"owned fixture").unwrap();
        let s = f.service(true);
        *s.inner.process_guard.lock().unwrap() = None;
        s.process_check(&target).unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("5")
            .stdin(File::open(&file).unwrap())
            .spawn()
            .unwrap();
        let outcome = s.process_check_detail(&target);
        let _ = child.kill();
        let _ = child.wait();
        let failure = outcome.unwrap_err();
        assert_eq!(failure.disposition, GuardDisposition::ObservedActive);
        assert!(failure.message.contains("open path"));
    }
    #[test]
    fn coordination_runs_off_request_thread_and_cancel_precedes_mutation() {
        let f = Fixture::new();
        let target = f.target();
        let file = target.join("debug/cache");
        fs::write(&file, b"cache").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &target, "rust_target");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let response = s
            .execute_with_before(plan["planId"].as_str().unwrap(), move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        s.cancel(response["jobId"].as_str().unwrap()).unwrap();
        release_tx.send(()).unwrap();
        let outcome = wait(&s, response["jobId"].as_str().unwrap());
        assert_eq!(outcome["state"], "cancelled");
        assert_eq!(outcome["deletedFiles"], 0);
        assert!(file.exists());
        assert!(outcome["diskBefore"].is_null());
    }
    #[test]
    fn coordination_failure_has_durable_receipt_without_mutation() {
        let f = Fixture::new();
        let target = f.target();
        let file = target.join("debug/cache");
        fs::write(&file, b"cache").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &target, "rust_target");
        let response = s
            .execute_with_before(plan["planId"].as_str().unwrap(), || {
                Err::<(), _>("scan lease unavailable".to_string())
            })
            .unwrap();
        let result = wait(&s, response["jobId"].as_str().unwrap());
        assert_eq!(result["state"], "failed");
        assert_eq!(result["error"], "scan lease unavailable");
        assert!(file.exists());
        let restarted = AgentCleanup::new(f.root.join("journal"), true).unwrap();
        assert_eq!(
            restarted.status(result["jobId"].as_str().unwrap()).unwrap()["state"],
            "failed"
        );
    }
    #[test]
    fn recognized_subtrees_offer_bounded_scopes_without_broadening_authority() {
        let f = Fixture::new();
        let target = f.target();
        let deps = target.join("debug/deps");
        fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        fs::write(deps.join("artifact"), b"cache").unwrap();
        fs::write(target.join("debug/keep"), b"another scope").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &deps, "rust_target");
        assert_eq!(plan["plannedFiles"], 1);
        assert_eq!(execute(&s, &plan)["deletedFiles"], 1);
        assert!(target.join("debug/keep").exists());
        let unknown = target.join("application-data/deep");
        fs::create_dir_all(&unknown).unwrap();
        let response = s
            .preview(&json!({"path":unknown,"kind":"rust_target"}))
            .unwrap();
        assert_eq!(
            wait(&s, response["jobId"].as_str().unwrap())["state"],
            "failed"
        );
        let go = f.root.join("go-build/ab");
        fs::create_dir_all(&go).unwrap();
        fs::write(go.join(format!("ab{}-a", "0".repeat(62))), b"go cache").unwrap();
        assert_eq!(preview(&s, &go, "go_build_cache")["plannedFiles"], 1);
    }
    #[test]
    fn other_live_session_is_never_misclassified_or_cancelled() {
        let f = Fixture::new();
        let owner = f.service(true);
        let job = id().unwrap();
        let plan = id().unwrap();
        owner.save_json(&format!("{job}.json"),&json!({"jobId":job,"planId":plan,"operation":"execute","ownerSession":owner.inner.session,"state":"running","deletedFiles":0})).unwrap();
        let observer = f.service(true);
        assert_eq!(observer.status(&job).unwrap()["state"], "running");
        assert!(observer.cancel(&job).is_err());
        owner.save_json(&format!("{job}.json"),&json!({"jobId":job,"planId":plan,"operation":"execute","ownerSession":owner.inner.session,"state":"completed","deletedFiles":7})).unwrap();
        assert_eq!(observer.status(&job).unwrap()["state"], "completed");
        assert_eq!(observer.status(&job).unwrap()["deletedFiles"], 7);
    }
    #[test]
    fn capacity_pruning_preserves_recovery_and_foreign_live_maps() {
        let f = Fixture::new();
        let owner = f.service(true);
        let recovery = id().unwrap();
        let live = id().unwrap();
        let completed = id().unwrap();
        for (job, state, recovery_files) in [
            (&recovery, "partial", 1),
            (&live, "running", 0),
            (&completed, "completed", 0),
        ] {
            owner.save_json(&format!("{job}.json"),&json!({"jobId":job,"ownerSession":owner.inner.session,"state":state,"recoveryFiles":recovery_files})).unwrap();
            owner
                .save_json(
                    &format!("{job}.plan.json"),
                    &json!({"mapping":"synthetic recovery map"}),
                )
                .unwrap();
        }
        let observer = f.service(true);
        let lock = observer.journal_lock().unwrap();
        assert!(observer
            .ensure_capacity(64 * 1024 * 1024, "new.json")
            .is_err());
        drop(lock);
        for job in [&recovery, &live, &completed] {
            assert!(owner
                .inner
                .journal
                .join(format!("{job}.plan.json"))
                .exists());
        }
        drop(owner);
        let lock = observer.journal_lock().unwrap();
        assert!(observer
            .ensure_capacity(64 * 1024 * 1024, "new.json")
            .is_err());
        drop(lock);
        assert!(observer
            .inner
            .journal
            .join(format!("{recovery}.plan.json"))
            .exists());
        assert!(observer
            .inner
            .journal
            .join(format!("{live}.plan.json"))
            .exists());
        assert!(!observer
            .inner
            .journal
            .join(format!("{completed}.plan.json"))
            .exists());
    }
    #[test]
    fn malformed_journal_identity_cannot_authorize_outside_path_pruning() {
        let f = Fixture::new();
        let s = f.service(true);
        let job = id().unwrap();
        let victim = f.root.join("victim.json");
        fs::write(&victim, b"retain").unwrap();
        s.save_json(
            &format!("{job}.json"),
            &json!({"jobId":"../victim","state":"completed"}),
        )
        .unwrap();
        let restored = f.service(true);
        assert!(restored.status("../victim").is_err());
        let lock = restored.journal_lock().unwrap();
        let _ = restored.ensure_capacity(64 * 1024 * 1024, "new.json");
        drop(lock);
        assert_eq!(fs::read(&victim).unwrap(), b"retain");
    }
    #[test]
    fn execution_lock_serializes_other_sessions_and_is_cancellable() {
        let f = Fixture::new();
        let one = f.service(true);
        let two = f.service(true);
        let lock = one
            .execution_lock(&AtomicBool::new(false), Instant::now())
            .unwrap();
        let started = Instant::now();
        assert!(two.execution_lock(&AtomicBool::new(true), started).is_err());
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(lock);
        assert!(two
            .execution_lock(&AtomicBool::new(false), Instant::now())
            .is_ok());
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_utf8_unknown_filename_is_retained_without_stalling_preview() {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let target = f.target();
        let unknown = target
            .join("debug")
            .join(std::ffi::OsString::from_vec(vec![b'f', 0xff]));
        fs::write(&unknown, b"retain").unwrap();
        fs::write(target.join("debug/normal"), b"cache").unwrap();
        let s = f.service(true);
        let plan = preview(&s, &target, "rust_target");
        assert_eq!(plan["plannedFiles"], 1);
        assert_eq!(execute(&s, &plan)["deletedFiles"], 1);
        assert_eq!(fs::read(unknown).unwrap(), b"retain");
    }
    #[test]
    fn non_utf8_boundary_issue_is_serializable_without_panicking() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'f', 0xff]));
        let mut issues = Vec::new();
        issue(&mut issues, &path, "Unknown filename retained");
        assert_eq!(issues.len(), 1);
        assert!(serde_json::to_vec(&issues).is_ok());
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn read_receipt(path: &Path, expected_id: &str) -> Result<Value, String> {
    let metadata = path.symlink_metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > 128 * 1024 {
        return Err("Invalid receipt file".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    let value: Value =
        serde_json::from_reader(file.take(128 * 1024 + 1)).map_err(|e| e.to_string())?;
    if !valid_id(expected_id) || value["jobId"].as_str() != Some(expected_id) {
        return Err("Receipt identifier mismatch".into());
    }
    Ok(value)
}
struct JsonCounter(u64);
impl Write for JsonCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        if self.0 > 32 * 1024 * 1024 {
            return Err(std::io::Error::other("32 MiB per-manifest limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.journal.join(format!(".session-{}.lock", self.session)));
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest<'a> {
    job_id: &'a str,
    plan_id: &'a str,
    root: &'a Path,
    quarantine: &'a str,
    entries: &'a [Entry],
    coordination_scope: &'a Path,
    coordination_locks: &'a [PathBuf],
    policy: &'a str,
}

fn plan_memory(plan: &Plan) -> usize {
    std::mem::size_of::<Plan>()
        + plan.entries.capacity() * std::mem::size_of::<Entry>()
        + plan.directories.capacity() * (std::mem::size_of::<(PathBuf, Identity)>() + 32)
        + plan
            .entries
            .iter()
            .map(|e| e.relative.capacity())
            .sum::<usize>()
        + plan
            .directories
            .keys()
            .map(PathBuf::capacity)
            .sum::<usize>()
}
fn trim_registry(registry: &mut Registry) {
    while registry.order.len() >= MAX_JOBS {
        let Some(index) = registry
            .order
            .iter()
            .position(|id| registry.jobs.get(id).is_none_or(|j| terminal(&j.value)))
        else {
            break;
        };
        let old = registry.order.remove(index);
        registry.jobs.remove(&old);
        registry.plans.remove(&old);
        registry.execution.retain(|_, job| job != &old);
    }
}

#[derive(Default)]
struct ExecutionContext {
    started: Option<Instant>,
    locks_held: bool,
    authorization_scope: Option<PathBuf>,
    parent_goal: Option<String>,
    goal: Option<GoalLimit>,
}
#[derive(Clone)]
struct GoalLimit {
    scope: PathBuf,
    identity: Identity,
    target: u64,
}
impl GoalLimit {
    fn available(&self) -> Result<u64, String> {
        let (_, file) = safe_root(&self.scope)?;
        if !self.identity.same_object(&Identity::metadata(
            &file.metadata().map_err(|e| e.to_string())?,
        )) {
            return Err("Authorized volume/scope changed; remaining files retained".into());
        }
        volume_available(&file).map(|(available, _)| available)
    }
}
fn volume_available(file: &File) -> Result<(u64, u64), String> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize;
    Ok((
        (stat.f_bavail as u64)
            .checked_mul(block)
            .ok_or("Volume capacity overflow")?,
        (stat.f_blocks as u64)
            .checked_mul(block)
            .ok_or("Volume capacity overflow")?,
    ))
}

fn cargo_lock_name(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".cargo-lock" | ".cargo-build-lock" | ".cargo-artifact-lock")
    )
}
fn cargo_coordination_paths(plan: &Plan) -> Result<Vec<PathBuf>, String> {
    if plan.kind != "rust_target" {
        return Ok(Vec::new());
    }
    let target = plan
        .root
        .ancestors()
        .find(|p| p.file_name() == Some(OsStr::new("target")))
        .ok_or("Cargo target provenance unavailable")?;
    let mut profiles = std::collections::BTreeSet::new();
    for entry in &plan.entries {
        let absolute = plan.root.join(&entry.relative);
        let relative = absolute.strip_prefix(target).map_err(|e| e.to_string())?;
        let parts: Vec<_> = relative.components().map(|p| p.as_os_str()).collect();
        if parts.len() < 2 {
            continue;
        }
        let count = if matches!(parts[0].to_str(), Some("debug" | "release")) {
            1
        } else {
            2
        };
        let mut profile = target.to_path_buf();
        for part in parts.iter().take(count) {
            profile.push(part);
        }
        profiles.insert(profile.join(".cargo-lock"));
    }
    if profiles.is_empty() && !plan.entries.is_empty() {
        return Err(
            "No compatible Cargo profile lock can be established; candidate retained".into(),
        );
    }
    if profiles.len() > 128 {
        return Err("Cargo profile coordination budget exceeded; select a smaller scope".into());
    }
    Ok(profiles.into_iter().collect())
}
struct CargoLocks(Vec<(PathBuf, Identity, File, bool)>);
impl CargoLocks {
    fn acquire(
        paths: &[PathBuf],
        authorized_scope: &Path,
        created_records: &mut Vec<Value>,
    ) -> Result<Self, GuardFailure> {
        let mut held = Vec::new();
        for path in paths {
            let parent = path.parent().ok_or("Cargo lock parent unavailable")?;
            let (_, directory) = safe_root(parent)?;
            #[cfg(target_os = "macos")]
            {
                let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
                if unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                    return Err(os_error().into());
                }
                let stat = unsafe { stat.assume_init() };
                if unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_bytes()
                    == b"nfs"
                {
                    return Err("Cargo lock coordination on NFS is unknown; retained".into());
                }
            }
            let may_create = parent.starts_with(authorized_scope);
            let name = cstr(OsStr::new(".cargo-lock"))?;
            let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
            let mut created = false;
            let fd = if may_create {
                let fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        flags | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )
                };
                if fd >= 0 {
                    created = true;
                    fd
                } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                    unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) }
                } else {
                    fd
                }
            } else {
                unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) }
            };
            if fd < 0 {
                return Err(format!("Cargo coordination lock unavailable (outside scope locks are never created); retained: {}",os_error()).into());
            }
            if created {
                created_records.push(json!({"path":path,"created":true,"identityRecorded":false}));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            if created {
                let row = created_records.last_mut().unwrap();
                row["dev"] = json!(metadata.dev());
                row["ino"] = json!(metadata.ino());
                row["logicalBytes"] = json!(metadata.len());
                row["identityRecorded"] = json!(true);
            }
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err("Cargo coordination lock identity/type is unknown; retained".into());
            }
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let e = std::io::Error::last_os_error();
                return Err(if e.kind() == std::io::ErrorKind::WouldBlock {
                    GuardFailure {
                        disposition: GuardDisposition::ObservedActive,
                        message: "Live process holds Cargo build coordination lock; retained"
                            .into(),
                    }
                } else {
                    format!("Cargo lock inspection failed; retained: {e}").into()
                });
            }
            let expected = Identity::metadata(&metadata);
            let current = stat_child(&directory, OsStr::new(".cargo-lock"))?;
            if !expected.same_object(&Identity::stat(&current)) {
                return Err("Cargo lock was replaced during acquisition; retained".into());
            }
            held.push((path.clone(), expected, file, created));
        }
        Ok(Self(held))
    }
    fn records(&self) -> Vec<Value> {
        self.0.iter().map(|(path,identity,_,created)|json!({"path":path,"dev":identity.dev,"ino":identity.ino,"created":created,"logicalBytes":identity.len})).collect()
    }
    fn revalidate(&self) -> Result<(), String> {
        for (path, identity, _, _) in &self.0 {
            let (_, parent) = safe_root(path.parent().ok_or("Cargo lock parent unavailable")?)?;
            let current = stat_child(&parent, OsStr::new(".cargo-lock"))?;
            if !identity.same_object(&Identity::stat(&current)) {
                return Err("Cargo coordination lock changed; remaining files retained".into());
            }
        }
        Ok(())
    }
}
