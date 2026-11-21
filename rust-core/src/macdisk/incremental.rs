//! Compact durable inventory: one direct-counter row per directory and a complete
//! ledger for observed multiply-linked objects. Ordinary files are not indexed.
//! Every verified result comes from a complete native metadata walk. Events
//! cannot authorize freshness, and ordinary files are never indexed.
use super::*;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::sync::atomic::AtomicU64;

const STORE_VERSION: u32 = 4;
const MAX_ROWS: usize = 1_000_000;
const MAX_ALIAS_HINTS: usize = 65_536;

#[derive(Clone, Default, Debug)]
pub(super) struct Diagnostics {
    pub metadata_entries: u64,
    pub dirty_directories: u64,
    pub reused_directories: u64,
    pub store_bytes: u64,
}
#[derive(Debug)]
#[allow(dead_code)] // Full materialization is reserved for cached views and tests.
pub(super) struct Prepared {
    pub directories: HashMap<String, DiskEntry>,
    pub large_files: Vec<DiskEntry>,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub scanned_files: u64,
    pub scanned_directories: u64,
    pub diagnostics: Diagnostics,
    pub issues: Vec<DiskIssue>,
    pub issue_count: u64,
    pub coverage_complete: bool,
    pub persistence_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Direct {
    device: u64,
    inode: u64,
    allocated: u64,
    own_allocated: u64,
    logical: u64,
    files: u64,
}
impl Direct {
    fn encode(&self) -> Vec<u8> {
        [
            self.device,
            self.inode,
            self.allocated,
            self.own_allocated,
            self.logical,
            self.files,
        ]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
    }
    fn decode(data: &[u8]) -> Result<Self, String> {
        if data.len() != 48 {
            return Err("Invalid compact directory row".into());
        }
        let mut values = [0u64; 6];
        for (index, chunk) in data.chunks_exact(8).enumerate() {
            values[index] = u64::from_le_bytes(chunk.try_into().map_err(|_| "Invalid counter")?);
        }
        Ok(Self {
            device: values[0],
            inode: values[1],
            allocated: values[2],
            own_allocated: values[3],
            logical: values[4],
            files: values[5],
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct File {
    path: PathBuf,
    device: u64,
    inode: u64,
    allocated: u64,
    logical: u64,
    links: u64,
}
impl File {
    fn new(path: &Path, meta: &InventoryMeta) -> Self {
        Self {
            path: path.to_path_buf(),
            device: meta.device,
            inode: meta.inode,
            allocated: meta.allocated,
            logical: meta.logical,
            links: meta.links,
        }
    }
}
#[derive(Default, Clone, Debug)]
struct Model {
    directories: BTreeMap<PathBuf, Direct>,
    aliases: BTreeMap<PathBuf, File>,
    exclusions: BTreeMap<PathBuf, (String, String)>,
}
impl Model {
    fn validate_scope(&self, roots: &[PathBuf], allow_protected: bool) -> Result<(), String> {
        let valid = |path: &Path| {
            path.is_absolute()
                && !path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                && roots.iter().any(|root| path.starts_with(root))
        };
        for path in self.directories.keys().chain(self.aliases.keys()) {
            if !valid(path) || excluded(path) || (!allow_protected && protected_path(path)) {
                return Err("Stored inventory path violates the scan scope".into());
            }
        }
        for path in self.exclusions.keys() {
            if !valid(path) {
                return Err("Stored exclusion path violates the scan scope".into());
            }
        }
        for path in self.aliases.keys() {
            if !path
                .parent()
                .is_some_and(|parent| self.directories.contains_key(parent))
            {
                return Err("Stored alias has no measured direct parent".into());
            }
        }
        for (path, direct) in &self.directories {
            if direct.own_allocated > direct.allocated {
                return Err("Invalid direct allocation counters".into());
            }
            if !roots.contains(path)
                && !path
                    .parent()
                    .is_some_and(|parent| self.directories.contains_key(parent))
            {
                return Err("Stored directory has no scope ancestor".into());
            }
        }
        if roots
            .iter()
            .any(|root| !self.directories.contains_key(root))
        {
            return Err("Stored inventory is missing a scope root".into());
        }
        Ok(())
    }
    fn cost(&self) -> usize {
        self.directories
            .keys()
            .map(|path| 256 + path.as_os_str().len() * 2)
            .sum::<usize>()
            + self
                .aliases
                .keys()
                .map(|path| 256 + path.as_os_str().len() * 2)
                .sum::<usize>()
            + self
                .exclusions
                .iter()
                .map(|(path, (kind, message))| {
                    128 + path.as_os_str().len() * 2 + kind.len() + message.len()
                })
                .sum::<usize>()
    }
    fn bounded(&self, limit: usize) -> bool {
        self.directories.len() + self.aliases.len() <= MAX_ROWS && self.cost() <= limit
    }
    fn prepare(&self, roots: &[PathBuf], mut diagnostics: Diagnostics) -> Result<Prepared, String> {
        let mut directories: HashMap<String, DiskEntry> = self
            .directories
            .iter()
            .map(|(path, direct)| {
                let mut row = new_entry(path, true);
                row.allocated_bytes = direct.allocated;
                row.logical_bytes = direct.logical;
                row.file_count = direct.files;
                row.directory_count = 1;
                row.complete = true;
                (row.path.clone(), row)
            })
            .collect();
        let mut owners = HashMap::<(u64, u64), &PathBuf>::new();
        let mut sizes = HashMap::new();
        for (path, file) in &self.aliases {
            if sizes
                .insert((file.device, file.inode), (file.allocated, file.logical))
                .is_some_and(|size| size != (file.allocated, file.logical))
            {
                return Err("Linked aliases disagree on inode size".into());
            }
            if owners.insert((file.device, file.inode), path).is_some() {
                let row = directories
                    .get_mut(&string(path.parent().ok_or("Alias has no parent")?))
                    .ok_or("Alias parent is absent")?;
                row.allocated_bytes = row
                    .allocated_bytes
                    .checked_sub(file.allocated)
                    .ok_or("Alias allocation mismatch")?;
                row.logical_bytes = row
                    .logical_bytes
                    .checked_sub(file.logical)
                    .ok_or("Alias logical size mismatch")?;
            }
        }
        let mut paths: Vec<_> = self.directories.keys().collect();
        paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in paths {
            if roots.iter().any(|root| root == path) {
                continue;
            }
            let row = directories
                .get(&string(path))
                .ok_or("Missing summary")?
                .clone();
            let parent = directories
                .get_mut(&string(path.parent().ok_or("Directory has no parent")?))
                .ok_or("Directory parent is absent")?;
            parent.allocated_bytes = parent
                .allocated_bytes
                .checked_add(row.allocated_bytes)
                .ok_or("Counter overflow")?;
            parent.logical_bytes = parent
                .logical_bytes
                .checked_add(row.logical_bytes)
                .ok_or("Counter overflow")?;
            parent.file_count = parent
                .file_count
                .checked_add(row.file_count)
                .ok_or("Counter overflow")?;
            parent.directory_count = parent
                .directory_count
                .checked_add(row.directory_count)
                .ok_or("Counter overflow")?;
        }
        for excluded in self.exclusions.keys() {
            let mut ancestor = excluded.parent();
            while let Some(path) = ancestor {
                if let Some(row) = directories.get_mut(&string(path)) {
                    row.complete = false;
                }
                if roots.iter().any(|root| root == path) {
                    break;
                }
                ancestor = path.parent();
            }
        }
        let mut result = Prepared {
            directories,
            large_files: vec![],
            allocated_bytes: 0,
            logical_bytes: 0,
            scanned_files: 0,
            scanned_directories: 0,
            diagnostics: Diagnostics::default(),
            issues: self
                .exclusions
                .iter()
                .take(MAX_ISSUES)
                .map(|(path, (kind, message))| DiskIssue {
                    path: string(path),
                    kind: kind.clone(),
                    message: message.clone(),
                })
                .collect(),
            issue_count: self.exclusions.len() as u64,
            coverage_complete: self.exclusions.is_empty(),
            persistence_error: None,
        };
        for root in roots {
            let row = result
                .directories
                .get(&string(root))
                .ok_or("Scope root is absent")?;
            result.allocated_bytes = result
                .allocated_bytes
                .checked_add(row.allocated_bytes)
                .ok_or("Counter overflow")?;
            result.logical_bytes = result
                .logical_bytes
                .checked_add(row.logical_bytes)
                .ok_or("Counter overflow")?;
            result.scanned_files = result
                .scanned_files
                .checked_add(row.file_count)
                .ok_or("Counter overflow")?;
            result.scanned_directories = result
                .scanned_directories
                .checked_add(row.directory_count)
                .ok_or("Counter overflow")?;
        }
        diagnostics.reused_directories =
            self.directories
                .len()
                .saturating_sub(diagnostics.dirty_directories as usize) as u64;
        result.diagnostics = diagnostics;
        Ok(result)
    }
}
#[derive(Default, Debug, PartialEq, Eq)]
struct Totals {
    allocated: u64,
    logical: u64,
    files: u64,
    directories: u64,
}
impl Model {
    fn totals(&self) -> Result<Totals, String> {
        let mut totals = Totals {
            directories: self.directories.len() as u64,
            ..Totals::default()
        };
        for direct in self.directories.values() {
            totals.allocated = totals
                .allocated
                .checked_add(direct.allocated)
                .ok_or("Allocation overflow")?;
            totals.logical = totals
                .logical
                .checked_add(direct.logical)
                .ok_or("Logical size overflow")?;
            totals.files = totals
                .files
                .checked_add(direct.files)
                .ok_or("File count overflow")?;
        }
        let mut identities = HashMap::new();
        for file in self.aliases.values() {
            if let Some(previous) =
                identities.insert((file.device, file.inode), (file.allocated, file.logical))
            {
                if previous != (file.allocated, file.logical) {
                    return Err("Linked aliases disagree on inode size".into());
                }
                totals.allocated = totals
                    .allocated
                    .checked_sub(file.allocated)
                    .ok_or("Alias allocation mismatch")?;
                totals.logical = totals
                    .logical
                    .checked_sub(file.logical)
                    .ok_or("Alias logical size mismatch")?;
            }
        }
        Ok(totals)
    }
    fn checksum(&self, scope: &str, generation: &str) -> Result<String, String> {
        // Length-framed stable rows detect accidental counter/header corruption.
        // This is an integrity check, never an authorization boundary.
        let mut hash = ContentChecksum(0xcbf29ce484222325, 0x84222325cbf29ce4);
        hash.field(scope.as_bytes());
        hash.field(generation.as_bytes());
        for (path, row) in &self.directories {
            hash.field(b"directory");
            hash.field(path.to_str().ok_or("Invalid cached path")?.as_bytes());
            hash.field(&row.encode());
        }
        for (path, file) in &self.aliases {
            hash.field(b"alias");
            hash.field(path.to_str().ok_or("Invalid cached path")?.as_bytes());
            hash.field(&serde_json::to_vec(file).map_err(|e| e.to_string())?);
        }
        for (path, (kind, message)) in &self.exclusions {
            hash.field(b"exclusion");
            hash.field(path.to_str().ok_or("Invalid cached path")?.as_bytes());
            hash.field(kind.as_bytes());
            hash.field(message.as_bytes());
        }
        Ok(format!("v1-{:016x}{:016x}", hash.0, hash.1))
    }
}
struct ContentChecksum(u64, u64);
impl ContentChecksum {
    fn field(&mut self, bytes: &[u8]) {
        for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            self.1 = (self.1 ^ u64::from(*byte)).wrapping_mul(0x100000001c9);
        }
    }
}
#[derive(Debug)]
pub(super) struct AuditCommit {
    pub diagnostics: Diagnostics,
    pub persistence_error: Option<String>,
}
#[derive(Debug)]
pub(super) struct CachedLayer {
    pub summary: DiskEntry,
    pub entries: Vec<DiskEntry>,
    pub detail_complete: bool,
    pub treemap_directories: Vec<DiskEntry>,
}

#[allow(clippy::too_many_arguments)] // Explicit cache identity and policy; no live traversal.
pub(super) fn cached_layer(
    path: &Path,
    scope: &str,
    roots: &[PathBuf],
    allow: bool,
    budget: u64,
    scan_id: &str,
    requested: &Path,
) -> Result<Option<CachedLayer>, String> {
    if !roots.iter().any(|root| requested.starts_with(root))
        || requested
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        || excluded(requested)
        || (!allow && protected_path(requested))
    {
        return Ok(None);
    }
    let loaded = match load_generation(path, scope, working_limit(budget), Some(scan_id)) {
        Ok(Some(value)) => value,
        _ => return Ok(None),
    };
    if loaded.validate_scope(roots, allow).is_err() {
        return Ok(None);
    }
    let prepared = match loaded.prepare(roots, Diagnostics::default()) {
        Ok(prepared) => prepared,
        Err(_) => return Ok(None),
    };
    let Some(summary) = prepared.directories.get(&string(requested)).cloned() else {
        return Ok(None);
    };
    let mut direct: Vec<_> = prepared
        .directories
        .values()
        .filter(|row| Path::new(&row.path).parent() == Some(requested))
        .cloned()
        .collect();
    direct.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    let direct_count = direct.len();
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    for row in direct {
        let cost = 192 + row.path.len().saturating_mul(3);
        if entries.len() < RESERVED_DIRECT_DIRECTORIES
            && bytes.saturating_add(cost) <= RESERVED_DIRECT_INDEX_BYTES
        {
            bytes += cost;
            entries.push(row);
        }
    }
    let detail_complete = entries.len() == direct_count;
    let parents: HashSet<_> = entries.iter().map(|row| row.path.as_str()).collect();
    let mut deeper: Vec<_> = prepared
        .directories
        .values()
        .filter(|row| parents.contains(row.parent_path.as_str()))
        .cloned()
        .collect();
    deeper.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut deeper_bytes = 0usize;
    for row in deeper.into_iter().take(MAX_SHALLOW_DIRECTORY_ROWS) {
        let cost = 192 + row.path.len().saturating_mul(3);
        if deeper_bytes.saturating_add(cost) <= MAX_SHALLOW_BYTES {
            deeper_bytes += cost;
            entries.push(row);
        }
    }
    let mut base = entries.clone();
    base.push(summary.clone());
    let treemap_directories =
        treemap_extra_rows(&[string(requested)], &base, prepared.directories.values());
    Ok(Some(CachedLayer {
        summary,
        entries,
        detail_complete,
        treemap_directories,
    }))
}
fn working_limit(budget: u64) -> usize {
    (if budget <= 32 * MIB {
        64 * MIB
    } else if budget <= 128 * MIB {
        128 * MIB
    } else {
        192 * MIB
    }) as usize
}

// One metadata layer is accumulated on the traversal worker. Ordinary files
// require only checked scalar additions: no shared lock and no retained path.
struct Recording {
    model: Mutex<Model>,
    failed: AtomicBool,
    entries: AtomicU64,
    active: AtomicU64,
    estimated: AtomicU64,
    limit: usize,
    known_links: HashSet<(u64, u64)>,
}
#[derive(Clone)]
pub(super) struct Recorder(Arc<Recording>);
pub(super) struct Layer {
    recording: Arc<Recording>,
    path: Option<PathBuf>,
    direct: Direct,
    aliases: Vec<File>,
    entries: u64,
    merged: bool,
}
impl Recording {
    fn reserve(&self, bytes: u64) -> bool {
        if self.failed.load(Ordering::Relaxed) {
            return false;
        }
        if self
            .estimated
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes)
            > self.limit as u64
        {
            self.failed.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }
}
impl Recorder {
    fn new(limit: usize, known_links: HashSet<(u64, u64)>) -> Self {
        Self(Arc::new(Recording {
            model: Mutex::new(Model::default()),
            failed: AtomicBool::new(false),
            entries: AtomicU64::new(0),
            active: AtomicU64::new(0),
            estimated: AtomicU64::new((known_links.len() * 64) as u64),
            limit,
            known_links,
        }))
    }
    pub fn layer(&self, path: &Path, meta: &InventoryMeta) -> Layer {
        self.0.active.fetch_add(1, Ordering::Relaxed);
        if !meta.directory || meta.symlink || path.to_str().is_none() {
            self.mark_incomplete();
        }
        self.0.reserve((256 + path.as_os_str().len() * 2) as u64);
        Layer {
            recording: self.0.clone(),
            path: Some(path.to_path_buf()),
            direct: Direct {
                device: meta.device,
                inode: meta.inode,
                allocated: meta.allocated,
                own_allocated: meta.allocated,
                logical: 0,
                files: 0,
            },
            aliases: vec![],
            entries: 1,
            merged: false,
        }
    }
    pub fn merge(&self, mut layer: Layer) {
        layer.merged = true;
        if !Arc::ptr_eq(&self.0, &layer.recording) || self.0.failed.load(Ordering::Relaxed) {
            self.mark_incomplete();
            return;
        }
        let Ok(mut model) = self.0.model.lock() else {
            self.mark_incomplete();
            return;
        };
        if model
            .directories
            .len()
            .saturating_add(model.aliases.len())
            .saturating_add(layer.aliases.len())
            >= MAX_ROWS
        {
            self.mark_incomplete();
            return;
        }
        let Some(path) = layer.path.take() else {
            self.mark_incomplete();
            return;
        };
        if model
            .directories
            .insert(path, layer.direct.clone())
            .is_some()
        {
            self.mark_incomplete();
            return;
        }
        for file in layer.aliases.drain(..) {
            if model.aliases.insert(file.path.clone(), file).is_some() {
                self.mark_incomplete();
                return;
            }
        }
        self.0.entries.fetch_add(layer.entries, Ordering::Relaxed);
    }
    pub fn exclude(&self, path: &Path, kind: &str, message: &str) {
        if self.0.failed.load(Ordering::Relaxed) {
            return;
        }
        if !matches!(kind, "cloud-excluded" | "permission-excluded") {
            self.mark_incomplete();
            return;
        }
        let cost = 128 + path.as_os_str().len() * 2 + kind.len() + message.len();
        if !self.0.reserve(cost as u64) {
            return;
        }
        let Ok(mut model) = self.0.model.lock() else {
            self.mark_incomplete();
            return;
        };
        model
            .exclusions
            .insert(path.to_path_buf(), (kind.into(), message.into()));
    }
    pub fn mark_incomplete(&self) {
        self.0.failed.store(true, Ordering::Relaxed);
    }
}
impl Layer {
    pub fn observe(&mut self, path: &Path, meta: &InventoryMeta) {
        if self.recording.failed.load(Ordering::Relaxed) {
            return;
        }
        if meta.directory || path.to_str().is_none() || path.parent() != self.path.as_deref() {
            self.recording.failed.store(true, Ordering::Relaxed);
            return;
        }
        let Some(allocated) = self.direct.allocated.checked_add(meta.allocated) else {
            self.recording.failed.store(true, Ordering::Relaxed);
            return;
        };
        let Some(logical) = self.direct.logical.checked_add(meta.logical) else {
            self.recording.failed.store(true, Ordering::Relaxed);
            return;
        };
        let Some(files) = self.direct.files.checked_add(1) else {
            self.recording.failed.store(true, Ordering::Relaxed);
            return;
        };
        self.direct.allocated = allocated;
        self.direct.logical = logical;
        self.direct.files = files;
        self.entries += 1;
        if meta.links > 1
            || self
                .recording
                .known_links
                .contains(&(meta.device, meta.inode))
        {
            if !self
                .recording
                .reserve((256 + path.as_os_str().len() * 2) as u64)
            {
                return;
            }
            self.aliases.push(File::new(path, meta));
        }
    }
}
impl Drop for Layer {
    fn drop(&mut self) {
        self.recording.active.fetch_sub(1, Ordering::Relaxed);
        if !self.merged {
            self.recording.failed.store(true, Ordering::Relaxed);
        }
    }
}

pub(super) struct Session {
    path: PathBuf,
    scope: String,
    roots: Vec<PathBuf>,
    budget: u64,
    recorder: Recorder,
    baseline: bool,
    replace_store: bool,
    allow_protected: bool,
}
impl Session {
    pub fn begin_audit(
        path: &Path,
        scope: &str,
        roots: &[PathBuf],
        allow_protected: bool,
        budget: u64,
        reuse: bool,
    ) -> Result<Self, String> {
        if budget < MIB {
            return Err("Metadata inventory store budget is too small".into());
        }
        let limit = working_limit(budget);
        // A previous payload cannot establish freshness. Read only bounded
        // compatibility headers and optional bounded ID hints; every directory and
        // alias is measured from current native metadata below.
        let header = if reuse {
            probe_header(path, scope, budget)
        } else {
            Ok(None)
        };
        let replace_store = header.is_err()
            || (reuse && header.as_ref().is_ok_and(Option::is_none) && path.exists())
            || fs::symlink_metadata(path)
                .is_ok_and(|meta| meta.len() > budget.saturating_sub(budget / 32 + 64 * 1024) / 2);
        let hints = header.ok().flatten();
        let has_baseline = hints.is_some();
        Ok(Self {
            path: path.to_path_buf(),
            scope: scope.into(),
            roots: roots.to_vec(),
            budget,
            recorder: Recorder::new(limit, hints.unwrap_or_default()),
            baseline: has_baseline,
            replace_store,
            allow_protected,
        })
    }
    pub fn has_baseline(&self) -> bool {
        self.baseline
    }
    pub fn recorder(&self) -> Recorder {
        self.recorder.clone()
    }
    pub fn finish_audit(&mut self, run: &Run) -> Result<Option<AuditCommit>, String> {
        if self.recorder.0.failed.load(Ordering::Relaxed)
            || self.recorder.0.active.load(Ordering::Relaxed) != 0
            || run.cancelled.load(Ordering::Relaxed)
            || run.measurement_gap.load(Ordering::Relaxed)
        {
            return Ok(None);
        }
        let model = std::mem::take(
            &mut *self
                .recorder
                .0
                .model
                .lock()
                .map_err(|_| "Metadata inventory recorder poisoned")?,
        );
        model.validate_scope(&self.roots, self.allow_protected)?;
        let totals = model.totals()?;
        let entries = self.recorder.0.entries.load(Ordering::Relaxed);
        let mut diagnostics = Diagnostics {
            metadata_entries: entries,
            dirty_directories: model.directories.len() as u64,
            reused_directories: 0,
            store_bytes: 0,
        };
        if diagnostics.metadata_entries
            != totals
                .files
                .checked_add(totals.directories)
                .ok_or("Metadata count overflow")?
        {
            return Err("Metadata layers do not cover their measured counters".into());
        }
        let measured = run
            .snapshot
            .lock()
            .map_err(|_| "Native scan counters unavailable")?;
        if totals.files != measured.scanned_files
            || totals.directories != measured.scanned_directories
        {
            return Err(
                "Metadata inventory does not cover every node accepted by the native traversal"
                    .into(),
            );
        }
        if totals.allocated != measured.allocated_bytes || totals.logical != measured.logical_bytes
        {
            return Err("Metadata inventory bytes disagree with the native traversal".into());
        }
        let generation = measured.scan_id.clone();
        drop(measured);
        let persistence_error = match save(
            &self.path,
            &self.scope,
            &model,
            &generation,
            self.budget,
            self.replace_store,
        ) {
            Ok(true) => {
                diagnostics.reused_directories = model.directories.len() as u64;
                diagnostics.dirty_directories = 0;
                None
            }
            Ok(false) => None,
            Err(error) => Some(error),
        };
        diagnostics.store_bytes = allocated_store_bytes(&self.path);
        Ok(Some(AuditCommit {
            diagnostics,
            persistence_error,
        }))
    }
}

fn open(path: &Path, budget: u64) -> Result<Connection, String> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let companion = PathBuf::from(format!("{}{}", path.display(), suffix));
        if let Ok(meta) = fs::symlink_metadata(companion) {
            if !meta.file_type().is_file() || meta.nlink() != 1 {
                return Err("Invalid incremental store companion".into());
            }
        }
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.file_type().is_file() || meta.nlink() != 1 || meta.len() > budget {
            return Err("Invalid incremental inventory store".into());
        }
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|e| e.to_string())?;
    connection
        .busy_timeout(Duration::from_millis(100))
        .map_err(|e| e.to_string())?;
    connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY; PRAGMA trusted_schema=OFF;").map_err(|e| e.to_string())?;
    let page_size: u64 = connection
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err("Invalid SQLite page size".into());
    }
    let pages = budget.saturating_sub(budget / 32 + 64 * 1024) / 2 / page_size;
    let page_count: u64 = connection
        .pragma_query_value(None, "page_count", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if page_count > pages {
        return Err("Existing inventory exceeds the journal-safe cache envelope".into());
    }
    connection
        .pragma_update(None, "max_page_count", pages)
        .map_err(|e| e.to_string())?;
    let maximum: u64 = connection
        .pragma_query_value(None, "max_page_count", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if maximum > pages {
        return Err("SQLite did not accept the bounded store allowance".into());
    }
    Ok(connection)
}
fn probe_header(
    path: &Path,
    scope: &str,
    budget: u64,
) -> Result<Option<HashSet<(u64, u64)>>, String> {
    // Close the reader before releasing physical cache-block accounting.
    let _storage = cache_storage_lock()
        .lock()
        .map_err(|_| "Cache storage coordinator poisoned")?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.file_type().is_file() || metadata.nlink() != 1 || metadata.len() > budget {
        return Err("Invalid compact inventory header path".into());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|e| e.to_string())?;
    connection
        .execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; BEGIN;")
        .map_err(|e| e.to_string())?;
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if version != STORE_VERSION {
        return Ok(None);
    }
    let header = |key: &str, maximum: usize| -> Result<Option<String>, String> {
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key=?1 AND length(CAST(value AS BLOB))<=?2",
                params![key, maximum],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
    };
    if header("scope", 16 * 1024)?.as_deref() != Some(scope) {
        return Ok(None);
    }
    let Some(generation) = header("generation", 512)? else {
        return Ok(None);
    };
    let Some(checksum) = header("checksum", 35)? else {
        return Ok(None);
    };
    if generation.is_empty()
        || checksum.len() != 35
        || !checksum.starts_with("v1-")
        || !checksum[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(None);
    }
    // Optional old IDs only decide which CURRENT metadata records to retain.
    // Never decode old directory counters or trust old alias sizes/paths here.
    #[derive(Deserialize)]
    struct AliasHint {
        device: u64,
        inode: u64,
        links: u64,
    }
    let mut hints = HashSet::new();
    let mut statement = connection
        .prepare("SELECT length(CAST(data AS BLOB)),data FROM aliases LIMIT ?1")
        .map_err(|e| e.to_string())?;
    let mut rows = statement
        .query(params![MAX_ALIAS_HINTS + 1])
        .map_err(|e| e.to_string())?;
    let mut count = 0usize;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        count += 1;
        let length: usize = row.get(0).map_err(|e| e.to_string())?;
        if count > MAX_ALIAS_HINTS || length > 32 * 1024 {
            // The current audit visits every object regardless of hint coverage.
            // A valid large historical ledger must not force database replacement.
            return Ok(Some(hints));
        }
        let data: String = row.get(1).map_err(|e| e.to_string())?;
        let hint: AliasHint = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        if hint.inode == 0 || hint.links == 0 {
            return Err("Invalid optional alias identity hint".into());
        }
        hints.insert((hint.device, hint.inode));
    }
    Ok(Some(hints))
}
#[cfg(test)]
fn load(path: &Path, scope: &str, limit: usize) -> Result<Option<Model>, String> {
    load_generation(path, scope, limit, None)
}
#[cfg(test)]
type ReaderHook = (PathBuf, Box<dyn FnOnce() + Send>);
#[cfg(test)]
static READER_HOOK: OnceLock<Mutex<Option<ReaderHook>>> = OnceLock::new();
fn load_generation(
    path: &Path,
    scope: &str,
    limit: usize,
    generation: Option<&str>,
) -> Result<Option<Model>, String> {
    // Declare before Connection: reverse drop order closes every SQLite handle
    // before eviction may unlink the store and stop accounting its blocks.
    let _storage = cache_storage_lock()
        .lock()
        .map_err(|_| "Cache storage coordinator poisoned")?;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !meta.file_type().is_file()
        || meta.nlink() != 1
        || meta.len() > (limit as u64).saturating_mul(3)
    {
        return Err("Invalid incremental inventory store".into());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|e| e.to_string())?;
    #[cfg(test)]
    {
        let hook = READER_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .take_if(|(expected, _)| expected == path);
        if let Some((_, callback)) = hook {
            callback();
        }
    }
    connection
        .execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; BEGIN;")
        .map_err(|e| e.to_string())?;
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version != STORE_VERSION {
        return Ok(None);
    }
    let health: String = connection
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if health != "ok" {
        return Err("Incremental inventory integrity check failed".into());
    }
    let saved_scope: Option<String> = connection
        .query_row("SELECT value FROM metadata WHERE key='scope'", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    if saved_scope.as_deref() != Some(scope) {
        return Ok(None);
    }
    let saved_generation: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key='generation'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if saved_generation.is_empty()
        || saved_generation.len() > 512
        || generation.is_some_and(|expected| saved_generation != expected)
    {
        return Ok(None);
    }
    let checksum: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key='checksum'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if checksum.len() != 35 {
        return Err("Invalid compact content checksum".into());
    }
    let mut model = Model::default();
    let mut statement = connection
        .prepare("SELECT path,data FROM directories")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    let mut bytes = 0usize;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let path: String = row.get(0).map_err(|e| e.to_string())?;
        let data: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;
        bytes = bytes.saturating_add(path.len() * 2 + 256);
        if bytes > limit || model.directories.len() >= MAX_ROWS {
            return Err("Incremental directory store exceeds its allowance".into());
        }
        let direct = Direct::decode(&data)?;
        if !Path::new(&path).is_absolute() || direct.inode == 0 {
            return Err("Invalid incremental directory identity".into());
        }
        model.directories.insert(PathBuf::from(path), direct);
    }
    let mut statement = connection
        .prepare("SELECT path,data FROM aliases")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let saved_path: String = row.get(0).map_err(|e| e.to_string())?;
        let data: String = row.get(1).map_err(|e| e.to_string())?;
        bytes = bytes.saturating_add(data.len() * 2 + 256);
        if bytes > limit || model.directories.len() + model.aliases.len() >= MAX_ROWS {
            return Err("Incremental alias store exceeds its allowance".into());
        }
        let file: File = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        if !file.path.is_absolute()
            || file.links == 0
            || file.path.to_str() != Some(saved_path.as_str())
        {
            return Err("Invalid incremental alias identity".into());
        }
        model.aliases.insert(file.path.clone(), file);
    }
    let mut statement = connection
        .prepare("SELECT path,kind,message FROM exclusions")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let path: String = row.get(0).map_err(|e| e.to_string())?;
        let kind: String = row.get(1).map_err(|e| e.to_string())?;
        let message: String = row.get(2).map_err(|e| e.to_string())?;
        bytes = bytes.saturating_add(path.len() * 2 + kind.len() + message.len() + 128);
        if bytes > limit || !matches!(kind.as_str(), "cloud-excluded" | "permission-excluded") {
            return Err("Invalid exclusion ledger".into());
        }
        model
            .exclusions
            .insert(PathBuf::from(path), (kind, message));
    }
    if !model.bounded(limit) {
        return Err("Compact inventory exceeded its allowance".into());
    }
    if model.checksum(scope, &saved_generation)? != checksum {
        return Err("Compact inventory content checksum mismatch".into());
    }
    Ok(Some(model))
}
fn compact_rows_equal(connection: &Connection, scope: &str, model: &Model) -> Result<bool, String> {
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let saved_scope: Option<String> = connection
        .query_row("SELECT value FROM metadata WHERE key='scope'", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    if version != STORE_VERSION || saved_scope.as_deref() != Some(scope) {
        return Ok(false);
    }
    let mut statement = connection
        .prepare("SELECT path,data FROM directories ORDER BY path")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    for (path, direct) in &model.directories {
        let Some(row) = rows.next().map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        let saved_path: String = row.get(0).map_err(|e| e.to_string())?;
        let data: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;
        if path.to_str() != Some(saved_path.as_str()) || data != direct.encode() {
            return Ok(false);
        }
    }
    if rows.next().map_err(|e| e.to_string())?.is_some() {
        return Ok(false);
    }
    let mut statement = connection
        .prepare("SELECT path,data FROM aliases ORDER BY path")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    for (path, file) in &model.aliases {
        let Some(row) = rows.next().map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        let saved_path: String = row.get(0).map_err(|e| e.to_string())?;
        let data: String = row.get(1).map_err(|e| e.to_string())?;
        if path.to_str() != Some(saved_path.as_str())
            || data != serde_json::to_string(file).map_err(|e| e.to_string())?
        {
            return Ok(false);
        }
    }
    if rows.next().map_err(|e| e.to_string())?.is_some() {
        return Ok(false);
    }
    let mut statement = connection
        .prepare("SELECT path,kind,message FROM exclusions ORDER BY path")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    for (path, (kind, message)) in &model.exclusions {
        let Some(row) = rows.next().map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        let saved_path: String = row.get(0).map_err(|e| e.to_string())?;
        let saved_kind: String = row.get(1).map_err(|e| e.to_string())?;
        let saved_message: String = row.get(2).map_err(|e| e.to_string())?;
        if path.to_str() != Some(saved_path.as_str())
            || kind != &saved_kind
            || message != &saved_message
        {
            return Ok(false);
        }
    }
    Ok(rows.next().map_err(|e| e.to_string())?.is_none())
}
fn save(
    path: &Path,
    scope: &str,
    model: &Model,
    generation: &str,
    budget: u64,
    replace_store: bool,
) -> Result<bool, String> {
    if generation.is_empty() || generation.len() > 512 {
        return Err("Invalid audited generation identity".into());
    }
    let checksum = model.checksum(scope, generation)?;
    let _storage = cache_storage_lock()
        .lock()
        .map_err(|_| "Cache storage coordinator poisoned")?;
    reserve_incremental_bytes(path, budget)?;
    if replace_store {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if !meta.file_type().is_file() {
                return Err("Invalid compact inventory store path".into());
            }
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        for suffix in ["-journal", "-wal", "-shm"] {
            let companion = PathBuf::from(format!("{}{}", path.display(), suffix));
            if let Ok(meta) = fs::symlink_metadata(&companion) {
                if !meta.file_type().is_file() {
                    return Err("Invalid compact store companion".into());
                }
                fs::remove_file(companion).map_err(|e| e.to_string())?;
            }
        }
    }
    let mut connection = open(path, budget)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    let transaction = connection.transaction().map_err(|e| e.to_string())?;
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS directories(path TEXT PRIMARY KEY,data BLOB NOT NULL) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS aliases(path TEXT PRIMARY KEY,data TEXT NOT NULL) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS exclusions(path TEXT PRIMARY KEY,kind TEXT NOT NULL,message TEXT NOT NULL) WITHOUT ROWID;").map_err(|e| e.to_string())?;
    // A checksum alone never authorizes row reuse. Compare every ordered key and
    // value against this full native traversal before performing a header-only commit.
    let unchanged = compact_rows_equal(&transaction, scope, model)?;
    if !unchanged {
        transaction
            .execute_batch("DELETE FROM directories; DELETE FROM aliases; DELETE FROM exclusions;")
            .map_err(|e| e.to_string())?;
        let mut statement = transaction
            .prepare("INSERT INTO directories VALUES(?1,?2)")
            .map_err(|e| e.to_string())?;
        for (path, direct) in &model.directories {
            statement
                .execute(params![
                    path.to_str().ok_or("Invalid cached path")?,
                    direct.encode()
                ])
                .map_err(|e| e.to_string())?;
        }
        let mut statement = transaction
            .prepare("INSERT INTO aliases VALUES(?1,?2)")
            .map_err(|e| e.to_string())?;
        for (path, file) in &model.aliases {
            statement
                .execute(params![
                    path.to_str().ok_or("Invalid cached path")?,
                    serde_json::to_string(file).map_err(|e| e.to_string())?
                ])
                .map_err(|e| e.to_string())?;
        }
        let mut statement = transaction
            .prepare("INSERT INTO exclusions VALUES(?1,?2,?3)")
            .map_err(|e| e.to_string())?;
        for (path, (kind, message)) in &model.exclusions {
            statement
                .execute(params![
                    path.to_str().ok_or("Invalid cached path")?,
                    kind,
                    message
                ])
                .map_err(|e| e.to_string())?;
        }
    }
    transaction
        .execute_batch("DELETE FROM metadata;")
        .map_err(|e| e.to_string())?;
    transaction
        .execute(
            "INSERT INTO metadata VALUES('scope',?1),('generation',?2),('checksum',?3)",
            params![scope, generation, checksum],
        )
        .map_err(|e| e.to_string())?;
    transaction
        .pragma_update(None, "user_version", STORE_VERSION)
        .map_err(|e| e.to_string())?;
    if allocated_store_bytes(path) > budget {
        return Err("Compact transaction exceeds its allocated cache budget".into());
    }
    transaction.commit().map_err(|e| e.to_string())?;
    Ok(unchanged)
}
fn allocated_store_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-journal", path.display())),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
    .iter()
    .filter_map(|path| fs::symlink_metadata(path).ok())
    .map(|meta| allocated(&meta))
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        cache: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!(
                    "macdisk-audit-{}-{}-{}",
                    std::process::id(),
                    now_ms(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            let root = base.join("scope");
            let cache = base.join("cache/inventory.index.sqlite");
            fs::create_dir_all(root.join("a")).unwrap();
            fs::create_dir_all(root.join("z")).unwrap();
            fs::create_dir_all(cache.parent().unwrap()).unwrap();
            Self { base, root, cache }
        }
        fn record(&self, recorder: &Recorder) {
            let mut pending = vec![self.root.clone()];
            while let Some(path) = pending.pop() {
                let meta = InventoryMeta::read(&path).unwrap();
                let mut layer = recorder.layer(&path, &meta);
                let mut frame = DirectoryFrame::open(&path, true).unwrap();
                while let Some(node) = frame.next() {
                    let node = node.unwrap();
                    let meta = frame.node_metadata(&node).unwrap();
                    if meta.directory {
                        pending.push(node.path);
                    } else {
                        layer.observe(&node.path, &meta);
                    }
                }
                recorder.merge(layer);
            }
        }
        fn begin(&self, reuse: bool) -> Session {
            Session::begin_audit(
                &self.cache,
                "audited-scope",
                std::slice::from_ref(&self.root),
                true,
                16 * MIB,
                reuse,
            )
            .unwrap()
        }
        fn run(&self) -> Arc<Run> {
            let run = super::super::tests::fixture_run(&self.root);
            let (bytes, files, directories) = self.oracle();
            {
                let mut snapshot = run.snapshot.lock().unwrap();
                snapshot.allocated_bytes = bytes;
                snapshot.logical_bytes = self.logical_oracle();
                snapshot.scanned_files = files;
                snapshot.scanned_directories = directories;
            }
            run
        }
        fn audit(&self, reuse: bool) -> Prepared {
            let mut session = self.begin(reuse);
            self.record(&session.recorder());
            let model = session.recorder.0.model.lock().unwrap().clone();
            let commit = session.finish_audit(&self.run()).unwrap().unwrap();
            let mut prepared = model
                .prepare(std::slice::from_ref(&self.root), commit.diagnostics)
                .unwrap();
            prepared.persistence_error = commit.persistence_error;
            prepared
        }
        fn model(&self) -> Model {
            let recorder = Recorder::new(64 * MIB as usize, HashSet::new());
            self.record(&recorder);
            let model = recorder.0.model.lock().unwrap().clone();
            model
        }
        fn logical_oracle(&self) -> u64 {
            let mut pending = vec![self.root.clone()];
            let mut seen = HashSet::new();
            let mut logical = 0;
            while let Some(path) = pending.pop() {
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_dir() {
                    pending.extend(
                        fs::read_dir(path)
                            .unwrap()
                            .map(|entry| entry.unwrap().path()),
                    );
                } else if metadata.nlink() <= 1 || seen.insert((metadata.dev(), metadata.ino())) {
                    logical += metadata.len();
                }
            }
            logical
        }
        fn oracle(&self) -> (u64, u64, u64) {
            let mut pending = vec![self.root.clone()];
            let mut identities = HashSet::new();
            let (mut bytes, mut files, mut dirs) = (0u64, 0u64, 0u64);
            while let Some(path) = pending.pop() {
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    dirs += 1;
                    bytes += allocated(&meta);
                    pending.extend(
                        fs::read_dir(path)
                            .unwrap()
                            .map(|entry| entry.unwrap().path()),
                    );
                } else {
                    files += 1;
                    if meta.nlink() <= 1 || identities.insert((meta.dev(), meta.ino())) {
                        bytes += allocated(&meta);
                    }
                }
            }
            (bytes, files, dirs)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    #[test]
    fn flat_files_use_only_directory_rows_and_bounded_hot_samples() {
        let fixture = Fixture::new();
        for index in 0..512 {
            fs::write(fixture.root.join(format!("file-{index}")), [0u8; 8]).unwrap();
        }
        let model = fixture.model();
        assert_eq!(model.directories.len(), 3);
        assert!(model.aliases.is_empty());
        let measured = fixture.audit(false);
        assert_eq!(measured.scanned_files, 512);
        assert_eq!(measured.scanned_directories, 3);
        assert_eq!(measured.allocated_bytes, fixture.oracle().0);
    }
    #[test]
    fn immediate_8k_file_changes_are_audited_without_event_waits() {
        let fixture = Fixture::new();
        for group in 0..32 {
            fs::create_dir_all(fixture.root.join(format!("group-{group}"))).unwrap();
        }
        for file in 0..8192 {
            fs::write(
                fixture
                    .root
                    .join(format!("group-{}/file-{file}", file % 32)),
                [0u8; 16],
            )
            .unwrap();
        }
        let baseline = fixture.audit(false);
        assert_eq!(baseline.scanned_files, 8192);
        for step in 0..9 {
            let target = fixture
                .root
                .join(format!("group-{}/file-{}", step % 32, step));
            fs::write(target, vec![1u8; (step + 3) * 8192]).unwrap();
            // No sleep/cookie/event flush: operations completed before native
            // enumeration must be represented by this audited endpoint.
            let measured = fixture.audit(true);
            let oracle = fixture.oracle();
            assert_eq!(
                (
                    measured.allocated_bytes,
                    measured.scanned_files,
                    measured.scanned_directories
                ),
                oracle,
                "immediate mutation step {step}"
            );
            assert_eq!(measured.diagnostics.metadata_entries, oracle.1 + oracle.2);
            assert_eq!(measured.diagnostics.metadata_entries, oracle.1 + oracle.2);
        }
    }
    #[test]
    fn closed_session_and_offline_changes_are_measured_from_metadata() {
        let fixture = Fixture::new();
        let path = fixture.root.join("a/data");
        fs::write(&path, [1u8; 8192]).unwrap();
        let before = fixture.audit(false);
        fs::write(&path, [2u8; 65536]).unwrap();
        fs::write(fixture.root.join("z/new"), [3u8; 12288]).unwrap();
        assert!(fixture.begin(true).has_baseline());
        let after = fixture.audit(true);
        assert!(after.allocated_bytes > before.allocated_bytes);
        assert_eq!(after.allocated_bytes, fixture.oracle().0);
        assert_eq!(after.scanned_files, 2);
        let connection = Connection::open(&fixture.cache).unwrap();
        let checkpoints: u64 = connection
            .query_row(
                "SELECT count(*) FROM metadata WHERE key='checkpoints'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            checkpoints, 0,
            "Freshness must not depend on event checkpoints"
        );
    }
    #[test]
    fn hardlink_ownership_is_lexical_and_external_writes_are_audited() {
        let fixture = Fixture::new();
        let a = fixture.root.join("a/canonical");
        let z = fixture.root.join("z/alias");
        let external = fixture.base.join("external");
        fs::write(&z, [1u8; 8192]).unwrap();
        fs::hard_link(&z, &a).unwrap();
        fs::hard_link(&z, &external).unwrap();
        let before = fixture.audit(false);
        fs::write(&external, [2u8; 65536]).unwrap();
        let after = fixture.audit(true);
        assert!(after.allocated_bytes > before.allocated_bytes);
        assert_eq!(after.allocated_bytes, fixture.oracle().0);
        assert_eq!(after.scanned_files, 2);
        assert_eq!(
            after.directories[&string(&fixture.root.join("z"))].allocated_bytes,
            InventoryMeta::read(&fixture.root.join("z"))
                .unwrap()
                .allocated
        );
    }
    #[test]
    fn newly_created_cross_boundary_hardlink_needs_no_event_identity_proof() {
        let fixture = Fixture::new();
        let a = fixture.root.join("a/original");
        fs::write(&a, [1u8; 8192]).unwrap();
        fixture.audit(false);
        fs::hard_link(&a, fixture.root.join("z/new-alias")).unwrap();
        let after = fixture.audit(true);
        assert_eq!(after.scanned_files, 2);
        assert_eq!(after.allocated_bytes, fixture.oracle().0);
    }
    #[test]
    fn formerly_linked_singleton_stays_in_the_durable_ledger() {
        let fixture = Fixture::new();
        let a = fixture.root.join("a/original");
        let z = fixture.root.join("z/alias");
        fs::write(&a, [1u8; 8192]).unwrap();
        fs::hard_link(&a, &z).unwrap();
        fixture.audit(false);
        fs::remove_file(z).unwrap();
        let after = fixture.audit(true);
        assert_eq!(after.scanned_files, 1);
        let model = load(&fixture.cache, "audited-scope", 64 * MIB as usize)
            .unwrap()
            .unwrap();
        assert_eq!(model.aliases.len(), 1);
        assert_eq!(model.aliases[&a].links, 1);
        assert_eq!(after.allocated_bytes, fixture.oracle().0);
    }
    #[test]
    fn dynamic_exclusion_does_not_hide_materialized_data_on_next_audit() {
        let fixture = Fixture::new();
        let path = fixture.root.join("a/placeholder");
        let mut session = fixture.begin(false);
        session
            .recorder()
            .exclude(&path, "cloud-excluded", "Formerly dataless");
        fixture.record(&session.recorder());
        let model = session.recorder.0.model.lock().unwrap().clone();
        session.finish_audit(&fixture.run()).unwrap().unwrap();
        assert!(
            !model
                .prepare(std::slice::from_ref(&fixture.root), Diagnostics::default())
                .unwrap()
                .coverage_complete
        );
        fs::write(path, [1u8; 8192]).unwrap();
        let after = fixture.audit(true);
        assert!(after.coverage_complete);
        assert_eq!(after.scanned_files, 1);
    }
    #[test]
    fn exclusions_mark_only_retained_ancestors_partial() {
        let fixture = Fixture::new();
        let mut model = fixture.model();
        model.exclusions.insert(
            fixture.root.join("a/excluded"),
            ("permission-excluded".into(), "Excluded fixture".into()),
        );
        let prepared = model
            .prepare(std::slice::from_ref(&fixture.root), Diagnostics::default())
            .unwrap();
        assert!(!prepared.coverage_complete);
        assert!(!prepared.directories[&string(&fixture.root)].complete);
        assert!(!prepared.directories[&string(&fixture.root.join("a"))].complete);
        assert!(prepared.directories[&string(&fixture.root.join("z"))].complete);
    }
    #[test]
    fn unmerged_duplicate_cancelled_and_overflowed_layers_cannot_commit() {
        let fixture = Fixture::new();
        let run = fixture.run();
        let mut session = fixture.begin(false);
        let meta = InventoryMeta::read(&fixture.root).unwrap();
        let layer = session.recorder().layer(&fixture.root, &meta);
        assert!(session.finish_audit(&run).unwrap().is_none());
        drop(layer);
        assert!(session.finish_audit(&run).unwrap().is_none());
        assert!(!fixture.cache.exists());
        let mut duplicate = fixture.begin(false);
        fixture.record(&duplicate.recorder());
        fixture.record(&duplicate.recorder());
        assert!(duplicate.finish_audit(&run).unwrap().is_none());
        let recorder = Recorder::new(1, HashSet::new());
        let layer = recorder.layer(&fixture.root, &meta);
        recorder.merge(layer);
        assert!(recorder.0.failed.load(Ordering::Relaxed));
        let mut cancelled = fixture.begin(false);
        fixture.record(&cancelled.recorder());
        run.cancelled.store(true, Ordering::Relaxed);
        assert!(cancelled.finish_audit(&run).unwrap().is_none());
    }
    #[test]
    fn durable_generation_survives_budget_rejection_and_corruption_recovers() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a/data"), [1u8; 8192]).unwrap();
        fixture.audit(false);
        let model = load(&fixture.cache, "audited-scope", 64 * MIB as usize)
            .unwrap()
            .unwrap();
        let before = fs::read(&fixture.cache).unwrap();
        assert!(save(
            &fixture.cache,
            "replacement",
            &model,
            "test-generation",
            1024,
            false
        )
        .is_err());
        assert_eq!(before, fs::read(&fixture.cache).unwrap());
        fs::write(&fixture.cache, b"corrupt SQLite").unwrap();
        let recovered = fixture.audit(true);
        assert!(recovered.persistence_error.is_none());
        assert!(fixture.begin(true).has_baseline());
    }
    #[test]
    fn loaded_aliases_cannot_escape_the_requested_scope() {
        let fixture = Fixture::new();
        let mut model = fixture.model();
        let outside = fixture.base.join("outside");
        fs::write(&outside, [1u8; 16]).unwrap();
        let mut meta = InventoryMeta::read(&outside).unwrap();
        meta.links = 2;
        model
            .aliases
            .insert(outside.clone(), File::new(&outside, &meta));
        assert!(model
            .validate_scope(std::slice::from_ref(&fixture.root), true)
            .is_err());
    }
    #[test]
    fn missing_accepted_file_layer_cannot_replace_a_committed_generation() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a/accepted-file"), [1u8; 8192]).unwrap();
        fixture.audit(false);
        let before = fs::read(&fixture.cache).unwrap();
        let mut session = fixture.begin(true);
        let recorder = session.recorder();
        // Simulates a producer accepting a file outside any inventory frame.
        // The independent native oracle still counts that accepted node.
        for path in [
            &fixture.root,
            &fixture.root.join("a"),
            &fixture.root.join("z"),
        ] {
            recorder.merge(recorder.layer(path, &InventoryMeta::read(path).unwrap()));
        }
        assert!(session
            .finish_audit(&fixture.run())
            .unwrap_err()
            .contains("accepted by the native traversal"));
        assert_eq!(before, fs::read(&fixture.cache).unwrap());
    }
    #[test]
    fn unchanged_compact_rows_only_advance_the_generation_header() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a/data"), [1u8; 8192]).unwrap();
        fixture.audit(false);
        let model = load(&fixture.cache, "audited-scope", 64 * MIB as usize)
            .unwrap()
            .unwrap();
        let connection = Connection::open(&fixture.cache).unwrap();
        connection.execute_batch("CREATE TRIGGER forbid_directory_delete BEFORE DELETE ON directories BEGIN SELECT RAISE(ABORT,'unexpected directory rewrite'); END;").unwrap();
        drop(connection);
        let mut session = fixture.begin(true);
        fixture.record(&session.recorder());
        let run = fixture.run();
        run.snapshot.lock().unwrap().scan_id = "generation-2".into();
        let commit = session.finish_audit(&run).unwrap().unwrap();
        assert!(commit.persistence_error.is_none());
        assert_eq!(
            commit.diagnostics.reused_directories,
            model.directories.len() as u64
        );
        let connection = Connection::open(&fixture.cache).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='generation'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "generation-2"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='hot'",
                    [],
                    |row| row.get::<_, u64>(0)
                )
                .unwrap(),
            0
        );
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_none());
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "generation-2",
            &fixture.root
        )
        .unwrap()
        .is_some());
    }
    #[test]
    fn cached_deep_layer_survives_deleted_filesystem_with_global_link_ownership() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.root.join("z/deep/grandchild")).unwrap();
        fs::create_dir_all(fixture.root.join("z/deep/grandchild/second/third/fourth")).unwrap();
        fs::write(
            fixture.root.join("z/deep/grandchild/second/third/payload"),
            [2u8; 4096],
        )
        .unwrap();
        fs::write(fixture.root.join("a/owner"), [1u8; 8192]).unwrap();
        fs::hard_link(
            fixture.root.join("a/owner"),
            fixture.root.join("z/deep/alias"),
        )
        .unwrap();
        let measured = fixture.audit(false);
        let requested = fixture.root.join("z/deep");
        let expected = measured.directories[&string(&requested)].allocated_bytes;
        fs::remove_dir_all(&fixture.root).unwrap();
        let layer = cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &requested,
        )
        .unwrap()
        .unwrap();
        assert_eq!(layer.summary.allocated_bytes, expected);
        assert!(layer.detail_complete);
        assert!(layer
            .entries
            .iter()
            .any(|row| row.path == string(&requested.join("grandchild"))));
        assert!(layer
            .treemap_directories
            .iter()
            .any(|row| row.path == string(&requested.join("grandchild/second/third"))));
        assert!(!layer
            .treemap_directories
            .iter()
            .any(|row| row.path.ends_with("fourth")));
        assert!(
            layer.summary.allocated_bytes < 8192,
            "Canonical allocation remains outside requested scope"
        );
    }
    #[test]
    fn cached_layers_fail_closed_on_scope_generation_eviction_and_counter_corruption() {
        let fixture = Fixture::new();
        fixture.audit(false);
        assert!(cached_layer(
            &fixture.cache,
            "wrong-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_none());
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "stale",
            &fixture.root
        )
        .unwrap()
        .is_none());
        let connection = Connection::open(&fixture.cache).unwrap();
        let mut data: Vec<u8> = connection
            .query_row(
                "SELECT data FROM directories WHERE path=?1",
                params![string(&fixture.root)],
                |row| row.get(0),
            )
            .unwrap();
        let mut direct = Direct::decode(&data).unwrap();
        direct.allocated += 4096;
        data = direct.encode();
        connection
            .execute(
                "UPDATE directories SET data=?1 WHERE path=?2",
                params![data, string(&fixture.root)],
            )
            .unwrap();
        drop(connection);
        assert!(load(&fixture.cache, "audited-scope", 64 * MIB as usize)
            .unwrap_err()
            .contains("checksum"));
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_none());
        fs::remove_file(&fixture.cache).unwrap();
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_none());
    }
    #[test]
    fn byte_mismatch_cannot_commit_even_with_matching_file_and_directory_counts() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a/file"), [1u8; 8192]).unwrap();
        fixture.audit(false);
        let before = fs::read(&fixture.cache).unwrap();
        let mut session = fixture.begin(true);
        fixture.record(&session.recorder());
        let run = fixture.run();
        run.snapshot.lock().unwrap().allocated_bytes += 4096;
        assert!(session
            .finish_audit(&run)
            .unwrap_err()
            .contains("bytes disagree"));
        assert_eq!(before, fs::read(&fixture.cache).unwrap());
    }

    #[test]
    fn cache_eviction_waits_until_the_sqlite_reader_closes() {
        use std::sync::mpsc;
        let fixture = Fixture::new();
        fixture.audit(false);
        let (opened_tx, opened_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *READER_HOOK.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some((
            fixture.cache.clone(),
            Box::new(move || {
                opened_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            }),
        ));
        let path = fixture.cache.clone();
        let reader = std::thread::spawn(move || {
            load_generation(&path, "audited-scope", 64 * MIB as usize, Some("fixture"))
        });
        opened_rx.recv().unwrap();
        assert!(
            cache_storage_lock().0.try_lock().is_err(),
            "Open SQLite reader must own the physical-block accounting lock"
        );
        let path = fixture.cache.clone();
        let (evicting_tx, evicting_rx) = mpsc::channel();
        let evictor = std::thread::spawn(move || {
            evicting_tx.send(()).unwrap();
            let _storage = cache_storage_lock().lock().unwrap();
            fs::remove_file(path).unwrap();
        });
        evicting_rx.recv().unwrap();
        assert!(fixture.cache.exists());
        resume_tx.send(()).unwrap();
        assert!(reader.join().unwrap().unwrap().is_some());
        evictor.join().unwrap();
        assert!(!fixture.cache.exists());
    }
    #[test]
    fn header_probe_never_authorizes_corrupt_payload_or_header_only_reuse() {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a/file"), [1u8; 8192]).unwrap();
        fixture.audit(false);
        let connection = Connection::open(&fixture.cache).unwrap();
        connection
            .execute(
                "UPDATE directories SET data=?1 WHERE path=?2",
                params![vec![1u8, 2, 3], string(&fixture.root)],
            )
            .unwrap();
        drop(connection);
        // Compatible headers remain readable without decoding any old payload.
        // A full directory preload would reject this row before the native walk starts.
        assert!(probe_header(&fixture.cache, "audited-scope", 16 * MIB)
            .unwrap()
            .is_some());
        assert!(fixture.begin(true).has_baseline());
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_none());
        let audited = fixture.audit(true);
        assert_eq!(audited.allocated_bytes, fixture.oracle().0);
        assert!(audited.persistence_error.is_none());
        assert_eq!(audited.diagnostics.reused_directories, 0);
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn old_alias_hint_bytes_never_flow_into_a_fresh_singleton_measurement() {
        let fixture = Fixture::new();
        let original = fixture.root.join("a/original");
        let alias = fixture.root.join("z/alias");
        fs::write(&original, [1u8; 8192]).unwrap();
        fs::hard_link(&original, &alias).unwrap();
        fixture.audit(false);
        fs::remove_file(alias).unwrap();
        let connection = Connection::open(&fixture.cache).unwrap();
        let data: String = connection
            .query_row(
                "SELECT data FROM aliases WHERE path=?1",
                params![string(&original)],
                |row| row.get(0),
            )
            .unwrap();
        let mut old: File = serde_json::from_str(&data).unwrap();
        old.allocated += 1_000_000;
        old.logical += 2_000_000;
        connection
            .execute(
                "UPDATE aliases SET data=?1 WHERE path=?2",
                params![serde_json::to_string(&old).unwrap(), string(&original)],
            )
            .unwrap();
        drop(connection);
        assert!(
            fixture.begin(true).has_baseline(),
            "Bounded ID hints do not require loading old counters"
        );
        assert!(cached_layer(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            16 * MIB,
            "fixture",
            &fixture.root
        )
        .unwrap()
        .is_none());
        let measured = fixture.audit(true);
        assert_eq!(measured.allocated_bytes, fixture.oracle().0);
        assert_eq!(measured.logical_bytes, fixture.logical_oracle());
        let model = load(&fixture.cache, "audited-scope", 64 * MIB as usize)
            .unwrap()
            .unwrap();
        assert_eq!(model.aliases[&original].links, 1);
    }

    #[test]
    fn optional_hint_overflow_preserves_a_compatible_large_index() {
        let fixture = Fixture::new();
        let original = fixture.root.join("a/original");
        fs::write(&original, [1u8; 8192]).unwrap();
        let mut model = fixture.model();
        let mut meta = InventoryMeta::read(&original).unwrap();
        meta.links = (MAX_ALIAS_HINTS + 1) as u64;
        for index in 0..=MAX_ALIAS_HINTS {
            let path = fixture.root.join(format!("a/historical-alias-{index}"));
            model.aliases.insert(path.clone(), File::new(&path, &meta));
        }
        let direct = model.directories.get_mut(&fixture.root.join("a")).unwrap();
        direct.files = (MAX_ALIAS_HINTS + 1) as u64;
        direct.allocated = direct.own_allocated + meta.allocated * direct.files;
        direct.logical = meta.logical * direct.files;
        save(
            &fixture.cache,
            "audited-scope",
            &model,
            "large-generation",
            64 * MIB,
            false,
        )
        .unwrap();
        drop(model);
        let before = fs::read(&fixture.cache).unwrap();
        let hints = probe_header(&fixture.cache, "audited-scope", 64 * MIB)
            .unwrap()
            .unwrap();
        assert_eq!(
            hints.len(),
            1,
            "Bounded duplicate identity hints remain useful"
        );
        let session = Session::begin_audit(
            &fixture.cache,
            "audited-scope",
            std::slice::from_ref(&fixture.root),
            true,
            64 * MIB,
            true,
        )
        .unwrap();
        assert!(session.has_baseline());
        assert!(
            !session.replace_store,
            "Optional hint truncation is not cache corruption"
        );
        assert_eq!(before, fs::read(&fixture.cache).unwrap());
    }
}
