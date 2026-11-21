//! Session-owned asynchronous scan jobs for local agents. Cached measurements are
//! historical evidence; a separately identified audit supplies fresh measurements.
//! Request/poll never enter the scanner registry or read its persistent cache.
use crate::macdisk::{self, DiskScan};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_JOBS: usize = 32;
const MAX_PAGE: usize = 200;
const MAX_AUXILIARY: usize = 20;
const AUDIT_DEADLINE: Duration = Duration::from_secs(600);

trait ScanBackend: Send + Sync {
    fn cached(&self, path: Option<String>, protected: bool) -> Result<Option<DiskScan>, String>;
    fn start(
        &self,
        path: Option<String>,
        protected: bool,
        mode: Option<String>,
    ) -> Result<DiskScan, String>;
    fn poll(&self, id: String) -> Result<DiskScan, String>;
    fn cancel(&self, id: String) -> Result<DiskScan, String>;
}
struct NativeBackend;
impl ScanBackend for NativeBackend {
    fn cached(&self, path: Option<String>, protected: bool) -> Result<Option<DiskScan>, String> {
        macdisk::get_cached_disk_scan(path, protected, None)
    }
    fn start(
        &self,
        path: Option<String>,
        protected: bool,
        mode: Option<String>,
    ) -> Result<DiskScan, String> {
        macdisk::start_disk_scan_with_mode(path, true, protected, mode)
    }
    fn poll(&self, id: String) -> Result<DiskScan, String> {
        macdisk::get_disk_scan(id)
    }
    fn cancel(&self, id: String) -> Result<DiskScan, String> {
        macdisk::cancel_disk_scan(id)
    }
}

struct Job {
    id: String,
    sequence: u64,
    path: Option<String>,
    allow_protected: bool,
    mode: Option<String>,
    status: String,
    baseline: Option<Arc<DiskScan>>,
    current: Option<Arc<DiskScan>>,
    cancel_requested: bool,
    invalidated: bool,
    error: Option<String>,
    cache_error: Option<String>,
}
impl Job {
    fn terminal(&self) -> bool {
        matches!(self.status.as_str(), "complete" | "cancelled" | "failed")
    }
}
#[derive(Default)]
struct Registry {
    jobs: HashMap<String, Arc<Mutex<Job>>>,
    active: Option<String>,
}
struct Inner {
    registry: Mutex<Registry>,
    // Also serializes a cleanup verification queued behind a cancelling audit.
    worker: Mutex<()>,
    next: AtomicU64,
    backend: Arc<dyn ScanBackend>,
}

pub struct AgentScanSession {
    inner: Arc<Inner>,
}
impl Default for AgentScanSession {
    fn default() -> Self {
        Self::new()
    }
}
impl AgentScanSession {
    pub(crate) fn has_background_work(&self) -> bool {
        // The audit worker holds Inner through cancellation and final publication.
        Arc::strong_count(&self.inner) > 1
    }

    pub fn new() -> Self {
        Self::with_backend(Arc::new(NativeBackend))
    }
    fn with_backend(backend: Arc<dyn ScanBackend>) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry: Mutex::new(Registry::default()),
                worker: Mutex::new(()),
                next: AtomicU64::new(0),
                backend,
            }),
        }
    }
    pub fn storage_status(&self) -> Result<Value, String> {
        let registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Agent scan registry unavailable")?;
        Ok(
            json!({ "overview": macdisk::disk_overview()?, "cache": macdisk::get_disk_cache_settings()?,
            "activeJobId": registry.active, "retainedJobs": registry.jobs.len(),
            "maxPageSize": MAX_PAGE, "auditDeadlineSeconds": AUDIT_DEADLINE.as_secs(),
            "measurementNote": "Allocated bytes are measured filesystem allocation, not guaranteed reclaimable physical space." }),
        )
    }
    /// Always schedules a fresh audit for a new scope. `refresh=false` also reuses
    /// its completed session job. An active same-scope request always reuses it.
    pub fn scan(
        &self,
        path: Option<String>,
        allow_protected: bool,
        refresh: bool,
        mode: Option<String>,
    ) -> Result<Value, String> {
        let path = normalized_path(path)?;
        if !matches!(mode.as_deref(), None | Some("auto" | "full")) {
            return Err("Disk scan mode must be auto or full".into());
        }
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Agent scan registry unavailable")?;
        if let Some(active) = registry
            .active
            .as_ref()
            .and_then(|id| registry.jobs.get(id))
        {
            let active = active.lock().map_err(|_| "Agent scan job unavailable")?;
            if !active.terminal() {
                if active.path == path && active.allow_protected == allow_protected {
                    return Ok(job_value(&active, 0, MAX_PAGE));
                }
                return Err(format!(
                    "A scan is active for another scope (job {}); poll or cancel it first",
                    active.id
                ));
            }
        }
        if !refresh {
            let previous = registry
                .jobs
                .values()
                .filter_map(|j| j.lock().ok())
                .filter(|j| {
                    j.path == path
                        && j.allow_protected == allow_protected
                        && j.status == "complete"
                        && !j.invalidated
                })
                .max_by_key(|j| j.sequence);
            if let Some(previous) = previous {
                return Ok(job_value(&previous, 0, MAX_PAGE));
            }
        }
        // Native cache lookup can encounter a newer failed/cancelled resident
        // run before its durable last-good cache. Retain the session's last good
        // generation independently; an explicit successful rescan advances it.
        let baseline = registry
            .jobs
            .values()
            .filter_map(|job| job.lock().ok())
            .filter(|job| job.path == path && job.allow_protected == allow_protected)
            .filter_map(|job| {
                let snapshot = if !job.invalidated {
                    job.current
                        .as_ref()
                        .filter(|scan| reusable(scan))
                        .cloned()
                        .or_else(|| job.baseline.clone())
                } else {
                    job.baseline.clone()
                };
                snapshot.map(|snapshot| (job.sequence, snapshot))
            })
            .max_by_key(|(sequence, _)| *sequence)
            .map(|(_, snapshot)| snapshot);
        self.schedule(&mut registry, path, allow_protected, mode, baseline)
    }
    fn schedule(
        &self,
        registry: &mut Registry,
        path: Option<String>,
        allow_protected: bool,
        mode: Option<String>,
        baseline: Option<Arc<DiskScan>>,
    ) -> Result<Value, String> {
        while registry.jobs.len() >= MAX_JOBS {
            let oldest = registry
                .jobs
                .iter()
                .filter_map(|(id, j)| {
                    j.lock()
                        .ok()
                        .filter(|j| j.terminal())
                        .map(|j| (id.clone(), j.sequence))
                })
                .min_by_key(|(_, seq)| *seq);
            if let Some((id, _)) = oldest {
                registry.jobs.remove(&id);
            } else {
                return Err("Agent scan job capacity reached".into());
            }
        }
        let sequence = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let id = format!("agent-scan-{}-{sequence}", std::process::id());
        let job = Arc::new(Mutex::new(Job {
            id: id.clone(),
            sequence,
            path,
            allow_protected,
            mode,
            status: "loadingCache".into(),
            baseline,
            current: None,
            cancel_requested: false,
            invalidated: false,
            error: None,
            cache_error: None,
        }));
        registry.jobs.insert(id.clone(), job.clone());
        registry.active = Some(id.clone());
        let inner = self.inner.clone();
        let worker_job = job.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("agent-storage-audit".into())
            .spawn(move || run_job(inner, worker_job))
        {
            registry.jobs.remove(&id);
            registry.active = None;
            return Err(format!("Cannot start background audit: {error}"));
        }
        let result = job.lock().map_err(|_| "Agent scan job unavailable")?;
        Ok(job_value(&result, 0, MAX_PAGE))
    }
    fn job(&self, id: &str) -> Result<Arc<Mutex<Job>>, String> {
        self.inner
            .registry
            .lock()
            .map_err(|_| "Agent scan registry unavailable")?
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| "Unknown agent scan job ID".into())
    }
    pub fn poll(&self, job_id: &str, offset: usize, limit: usize) -> Result<Value, String> {
        let job = self.job(job_id)?;
        let job = job.lock().map_err(|_| "Agent scan job unavailable")?;
        Ok(job_value(&job, offset, limit))
    }
    pub fn cancel(&self, job_id: &str) -> Result<Value, String> {
        let job = self.job(job_id)?;
        let mut job = job.lock().map_err(|_| "Agent scan job unavailable")?;
        if !job.terminal() {
            job.cancel_requested = true;
            job.status = "cancelling".into();
        }
        Ok(job_value(&job, 0, MAX_PAGE))
    }
    fn source(&self, job_id: &str, source: Option<String>) -> Result<String, String> {
        let job = self.job(job_id)?;
        let job = job.lock().map_err(|_| "Agent scan job unavailable")?;
        let scan = match source.as_deref() {
            Some("baseline") => job.baseline.as_ref(),
            Some("current") => job.current.as_ref(),
            None => displayed(&job),
            _ => return Err("Snapshot source must be baseline or current".into()),
        };
        scan.map(|s| s.scan_id.clone())
            .ok_or_else(|| "Snapshot is not available yet; poll the job".into())
    }
    pub fn view(
        &self,
        job_id: &str,
        path: String,
        source: Option<String>,
        offset: usize,
        limit: usize,
    ) -> Result<Value, String> {
        let id = self.source(job_id, source)?;
        let snapshot = match macdisk::get_disk_scan_view(id.clone(), path.clone()) {
            Ok(snapshot) => snapshot,
            Err(error) if error == "Unknown disk scan ID" => {
                let job = self.job(job_id)?;
                let job = job.lock().map_err(|_| "Agent scan job unavailable")?;
                let retained = [job.baseline.as_ref(), job.current.as_ref()]
                    .into_iter()
                    .flatten()
                    .find(|s| s.scan_id == id)
                    .ok_or("Retained snapshot is unavailable")?;
                let path = normalized_path(Some(path))?.unwrap();
                if !retained.roots.contains(&path) {
                    return Err("Historical deep detail was evicted from the native registry; retained root summary remains available via scan_poll. Start a separate current scoped scan if fresh detail is needed".into());
                }
                let mut snapshot = (**retained).clone();
                snapshot
                    .entries
                    .retain(|entry| entry.parent_path == path || entry.path == path);
                snapshot.view_path = Some(path);
                snapshot.view_detail_complete = false;
                snapshot
            }
            Err(error) => return Err(error),
        };
        let job = self.job(job_id)?;
        let job = job.lock().map_err(|_| "Agent scan job unavailable")?;
        let historical = job.invalidated
            || job
                .baseline
                .as_ref()
                .is_some_and(|s| s.scan_id == snapshot.scan_id);
        Ok(snapshot_value(&snapshot, offset, limit, historical))
    }
    pub fn list(
        &self,
        job_id: &str,
        path: String,
        source: Option<String>,
        after_path: Option<String>,
        limit: usize,
    ) -> Result<Value, String> {
        let id = self.source(job_id, source)?;
        let page =
            macdisk::list_disk_directory(id, path, after_path, Some(limit.clamp(1, MAX_PAGE)))?;
        let mut value = serde_json::to_value(page).map_err(|e| e.to_string())?;
        truncate_field(&mut value, "issues", MAX_AUXILIARY);
        value["measurementNote"] = json!("This lists current filesystem names; snapshot measurements can precede a cleanup or active audit.");
        Ok(value)
    }
    pub fn growth(
        &self,
        job_id: &str,
        path: Option<String>,
        baseline_id: Option<String>,
        offset: usize,
        limit: usize,
    ) -> Result<Value, String> {
        let id = self
            .source(job_id, Some("current".into()))
            .or_else(|_| self.source(job_id, Some("baseline".into())))?;
        let comparison = macdisk::get_disk_scan_comparison_with_baseline(id, path, baseline_id)?;
        let mut value = serde_json::to_value(comparison).map_err(|e| e.to_string())?;
        paginate_field(&mut value, "entries", offset, limit);
        let job = self.job(job_id)?;
        let job = job.lock().map_err(|_| "Agent scan job unavailable")?;
        value["auditStatus"] = json!(job.status);
        value["invalidated"] = json!(job.invalidated);
        Ok(value)
    }
    pub fn history(&self, job_id: &str, offset: usize, limit: usize) -> Result<Value, String> {
        let id = self.source(job_id, None)?;
        let mut value = serde_json::to_value(macdisk::list_disk_scan_history(id)?)
            .map_err(|e| e.to_string())?;
        paginate_field(&mut value, "snapshots", offset, limit);
        Ok(value)
    }
    /// Establish the stale boundary before cleanup begins. This never starts an
    /// audit over a half-completed mutation and never waits for cancellation I/O.
    pub fn invalidate_for_cleanup(&self, path: &str) -> Result<Value, String> {
        let path = normalized_path(Some(path.into()))?.unwrap();
        let registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Agent scan registry unavailable")?;
        let mut affected = Vec::new();
        for job in registry.jobs.values() {
            let mut job = job.lock().map_err(|_| "Agent scan job unavailable")?;
            if overlaps(job.path.as_deref(), &path) {
                // A completed current snapshot captured BEFORE the mutation is a
                // valid historical baseline. An unfinished audit is not one.
                if !job.invalidated && job.current.as_ref().is_some_and(|s| reusable(s)) {
                    job.baseline = job.current.clone();
                }
                job.invalidated = true;
                if !job.terminal() {
                    job.cancel_requested = true;
                    job.status = "cancelling".into();
                }
                affected.push(job.id.clone());
            }
        }
        Ok(
            json!({"affectedJobs":affected,"invalidated":true,"verificationRequired":true,"path":path}),
        )
    }
    /// Blocking cleanup-worker barrier. Do not call this from the request loop.
    /// A failed barrier authorizes no filesystem mutation. Native terminal polls
    /// join durable publication, so a mixed measurement cannot enter history.
    pub fn settle_before_cleanup(&self, path: &str) -> Result<Value, String> {
        let mut result = self.invalidate_for_cleanup(path)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut requested = HashSet::new();
        loop {
            let jobs: Vec<_> = {
                let registry = self
                    .inner
                    .registry
                    .lock()
                    .map_err(|_| "Agent scan registry unavailable")?;
                registry.jobs.values().cloned().collect()
            };
            let mut pending = false;
            for job in jobs {
                let (native, terminal) = {
                    let job = job.lock().map_err(|_| "Agent scan job unavailable")?;
                    if !overlaps(job.path.as_deref(), path) {
                        continue;
                    }
                    (
                        job.current
                            .as_ref()
                            .filter(|s| s.status == "running")
                            .map(|s| s.scan_id.clone()),
                        job.terminal(),
                    )
                };
                if let Some(id) = native {
                    let snapshot = if requested.insert(id.clone()) {
                        self.inner.backend.cancel(id.clone())?
                    } else {
                        self.inner.backend.poll(id.clone())?
                    };
                    let native_running = snapshot.status == "running";
                    let mut state = job.lock().map_err(|_| "Agent scan job unavailable")?;
                    if state.current.as_ref().is_some_and(|s| s.scan_id == id) {
                        state.current = Some(Arc::new(snapshot));
                    }
                    pending |= native_running;
                }
                // Covers the gap between a worker's cancellation check and start.
                pending |= !terminal;
            }
            if !pending {
                result["settled"] = json!(true);
                result["cancelledScanIds"] = json!(requested.into_iter().collect::<Vec<_>>());
                return Ok(result);
            }
            if Instant::now() >= deadline {
                return Err(
                    "Scan cancellation did not settle within 15 seconds; cleanup must not start"
                        .into(),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Call after a successful filesystem mutation, using its real affected path.
    /// Invalidates overlapping in-flight measurements before returning. A new
    /// audit waits for their cancellation and preserves the pre-mutation baseline.
    pub fn refresh_after_cleanup(&self, path: &str) -> Result<Value, String> {
        let path = normalized_path(Some(path.into()))?.unwrap();
        let mut registry = self
            .inner
            .registry
            .lock()
            .map_err(|_| "Agent scan registry unavailable")?;
        let mut baseline = None;
        let mut sequence = 0;
        let mut selected_path = Some(path.clone());
        let mut protected = false;
        let mut mode = None;
        let mut affected = Vec::new();
        for job in registry.jobs.values() {
            let mut job = job.lock().map_err(|_| "Agent scan job unavailable")?;
            if overlaps(job.path.as_deref(), &path) {
                if !job.invalidated && job.current.as_ref().is_some_and(|s| reusable(s)) {
                    job.baseline = job.current.clone();
                }
                job.invalidated = true;
                if !job.terminal() {
                    job.cancel_requested = true;
                    job.status = "cancelling".into();
                }
                affected.push(job.id.clone());
                if job.sequence >= sequence
                    && job
                        .path
                        .as_deref()
                        .is_none_or(|scope| Path::new(&path).starts_with(scope))
                {
                    sequence = job.sequence;
                    // Keep an enclosing scope/policy so history is comparable.
                    // A cleanup of an ancestor needs its own wider verification.
                    selected_path = job.path.clone();
                    protected = job.allow_protected;
                    mode = job.mode.clone();
                    baseline = job.baseline.clone();
                }
            }
        }
        if let Some(active) = registry
            .active
            .as_ref()
            .and_then(|id| registry.jobs.get(id))
        {
            let active = active.lock().map_err(|_| "Agent scan job unavailable")?;
            if !active.terminal() && !active.cancel_requested {
                return Ok(
                    json!({"affectedJobs": affected, "verificationRequired": true, "verificationQueued": false,
                    "reason": "Another scope is scanning; request a fresh scan after it finishes", "path": path}),
                );
            }
        }
        let mut result = self.schedule(&mut registry, selected_path, protected, mode, baseline)?;
        result["affectedJobs"] = json!(affected);
        result["verificationQueued"] = json!(true);
        result["cleanupPath"] = json!(path);
        Ok(result)
    }
}
impl Drop for AgentScanSession {
    fn drop(&mut self) {
        if let Ok(registry) = self.inner.registry.lock() {
            for job in registry.jobs.values() {
                if let Ok(mut job) = job.lock() {
                    if !job.terminal() {
                        job.cancel_requested = true;
                    }
                }
            }
        }
    }
}

fn run_job(inner: Arc<Inner>, job: Arc<Mutex<Job>>) {
    // Keep cancellation cleanup inside the same serial worker lease.
    let _worker = inner
        .worker
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Catch a backend panic so capacity and retry are not stranded indefinitely.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| audit(&inner, &job)));
    if let Ok(mut state) = job.lock() {
        match result {
            Err(_) => {
                state.status = "failed".into();
                state.error = Some("Background audit panicked".into());
            }
            Ok(Err(error)) => {
                state.status = "failed".into();
                state.error = Some(error);
            }
            Ok(Ok(())) => {}
        }
        // A failure must not abandon an active native worker and wedge retry.
        if state.status == "failed" {
            if let Some(current) = state.current.as_ref().filter(|s| s.status == "running") {
                let id = current.scan_id.clone();
                drop(state);
                if let Ok(mut snapshot) = inner.backend.cancel(id.clone()) {
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while snapshot.status == "running" && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                        match inner.backend.poll(id.clone()) {
                            Ok(next) => snapshot = next,
                            Err(_) => break,
                        }
                    }
                    if let Ok(mut state) = job.lock() {
                        state.current = Some(Arc::new(snapshot));
                    }
                }
            }
        }
    }
    let id = job.lock().ok().map(|j| j.id.clone());
    if let Ok(mut registry) = inner.registry.lock() {
        if registry.active == id {
            registry.active = None;
        }
    }
}
fn audit(inner: &Inner, job: &Mutex<Job>) -> Result<(), String> {
    let (path, protected, mode) = {
        let state = job.lock().map_err(|_| "Agent scan job unavailable")?;
        (
            state.path.clone(),
            state.allow_protected,
            state.mode.clone(),
        )
    };
    {
        let mut state = job.lock().map_err(|_| "Agent scan job unavailable")?;
        if state.cancel_requested {
            state.status = "cancelled".into();
            return Ok(());
        }
    }
    match inner.backend.cached(path.clone(), protected) {
        Ok(Some(cached)) if reusable(&cached) => {
            let mut state = job.lock().map_err(|_| "Agent scan job unavailable")?;
            // A post-cleanup job retains the explicitly captured before state.
            if state.baseline.is_none() {
                state.baseline = Some(Arc::new(cached));
            }
        }
        Err(error) => {
            job.lock()
                .map_err(|_| "Agent scan job unavailable")?
                .cache_error = Some(error)
        }
        _ => {}
    }
    {
        let mut state = job.lock().map_err(|_| "Agent scan job unavailable")?;
        if state.cancel_requested {
            state.status = "cancelled".into();
            return Ok(());
        }
        state.status = "starting".into();
    }
    let started = inner.backend.start(path, protected, mode)?;
    let id = started.scan_id.clone();
    let mut snapshot = started;
    let deadline = Instant::now() + AUDIT_DEADLINE;
    let mut sent_cancel = false;
    loop {
        let mut state = job.lock().map_err(|_| "Agent scan job unavailable")?;
        let timed_out = Instant::now() >= deadline;
        if timed_out {
            state.cancel_requested = true;
            state.error = Some("Audit time budget exceeded; cached baseline retained".into());
        }
        let cancel = state.cancel_requested;
        state.current = Some(Arc::new(snapshot));
        let terminal = state.current.as_ref().unwrap().status != "running";
        if terminal {
            let current = state.current.as_ref().unwrap();
            state.status = if cancel || current.status == "cancelled" {
                "cancelled".into()
            } else if current.status == "complete" {
                "complete".into()
            } else {
                "failed".into()
            };
            return Ok(());
        }
        state.status = if cancel { "cancelling" } else { "running" }.into();
        drop(state);
        if cancel && !sent_cancel {
            snapshot = inner.backend.cancel(id.clone())?;
            sent_cancel = true;
        } else {
            std::thread::sleep(Duration::from_millis(100));
            snapshot = inner.backend.poll(id.clone())?;
        }
    }
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn reusable(scan: &DiskScan) -> bool {
    scan.status == "complete" && !matches!(scan.freshness.as_str(), "partial" | "updating")
}
fn displayed(job: &Job) -> Option<&Arc<DiskScan>> {
    if !job.invalidated && job.current.as_ref().is_some_and(|s| reusable(s)) {
        job.current.as_ref()
    } else {
        job.baseline.as_ref().or(job.current.as_ref())
    }
}
fn normalized_path(path: Option<String>) -> Result<Option<String>, String> {
    path.map(|path| {
        let path = Path::new(&path);
        if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
            return Err("Scan path must be absolute and contain no parent traversal".into());
        }
        Ok(path
            .components()
            .collect::<PathBuf>()
            .to_string_lossy()
            .into_owned())
    })
    .transpose()
}
fn overlaps(scope: Option<&str>, path: &str) -> bool {
    scope.is_none_or(|scope| {
        Path::new(scope).starts_with(path) || Path::new(path).starts_with(scope)
    })
}
fn job_value(job: &Job, offset: usize, limit: usize) -> Value {
    json!({"jobId": job.id, "status": job.status, "path": job.path, "allowProtected": job.allow_protected,
        "invalidated": job.invalidated, "cancelRequested": job.cancel_requested, "error": job.error, "cacheError": job.cache_error,
        "cacheState": if job.baseline.is_some() {"available"} else if job.status == "loadingCache" {"loading"} else {"miss"},
        "baseline": job.baseline.as_ref().map(|s| snapshot_value(s, offset, limit, true)),
        "current": job.current.as_ref().map(|s| snapshot_value(s, offset, limit, job.invalidated)),
        "displaySource": if displayed(job).is_some_and(|s| job.baseline.as_ref().is_some_and(|b| Arc::ptr_eq(s,b))) {"baseline"} else if job.current.is_some() {"current"} else {"none"},
        "baselineNote": "Cached baseline is historical and remains available during audit, cancellation or failure."})
}
fn snapshot_value(scan: &DiskScan, offset: usize, limit: usize, historical: bool) -> Value {
    let end = offset
        .saturating_add(limit.clamp(1, MAX_PAGE))
        .min(scan.entries.len());
    let entries = &scan.entries[offset.min(end)..end];
    let applications: Vec<_> = scan.applications.iter().take(MAX_AUXILIARY).map(|app| json!({"path":app.path,"name":app.name,"bundleId":app.bundle_id,
        "bundleBytes":app.bundle_bytes,"supportBytes":app.support_bytes,"cacheBytes":app.cache_bytes,
        "associatedPaths":app.associated_paths.iter().take(MAX_AUXILIARY).collect::<Vec<_>>(), "associatedPathsTruncated":app.associated_paths.len()>MAX_AUXILIARY})).collect();
    json!({"scanId":scan.scan_id,"status":scan.status,"roots":scan.roots.iter().take(MAX_AUXILIARY).collect::<Vec<_>>(),
        "startedAtMs":scan.started_at_ms,"updatedAtMs":scan.updated_at_ms,"elapsedMs":scan.elapsed_ms,
        "scannedFiles":scan.scanned_files,"scannedDirectories":scan.scanned_directories,"allocatedBytes":scan.allocated_bytes,"logicalBytes":scan.logical_bytes,
        "currentPath":scan.current_path,"entries":entries,"totalEntries":scan.entries.len(),"offset":offset,"nextOffset":if end<scan.entries.len(){Some(end)}else{None},
        "applications":applications,"applicationsTruncated":scan.applications.len()>MAX_AUXILIARY || scan.application_copies_truncated,
        "issues":scan.issues.iter().take(MAX_AUXILIARY).collect::<Vec<_>>(),"issueCount":scan.issue_count,
        "cacheHit":scan.cache_hit,"cacheAgeMs":if historical || scan.cache_hit { now_ms().saturating_sub(scan.updated_at_ms) } else { scan.cache_age_ms },"coverageComplete":scan.coverage_complete,
        "viewDetailComplete":scan.view_detail_complete,"viewPath":scan.view_path,"indexPolicyVersion":scan.index_policy_version,
        "scanMethod":scan.scan_method,"freshness":if historical {"historical"} else {&scan.freshness},"sourceFreshness":scan.freshness,"historical":historical,"fallbackReason":scan.fallback_reason,"scopeNote":scan.scope_note})
}
fn truncate_field(value: &mut Value, field: &str, limit: usize) {
    if let Some(rows) = value[field].as_array_mut() {
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        value[format!("{field}Truncated")] = json!(truncated);
    }
}
fn paginate_field(value: &mut Value, field: &str, offset: usize, limit: usize) {
    if let Some(rows) = value[field].as_array_mut() {
        let total = rows.len();
        let end = offset.saturating_add(limit.clamp(1, MAX_PAGE)).min(total);
        let page = rows.drain(offset.min(end)..end).collect::<Vec<_>>();
        *rows = page;
        value["totalEntries"] = json!(total);
        value["offset"] = json!(offset);
        value["nextOffset"] = json!(if end < total { Some(end) } else { None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct Fake {
        loads: AtomicUsize,
        starts: AtomicUsize,
        cancels: AtomicUsize,
        loading: AtomicBool,
        finish: AtomicBool,
        cancelled: AtomicBool,
        fail_start: AtomicBool,
        cache_failed: AtomicBool,
        baseline: bool,
    }
    impl Fake {
        fn new(baseline: bool) -> Arc<Self> {
            Arc::new(Self {
                loads: AtomicUsize::new(0),
                starts: AtomicUsize::new(0),
                cancels: AtomicUsize::new(0),
                loading: AtomicBool::new(false),
                finish: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
                fail_start: AtomicBool::new(false),
                cache_failed: AtomicBool::new(false),
                baseline,
            })
        }
    }
    fn fixture(id: &str, status: &str) -> DiskScan {
        serde_json::from_value(json!({"scanId":id,"status":status,"roots":["/fixture"],"startedAtMs":1,"updatedAtMs":2,"elapsedMs":1,
            "scannedFiles":1,"scannedDirectories":1,"allocatedBytes":4096,"logicalBytes":32,"currentPath":"/fixture",
            "entries":[],"applications":[],"issues":[],"issueCount":0,"cacheHit":false,"cacheAgeMs":900000,
            "coverageComplete":status=="complete","scopeNote":"synthetic fixture","freshness":if status=="complete"{"cached"}else{"updating"}})).unwrap()
    }
    impl ScanBackend for Fake {
        fn cached(&self, _: Option<String>, _: bool) -> Result<Option<DiskScan>, String> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            while self.loading.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(self.baseline.then(|| {
                if self.cache_failed.load(Ordering::SeqCst) {
                    fixture("newer-failed-native", "failed")
                } else {
                    fixture("old-cached", "complete")
                }
            }))
        }
        fn start(&self, _: Option<String>, _: bool, _: Option<String>) -> Result<DiskScan, String> {
            let generation = self.starts.fetch_add(1, Ordering::SeqCst);
            if self.fail_start.swap(false, Ordering::SeqCst) {
                return Err("injected start failure".into());
            }
            self.cancelled.store(false, Ordering::SeqCst);
            Ok(fixture(&format!("fresh-{generation}"), "running"))
        }
        fn poll(&self, id: String) -> Result<DiskScan, String> {
            Ok(fixture(
                &id,
                if self.cancelled.load(Ordering::SeqCst) {
                    "cancelled"
                } else if self.finish.load(Ordering::SeqCst) {
                    "complete"
                } else {
                    "running"
                },
            ))
        }
        fn cancel(&self, id: String) -> Result<DiskScan, String> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(fixture(&id, "cancelled"))
        }
    }
    fn wait(session: &AgentScanSession, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let value = session.poll(id, 0, 10).unwrap();
            if predicate(&value) {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out: {value}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn request(session: &AgentScanSession, refresh: bool) -> String {
        session
            .scan(Some("/fixture".into()), false, refresh, None)
            .unwrap()["jobId"]
            .as_str()
            .unwrap()
            .into()
    }
    #[test]
    fn slow_cache_does_not_block_request_poll_or_cancel_and_never_starts_cancelled_job() {
        let fake = Fake::new(true);
        fake.loading.store(true, Ordering::SeqCst);
        let session = AgentScanSession::with_backend(fake.clone());
        let before = Instant::now();
        let id = request(&session, true);
        assert!(before.elapsed() < Duration::from_millis(100));
        let state = session.poll(&id, 0, 10).unwrap();
        assert_eq!(state["status"], "loadingCache");
        assert!(state["baseline"].is_null());
        session.cancel(&id).unwrap();
        fake.loading.store(false, Ordering::SeqCst);
        let state = wait(&session, &id, |s| s["status"] == "cancelled");
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
        // Cancellation before cache load also retains any completed cache it read.
        if !state["baseline"].is_null() {
            assert_eq!(state["baseline"]["freshness"], "historical");
        }
    }
    #[test]
    fn cached_baseline_survives_dedup_cancellation_and_next_explicit_audit() {
        let fake = Fake::new(true);
        let session = AgentScanSession::with_backend(fake.clone());
        let id = request(&session, true);
        let state = wait(&session, &id, |s| s["status"] == "running");
        assert_eq!(state["baseline"]["scanId"], "old-cached");
        assert_eq!(state["displaySource"], "baseline");
        assert_eq!(request(&session, true), id);
        assert!(session
            .scan(Some("/another".into()), false, true, None)
            .is_err());
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
        session.cancel(&id).unwrap();
        let cancelled = wait(&session, &id, |s| s["status"] == "cancelled");
        assert_eq!(cancelled["baseline"]["scanId"], "old-cached");
        assert_eq!(cancelled["displaySource"], "baseline");
        fake.finish.store(true, Ordering::SeqCst);
        let next = request(&session, true);
        assert_ne!(next, id);
        wait(&session, &next, |s| s["status"] == "complete");
        assert_eq!(request(&session, false), next);
        let third = request(&session, true);
        assert_ne!(next, third);
        wait(&session, &third, |s| s["status"] == "complete");
    }
    #[test]
    fn failure_retains_baseline_and_releases_session_for_retry() {
        let fake = Fake::new(true);
        fake.fail_start.store(true, Ordering::SeqCst);
        let session = AgentScanSession::with_backend(fake.clone());
        let id = request(&session, true);
        let failed = wait(&session, &id, |s| s["status"] == "failed");
        assert_eq!(failed["baseline"]["scanId"], "old-cached");
        assert_eq!(failed["error"], "injected start failure");
        fake.finish.store(true, Ordering::SeqCst);
        let retry = request(&session, true);
        wait(&session, &retry, |s| s["status"] == "complete");
    }
    #[test]
    fn retry_preserves_last_good_when_failed_resident_generation_masks_native_cache() {
        let fake = Fake::new(true);
        fake.fail_start.store(true, Ordering::SeqCst);
        let session = AgentScanSession::with_backend(fake.clone());
        let first = request(&session, true);
        wait(&session, &first, |s| s["status"] == "failed");
        // Reproduce native audit-lease refusal: subsequent cache lookup returns
        // the newer failed generation rather than the saved complete one.
        fake.cache_failed.store(true, Ordering::SeqCst);
        fake.finish.store(true, Ordering::SeqCst);
        let retry = request(&session, true);
        let retried = wait(&session, &retry, |s| s["status"] == "complete");
        assert_eq!(retried["baseline"]["scanId"], "old-cached");
        let before = retried["current"]["scanId"].clone();
        let next = request(&session, true);
        let next = wait(&session, &next, |s| s["status"] == "complete");
        assert_eq!(next["baseline"]["scanId"], before);
        session.invalidate_for_cleanup("/fixture/deleted").unwrap();
        let verification = session.refresh_after_cleanup("/fixture/deleted").unwrap();
        let verified = wait(&session, verification["jobId"].as_str().unwrap(), |s| {
            s["status"] == "complete"
        });
        // Cleanup's explicit before state takes precedence over a failed lookup.
        assert_eq!(verified["baseline"]["scanId"], next["current"]["scanId"]);
    }
    #[test]
    fn cleanup_invalidates_old_audit_and_serializes_comparable_verification() {
        let fake = Fake::new(true);
        let session = AgentScanSession::with_backend(fake.clone());
        let id = request(&session, true);
        wait(&session, &id, |s| s["status"] == "running");
        let verification = session.refresh_after_cleanup("/fixture/deleted").unwrap();
        let next = verification["jobId"].as_str().unwrap();
        assert_ne!(id, next);
        let previous = session.poll(&id, 0, 10).unwrap();
        assert_eq!(previous["invalidated"], true);
        assert_eq!(previous["current"]["freshness"], "historical");
        wait(&session, &id, |s| s["status"] == "cancelled");
        wait(&session, next, |s| s["status"] == "running");
        assert_eq!(fake.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
        fake.finish.store(true, Ordering::SeqCst);
        let result = wait(&session, next, |s| s["status"] == "complete");
        assert_eq!(result["baseline"]["scanId"], "old-cached");
        assert_eq!(result["displaySource"], "current");
    }
    #[test]
    fn cleanup_barrier_waits_until_prior_generation_is_terminal() {
        let fake = Fake::new(true);
        let session = AgentScanSession::with_backend(fake.clone());
        let id = request(&session, true);
        wait(&session, &id, |s| s["status"] == "running");
        let settled = session.settle_before_cleanup("/fixture/deleted").unwrap();
        assert_eq!(settled["settled"], true);
        let old = session.poll(&id, 0, 10).unwrap();
        assert_eq!(old["status"], "cancelled");
        assert_eq!(old["current"]["status"], "cancelled");
        assert_eq!(old["invalidated"], true);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn evicted_baseline_has_explicit_shallow_fallback_without_substitution() {
        let fake = Fake::new(true);
        fake.finish.store(true, Ordering::SeqCst);
        let session = AgentScanSession::with_backend(fake);
        let id = request(&session, true);
        wait(&session, &id, |s| s["status"] == "complete");
        let root = session
            .view(&id, "/fixture".into(), Some("baseline".into()), 0, 20)
            .unwrap();
        assert_eq!(root["scanId"], "old-cached");
        assert_eq!(root["historical"], true);
        assert_eq!(root["viewDetailComplete"], false);
        let error = session
            .view(
                &id,
                "/fixture/deeper".into(),
                Some("baseline".into()),
                0,
                20,
            )
            .unwrap_err();
        assert!(error.contains("Historical deep detail was evicted"));
    }
    #[test]
    fn bounded_pages_never_serialize_unbounded_auxiliary_arrays() {
        let mut scan = fixture("large", "complete");
        let entry = macdisk::DiskEntry {
            path: "/fixture/a".into(),
            name: "a".into(),
            parent_path: "/fixture".into(),
            kind: "directory".into(),
            allocated_bytes: 0,
            logical_bytes: 0,
            file_count: 0,
            directory_count: 0,
            complete: true,
        };
        scan.entries = vec![entry; 501];
        scan.issues = vec![
            macdisk::DiskIssue {
                path: "/fixture".into(),
                kind: "fixture".into(),
                message: "synthetic".into()
            };
            501
        ];
        scan.applications = vec![
            macdisk::DiskApplication {
                path: "/fixture/a.app".into(),
                name: "fixture".into(),
                bundle_id: None,
                bundle_bytes: 0,
                support_bytes: 0,
                cache_bytes: 0,
                associated_paths: vec!["/fixture/data".into(); 501]
            };
            501
        ];
        let value = snapshot_value(&scan, 0, usize::MAX, false);
        assert_eq!(value["entries"].as_array().unwrap().len(), MAX_PAGE);
        assert_eq!(value["nextOffset"], 200);
        assert_eq!(value["issues"].as_array().unwrap().len(), MAX_AUXILIARY);
        assert_eq!(
            value["applications"].as_array().unwrap().len(),
            MAX_AUXILIARY
        );
        assert_eq!(
            value["applications"][0]["associatedPaths"]
                .as_array()
                .unwrap()
                .len(),
            MAX_AUXILIARY
        );
        assert_eq!(value["applicationsTruncated"], true);
        let end = snapshot_value(&scan, usize::MAX, usize::MAX, false);
        assert_eq!(end["entries"].as_array().unwrap().len(), 0);
        assert!(end["nextOffset"].is_null());
        let mut listing = json!({"entries":vec![0;501]});
        paginate_field(&mut listing, "entries", 200, 200);
        assert_eq!(listing["entries"].as_array().unwrap().len(), 200);
        assert_eq!(listing["nextOffset"], 400);
        assert_eq!(
            normalized_path(Some("/fixture/./child".into())).unwrap(),
            Some("/fixture/child".into())
        );
        assert!(normalized_path(Some("/fixture/../other".into())).is_err());
    }
}
