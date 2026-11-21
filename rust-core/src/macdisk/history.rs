//! Bounded metadata catalog for immutable completed scan archives.
//! Paths are derived locally; IDs from RPC callers are only matched to records.
use super::*;
use chrono::{Local, TimeZone};

const CATALOG_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 256 * 1024;
// Bound catalog RAM and publication separately from the shared snapshot budget.
const MAX_CATALOG_ROWS: usize = 384;
const MAX_SELECTIONS: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub scope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<Summary>,
    pub snapshots: Vec<Summary>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    length: u64,
    #[serde(default)]
    modified_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub summary: Summary,
    key: String,
    policy: u32,
    // A single component obtained from cache enumeration, never an RPC path.
    filename: String,
    stamp: Stamp,
}
impl Record {
    pub(super) fn is_current(&self) -> bool {
        cache_file(&self.key)
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .as_deref()
            == Some(self.filename.as_str())
    }
    pub(super) fn path(&self) -> Option<PathBuf> {
        safe_filename(&self.filename).then(|| cache_directory().map(|p| p.join(&self.filename)))?
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Selection {
    scope_id: String,
    id: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    version: u32,
    records: Vec<Record>,
    selections: Vec<Selection>,
}
#[derive(Default)]
struct State {
    directory: Option<PathBuf>,
    catalog_stamp: Option<Stamp>,
    catalog: Catalog,
    rejected: HashMap<String, (Stamp, SystemTime)>,
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}
fn hash(value: &str) -> u64 {
    value.bytes().fold(14695981039346656037, |h, b| {
        (h ^ b as u64).wrapping_mul(1099511628211)
    })
}
pub(super) fn scope_id(key: &str, policy: u32) -> String {
    format!("scope-{:016x}", hash(&format!("{policy}\n{key}")))
}
pub(super) fn snapshot_id(key: &str, snapshot: &DiskScan) -> String {
    format!(
        "snapshot-{:016x}",
        hash(&format!(
            "{}\n{}\n{}",
            scope_id(key, snapshot.index_policy_version),
            snapshot.scan_id,
            snapshot.started_at_ms
        ))
    )
}
fn safe_filename(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
}
fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CACHE_FILE_BYTES {
        return None;
    }
    #[cfg(unix)]
    let identity = (metadata.dev(), metadata.ino(), 0);
    #[cfg(not(unix))]
    let identity = (
        0,
        0,
        metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64,
    );
    Some(Stamp {
        device: identity.0,
        inode: identity.1,
        length: metadata.len(),
        modified_ms: identity.2,
    })
}
fn catalog_path(directory: &Path) -> PathBuf {
    directory.join("history.catalog")
}
fn read_catalog(directory: &Path) -> Catalog {
    let path = catalog_path(directory);
    let Some(identity) = stamp(&path).filter(|stamp| stamp.length <= MAX_CATALOG_BYTES) else {
        return Catalog::default();
    };
    let Some(bytes) = read_cache_bytes(&path) else {
        return Catalog::default();
    };
    if stamp(&path) != Some(identity) {
        return Catalog::default();
    }
    let mut catalog = serde_json::from_slice::<Catalog>(&bytes)
        .ok()
        .filter(|catalog| {
            catalog.version == CATALOG_VERSION
                && catalog.records.len() <= MAX_CATALOG_ROWS
                && catalog.selections.len() <= MAX_SELECTIONS
                && catalog
                    .records
                    .iter()
                    .all(|row| safe_filename(&row.filename))
        })
        .unwrap_or_default();
    // A catalog is an index, not evidence. On process reload validate every
    // cached summary against its archive header before presenting timestamps.
    catalog.records.retain(|row| {
        stamp(&directory.join(&row.filename)).as_ref() == Some(&row.stamp)
            && header(&directory.join(&row.filename), row.stamp.clone()).is_some_and(|actual| {
                actual.summary == row.summary
                    && actual.key == row.key
                    && actual.policy == row.policy
            })
    });
    catalog
}
fn save(directory: &Path, state: &mut State) {
    let Ok(_storage) = cache_storage_lock().lock() else {
        return;
    };
    state.catalog.version = CATALOG_VERSION;
    let Ok(bytes) = serde_json::to_vec(&state.catalog) else {
        return;
    };
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return;
    }
    if fs::create_dir_all(directory).is_err() {
        return;
    }
    let Ok(files) = cache_files(directory, false) else {
        return;
    };
    let reservation = (bytes.len() as u64)
        .saturating_mul(2)
        .saturating_add(16 * 1024);
    if cache_allocated_bytes(&files).saturating_add(reservation) > configured_cache_limit() * MIB {
        return;
    }
    let path = catalog_path(directory);
    let temporary = directory.join(format!("history-{}-{}.tmp", std::process::id(), now_ms()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(&temporary) {
        use std::io::Write;
        if file.write_all(&bytes).is_ok() && file.sync_all().is_ok() {
            let _ = fs::rename(&temporary, &path);
        }
    }
    let _ = fs::remove_file(temporary);
    state.catalog_stamp = stamp(&path);
}
// Deserialize only the source header. serde skips the directory index rather
// than allocating tens of thousands of DiskEntry values for a history menu.
#[derive(Deserialize)]
struct Header {
    version: u32,
    key: String,
    snapshot: SnapshotHeader,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotHeader {
    scan_id: String,
    status: String,
    #[serde(default)]
    freshness: Option<String>,
    roots: Vec<String>,
    started_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    index_policy_version: u32,
}
#[cfg(test)]
static HEADER_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(super) fn header_reads() -> usize {
    HEADER_READS.load(Ordering::Relaxed)
}
fn header(path: &Path, identity: Stamp) -> Option<Record> {
    #[cfg(test)]
    HEADER_READS.fetch_add(1, Ordering::Relaxed);
    let bytes = read_cache_bytes(path)?;
    let header: Header = serde_json::from_slice(&bytes).ok()?;
    if header.key.len() > 4096
        || header.snapshot.roots.len() > 8
        || header.snapshot.scan_id.len() > 256
        || header.version != CACHE_VERSION
        || header.snapshot.status != "complete"
        || matches!(
            header.snapshot.freshness.as_deref(),
            Some("partial" | "updating")
        )
        || cache_key(
            &header
                .snapshot
                .roots
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
            header.key.starts_with("protected=true\n"),
        ) != header.key
        || stamp(path) != Some(identity.clone())
    {
        return None;
    }
    let filename = path.file_name()?.to_str()?.to_string();
    // Verify the filename is one of this scope's generated cache destinations.
    let source = cache_file(&header.key)?;
    let current_name = source.file_name()?.to_str()?;
    let previous_name = source
        .with_extension("previous.json")
        .file_name()?
        .to_str()?
        .to_string();
    let id = format!(
        "snapshot-{:016x}",
        hash(&format!(
            "{}\n{}\n{}",
            scope_id(&header.key, header.snapshot.index_policy_version),
            header.snapshot.scan_id,
            header.snapshot.started_at_ms
        ))
    );
    if filename != current_name
        && filename != previous_name
        && filename != format!("retained-{id}.json")
    {
        return None;
    }
    Some(Record {
        summary: Summary {
            id,
            started_at_ms: header.snapshot.started_at_ms,
            updated_at_ms: header.snapshot.updated_at_ms,
        },
        key: header.key,
        policy: header.snapshot.index_policy_version,
        filename,
        stamp: identity,
    })
}
fn sync<'a>(directory: &Path, state: &'a mut State) -> &'a mut Catalog {
    let on_disk = stamp(&catalog_path(directory));
    if state.directory.as_deref() != Some(directory) || state.catalog_stamp != on_disk {
        state.catalog = read_catalog(directory);
        state.directory = Some(directory.to_path_buf());
        state.catalog_stamp = on_disk;
        state.rejected.clear();
    }
    let before = state.catalog.records.len();
    state.catalog.records.retain(|row| {
        safe_filename(&row.filename)
            && stamp(&directory.join(&row.filename)).as_ref() == Some(&row.stamp)
    });
    let mut changed = before != state.catalog.records.len();
    if let Ok(files) = cache_files(directory, false) {
        for file in files {
            if file.path.extension().is_none_or(|ext| ext != "json")
                || file.path.to_string_lossy().ends_with(".partial.json")
            {
                continue;
            }
            let Some(identity) = stamp(&file.path) else {
                continue;
            };
            if state
                .catalog
                .records
                .iter()
                .any(|row| directory.join(&row.filename) == file.path && row.stamp == identity)
            {
                continue;
            }
            let name = file
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let rejected_stamp = (identity.clone(), file.modified);
            if state.rejected.get(&name) == Some(&rejected_stamp) {
                continue;
            }
            if let Some(row) = header(&file.path, identity) {
                state.rejected.remove(&name);
                state.catalog.records.push(row);
                changed = true;
            } else {
                if state.rejected.len() >= MAX_CATALOG_ROWS {
                    if let Some(evicted) = state.rejected.keys().next().cloned() {
                        state.rejected.remove(&evicted);
                    }
                }
                state.rejected.insert(name, rejected_stamp);
            }
        }
    }
    state.catalog.records.sort_by_key(|row| {
        std::cmp::Reverse((row.summary.updated_at_ms, row.summary.started_at_ms))
    });
    state.catalog.records.truncate(MAX_CATALOG_ROWS);
    if changed {
        save(directory, state);
    }
    &mut state.catalog
}
pub(super) fn records(key: &str, policy: u32) -> Vec<Record> {
    let Ok(_storage) = cache_storage_lock().lock() else {
        return vec![];
    };
    let Some(directory) = cache_directory() else {
        return vec![];
    };
    let Ok(mut state) = state().lock() else {
        return vec![];
    };
    let catalog = sync(&directory, &mut state);
    let mut seen = HashSet::new();
    let current_name = cache_file(key)
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let mut rows: Vec<_> = catalog
        .records
        .iter()
        .filter(|row| row.key == key && row.policy == policy && policy > 0)
        .collect();
    rows.sort_by_key(|row| {
        (
            row.filename != current_name,
            std::cmp::Reverse((row.summary.updated_at_ms, row.summary.started_at_ms)),
        )
    });
    rows.into_iter()
        .filter(|row| seen.insert(row.summary.id.clone()))
        .cloned()
        .collect()
}
pub(super) fn select(key: &str, policy: u32, id: &str) {
    let Ok(_storage) = cache_storage_lock().lock() else {
        return;
    };
    let Some(directory) = cache_directory() else {
        return;
    };
    let Ok(mut state) = state().lock() else {
        return;
    };
    let catalog = sync(&directory, &mut state);
    let scope = scope_id(key, policy);
    if catalog
        .selections
        .iter()
        .any(|selection| selection.scope_id == scope && selection.id == id)
    {
        return;
    }
    if !catalog
        .records
        .iter()
        .any(|row| row.key == key && row.policy == policy && row.summary.id == id)
    {
        return;
    }
    catalog
        .selections
        .retain(|selection| selection.scope_id != scope);
    catalog.selections.push(Selection {
        scope_id: scope,
        id: id.into(),
    });
    if catalog.selections.len() > MAX_SELECTIONS {
        catalog.selections.remove(0);
    }
    save(&directory, &mut state);
}
// Before adjacent rotation, retain the outgoing previous generation under its
// stable ID. The hard link shares the immutable inode until previous is replaced.
pub(super) fn archive_previous(key: &str, write_id: u64) -> bool {
    let Ok(_storage) = cache_storage_lock().lock() else {
        return false;
    };
    let Some(previous) = previous_cache_file(key) else {
        return false;
    };
    let Some(identity) = stamp(&previous) else {
        return true;
    };
    let Some(row) = header(&previous, identity) else {
        return true;
    };
    let Some(directory) = previous.parent() else {
        return false;
    };
    let destination = directory.join(format!("retained-{}.json", row.summary.id));
    if stamp(&destination).as_ref() == Some(&row.stamp) {
        return true;
    }
    let temporary = directory.join(format!(
        "retained-{write_id}-{}-{}.tmp",
        std::process::id(),
        now_ms()
    ));
    let ok = fs::hard_link(&previous, &temporary)
        .and_then(|()| fs::rename(&temporary, destination))
        .is_ok();
    let _ = fs::remove_file(temporary);
    ok
}
fn day(timestamp: u64) -> Option<chrono::NaiveDate> {
    Local
        .timestamp_millis_opt(i64::try_from(timestamp).ok()?)
        .single()
        .map(|date| date.date_naive())
}
// Larger priority survives longer. Day roles use actual local completion dates,
// independent of cache LRU touches. Every current/previous/pin still yields to a
// hard byte budget when necessary; only the caller's new current is non-evictable.
pub(super) fn priorities(directory: &Path) -> HashMap<PathBuf, u8> {
    let Ok(_storage) = cache_storage_lock().lock() else {
        return HashMap::new();
    };
    let Ok(mut state) = state().lock() else {
        return HashMap::new();
    };
    let catalog = sync(directory, &mut state);
    let today = Local::now().date_naive();
    let mut groups: HashMap<(String, u32), Vec<&Record>> = HashMap::new();
    for row in &catalog.records {
        groups
            .entry((row.key.clone(), row.policy))
            .or_default()
            .push(row);
    }
    let mut result = HashMap::new();
    for ((key, policy), rows) in groups {
        let scope = scope_id(&key, policy);
        let selected = catalog
            .selections
            .iter()
            .find(|selection| selection.scope_id == scope)
            .map(|selection| selection.id.as_str());
        let first_today = rows
            .iter()
            .filter(|row| day(row.summary.updated_at_ms) == Some(today))
            .min_by_key(|row| row.summary.updated_at_ms)
            .map(|row| row.summary.id.as_str());
        let previous_day = rows
            .iter()
            .filter_map(|row| day(row.summary.updated_at_ms))
            .filter(|date| *date < today)
            .max();
        let previous_day_last = rows
            .iter()
            .filter(|row| day(row.summary.updated_at_ms) == previous_day)
            .max_by_key(|row| row.summary.updated_at_ms)
            .map(|row| row.summary.id.as_str());
        let mut days = HashSet::new();
        let mut recent_ids = HashSet::new();
        for row in rows {
            let recent = recent_ids.len() < 3 && recent_ids.insert(row.summary.id.as_str());
            let daily = day(row.summary.updated_at_ms).is_some_and(|date| days.insert(date));
            let priority = if selected == Some(row.summary.id.as_str()) {
                120
            } else if row.filename
                == cache_file(&key)
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default()
            {
                110
            } else if first_today == Some(row.summary.id.as_str())
                || previous_day_last == Some(row.summary.id.as_str())
            {
                90
            } else if row.filename.ends_with(".previous.json") {
                100
            } else if recent {
                70
            } else if daily {
                50
            } else {
                10
            };
            result.insert(directory.join(&row.filename), priority);
        }
    }
    result
}

#[cfg(test)]
pub(super) fn reset_memory() {
    *state().lock().unwrap() = State::default();
}
