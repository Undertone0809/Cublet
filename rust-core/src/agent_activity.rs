//! Read-only desktop history of the cleanup service's durable unlink receipts.
//!
//! Do not construct `AgentCleanup` here: its constructor creates a session and
//! restores jobs. History neither recovers jobs nor writes locks or receipts.
//! Deleted allocation is the sum recorded for unlinked files, not a measurement
//! of the change in available disk space.
//!
//! This is recent local evidence under the cleanup journal's existing retention,
//! not a permanent audit log. One settled reclaim operation replaces its child
//! executions; standalone executions remain individual entries. In-progress work
//! is absent. Persisted nonterminal receipts with known deletions are reported
//! as incomplete: read-only history cannot tell a live worker from a crashed one.
//! `skippedRecords` reports malformed, inconsistent, incomplete or orphaned
//! receipts without turning missing evidence into zero deletion. An inaccessible
//! journal or a global read limit returns an error rather than empty history.
use crate::agent_cleanup::AgentCleanup;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

// Cleanup writes at most 32 MiB per manifest into a 64 MiB journal. Also bound
// directory traversal and object count so unrelated tiny files cannot hang IPC.
const MAX_RECEIPT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_RECEIPTS: usize = 2_048;
const MAX_ACTIVITY: usize = 100;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: String,
    pub completed_at_ms: u64,
    pub deleted_allocated_bytes: u64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivity {
    pub entries: Vec<ActivityEntry>,
    pub skipped_records: usize,
    pub truncated: bool,
}

pub fn get_agent_activity() -> Result<AgentActivity, String> {
    read_activity(&AgentCleanup::default_journal_dir()?)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn terminal(value: &Value) -> bool {
    matches!(
        value["state"].as_str(),
        Some("completed" | "partial" | "cancelled" | "failed" | "interrupted")
    )
}

fn invalid(id: &str, reason: &str) -> String {
    format!("Cannot read cleanup activity receipt {id}: {reason}")
}

// Open every ancestor with O_NOFOLLOW. A missing journal is the sole filesystem
// absence that becomes empty history; permissions, aliases and I/O errors surface.
fn open_journal(path: &Path) -> Result<Option<File>, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("Cleanup activity journal must have an absolute path".into());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(|e| format!("Cannot open cleanup activity journal: {e}"))?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_bytes()).map_err(|_| "Invalid journal path")?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(format!("Cannot open cleanup activity journal: {error}"));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(Some(directory))
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn errno() -> *mut libc::c_int {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::__error()
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        libc::__errno_location()
    }
}

fn receipt_names(directory: &File) -> Result<Vec<String>, String> {
    let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err("Cannot duplicate cleanup activity journal handle".into());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err("Cannot enumerate cleanup activity journal".into());
    }
    let stream = Directory(stream);
    let mut names = Vec::new();
    for count in 0..=MAX_DIRECTORY_ENTRIES {
        unsafe { *errno() = 0 };
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if unsafe { *errno() } != 0 {
                return Err("Cannot enumerate cleanup activity journal".into());
            }
            return Ok(names);
        }
        if count == MAX_DIRECTORY_ENTRIES {
            return Err("Cleanup activity journal has too many directory entries".into());
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let Ok(name) = name.to_str() else { continue };
        // Supplementals, locks, temporary files and unrelated files are not
        // receipts. Their contents and any paths recorded inside JSON are unused.
        if name.strip_suffix(".json").is_some_and(valid_id) {
            names.push(name.to_owned());
            if names.len() > MAX_RECEIPTS {
                return Err("Cleanup activity journal has too many receipts".into());
            }
        }
    }
    unreachable!()
}

fn read_receipt(directory: &File, name: &str, used: &mut u64) -> Result<Option<Value>, String> {
    let id = name.trim_end_matches(".json");
    let name = CString::new(name).map_err(|_| invalid(id, "invalid filename"))?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(invalid(
            id,
            "file disappeared or cannot be inspected; refresh",
        ));
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
        return Ok(None);
    }
    // O_NONBLOCK prevents a replaced FIFO from hanging before fstat validates it.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(invalid(id, "file changed or cannot be opened; refresh"));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|_| invalid(id, "cannot inspect file"))?;
    if !metadata.is_file() || metadata.len() > MAX_RECEIPT_BYTES {
        return Err(invalid(id, "not a regular receipt within the size limit"));
    }
    *used = used.saturating_add(metadata.len());
    if *used > MAX_TOTAL_BYTES {
        return Err("Cleanup activity journal exceeds the read budget".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid(id, "cannot read file"))?;
    if bytes.len() as u64 != metadata.len() {
        return Err(invalid(id, "receipt changed while reading; refresh"));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid(id, "invalid JSON"))?;
    if value["jobId"].as_str() != Some(id) {
        return Err(invalid(id, "identifier mismatch"));
    }
    if !matches!(
        value["operation"].as_str(),
        Some("preview" | "execute" | "reclaim")
    ) || !matches!(
        value["state"].as_str(),
        Some(
            "planning"
                | "ready"
                | "running"
                | "measuring"
                | "completed"
                | "partial"
                | "cancelled"
                | "failed"
                | "interrupted"
        )
    ) {
        return Err(invalid(id, "unknown operation or state"));
    }
    Ok(Some(value))
}

fn activity_entry(id: &str, value: &Value) -> Result<Option<ActivityEntry>, String> {
    if !terminal(value) {
        return Ok(None);
    }
    let bytes = value["deletedAllocatedBytes"]
        .as_u64()
        .ok_or_else(|| invalid(id, "missing deleted allocation"))?;
    if bytes == 0 {
        return Ok(None);
    }
    let completed = value["completedAtMs"]
        .as_u64()
        .filter(|time| *time > 0)
        .ok_or_else(|| invalid(id, "missing completion time"))?;
    Ok(Some(ActivityEntry {
        id: id.to_owned(),
        completed_at_ms: completed,
        deleted_allocated_bytes: bytes,
    }))
}

fn read_activity(path: &Path) -> Result<AgentActivity, String> {
    let Some(directory) = open_journal(path)? else {
        return Ok(AgentActivity {
            entries: Vec::new(),
            skipped_records: 0,
            truncated: false,
        });
    };
    let mut used = 0;
    let mut receipts = HashMap::new();
    let mut skipped = 0;
    for name in receipt_names(&directory)? {
        match read_receipt(&directory, &name, &mut used) {
            Ok(Some(value)) => {
                receipts.insert(name.trim_end_matches(".json").to_owned(), value);
            }
            Ok(None) => {}
            Err(_) => {
                skipped += 1;
            }
        }
        if used > MAX_TOTAL_BYTES {
            return Err("Cleanup activity journal exceeds the read budget".into());
        }
    }
    let mut activity = summarize(&receipts);
    activity.skipped_records += skipped;
    Ok(activity)
}

fn completed_root<'a>(
    id: &str,
    value: &'a Value,
    receipts: &HashMap<String, Value>,
) -> Result<(Option<ActivityEntry>, HashSet<&'a str>), String> {
    let entry = activity_entry(id, value)?;
    let rows = value["childDetails"]
        .as_array()
        .ok_or_else(|| invalid(id, "missing child ledger"))?;
    let mut children = HashSet::new();
    let mut total = 0u64;
    for row in rows {
        let child = row["childOperationId"]
            .as_str()
            .filter(|child| valid_id(child) && *child != id)
            .ok_or_else(|| invalid(id, "invalid child identity"))?;
        if !children.insert(child) || row["parentOperationId"].as_str() != Some(id) {
            return Err(invalid(id, "duplicate or mismatched child ledger"));
        }
        if row["settled"] != true || !terminal(row) {
            return Err(invalid(id, "terminal operation has an unsettled child"));
        }
        let bytes = row["deletedAllocatedBytes"]
            .as_u64()
            .ok_or_else(|| invalid(id, "missing child allocation"))?;
        total = total
            .checked_add(bytes)
            .ok_or_else(|| invalid(id, "allocation overflow"))?;
        // A newer terminal root's ledger is authoritative even if retention
        // removed a child file. Never suppress a contradictory extant child.
        if let Some(child_receipt) = receipts.get(child) {
            if child_receipt["operation"] != "execute"
                || child_receipt["parentOperationId"].as_str() != Some(id)
                || !terminal(child_receipt)
                || child_receipt["deletedAllocatedBytes"].as_u64() != Some(bytes)
            {
                return Err(invalid(
                    id,
                    "child receipt disagrees with completed operation; refresh",
                ));
            }
        }
    }
    if value["deletedAllocatedBytes"].as_u64() != Some(total) {
        return Err(invalid(
            id,
            "child ledger allocation disagrees with operation",
        ));
    }
    if receipts.iter().any(|(child, receipt)| {
        receipt["operation"] == "execute"
            && receipt["parentOperationId"].as_str() == Some(id)
            && !children.contains(child.as_str())
    }) {
        return Err(invalid(
            id,
            "child receipt is missing from operation ledger",
        ));
    }
    Ok((entry, children))
}

fn summarize(receipts: &HashMap<String, Value>) -> AgentActivity {
    let mut entries = Vec::new();
    let mut grouped = HashSet::new();
    // A process may stop after publishing unlink counters and before a terminal
    // receipt. Never turn that durable evidence into complete empty history.
    let mut skipped_records = receipts
        .values()
        .filter(|value| {
            matches!(value["operation"].as_str(), Some("reclaim" | "execute"))
                && !terminal(value)
                && value["deletedAllocatedBytes"]
                    .as_u64()
                    .is_some_and(|bytes| bytes > 0)
        })
        .count();
    let mut completed_roots = Vec::new();
    let mut child_owners = HashMap::new();
    let mut conflicting_roots = HashSet::new();
    for (id, value) in receipts {
        if value["operation"] != "reclaim" || !terminal(value) {
            continue;
        }
        match completed_root(id, value, receipts) {
            Ok((entry, children)) => {
                for child in &children {
                    if let Some(previous) = child_owners.insert(*child, id) {
                        conflicting_roots.insert(previous);
                        conflicting_roots.insert(id);
                    }
                }
                completed_roots.push((id, entry, children));
            }
            Err(_) => {
                skipped_records += 1;
            }
        }
    }
    for (id, entry, children) in completed_roots {
        if conflicting_roots.contains(id) {
            skipped_records += 1;
            continue;
        }
        grouped.extend(children.into_iter().map(str::to_owned));
        if let Some(entry) = entry {
            entries.push(entry);
        }
    }
    // Parented executions never appear as separate user operations. Active roots
    // can lag child counters, so wait for the final consistent root ledger. An
    // orphan's visible children cannot prove the complete original dispatch set:
    // report skipped evidence instead of presenting their partial sum as a total.
    for (id, value) in receipts {
        if value["operation"] == "execute" && !grouped.contains(id) {
            if let Some(parent) = value.get("parentOperationId") {
                if terminal(value)
                    && (value["deletedAllocatedBytes"]
                        .as_u64()
                        .is_some_and(|bytes| bytes > 0)
                        || !parent
                            .as_str()
                            .and_then(|parent| receipts.get(parent))
                            .is_some_and(|root| root["operation"] == "reclaim" && !terminal(root)))
                {
                    skipped_records += 1;
                }
                continue;
            }
            match activity_entry(id, value) {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {}
                Err(_) => {
                    skipped_records += 1;
                }
            }
        }
    }
    entries.sort_by(|a, b| {
        b.completed_at_ms
            .cmp(&a.completed_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let truncated = entries.len() > MAX_ACTIVITY;
    entries.truncate(MAX_ACTIVITY);
    AgentActivity {
        entries,
        skipped_records,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    const ROOT: &str = "11111111111111111111111111111111";
    const CHILD: &str = "22222222222222222222222222222222";
    const SOLO: &str = "33333333333333333333333333333333";
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cublet-activity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn write(&self, id: &str, value: Value) {
            fs::write(
                self.0.join(format!("{id}.json")),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
        fn read(&self) -> AgentActivity {
            read_activity(&self.0).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn execute(id: &str, state: &str, bytes: u64, time: u64) -> Value {
        json!({"jobId":id,"operation":"execute","state":state,"deletedAllocatedBytes":bytes,"completedAtMs":time})
    }
    fn root(state: &str, bytes: u64) -> Value {
        json!({"jobId":ROOT,"operationId":ROOT,"operation":"reclaim","state":state,"completedAtMs":2000,"deletedAllocatedBytes":bytes,"childDetails":[{"childOperationId":CHILD,"parentOperationId":ROOT,"state":"completed","settled":true,"deletedAllocatedBytes":bytes}]})
    }
    fn child(state: &str, bytes: u64) -> Value {
        let mut value = execute(CHILD, state, bytes, 1000);
        value["parentOperationId"] = json!(ROOT);
        value
    }

    #[test]
    fn missing_journal_is_empty_and_creates_nothing() {
        let f = Fixture::new();
        let path = f.0.join("absent/subdirectory");
        assert_eq!(
            read_activity(&path).unwrap(),
            AgentActivity {
                entries: vec![],
                skipped_records: 0,
                truncated: false
            }
        );
        assert!(!f.0.join("absent").exists());
    }

    #[test]
    fn standalone_terminal_results_include_real_deleted_allocation_only() {
        let f = Fixture::new();
        for (index, state) in ["completed", "partial", "cancelled", "failed", "interrupted"]
            .into_iter()
            .enumerate()
        {
            let id = format!("{:032x}", index + 10);
            let mut value = execute(&id, state, (index as u64 + 1) * 4096, index as u64 + 100);
            value["availableBytesDelta"] = json!(999_999_999);
            f.write(&id, value);
        }
        let activity = f.read();
        assert_eq!(activity.entries.len(), 5);
        assert_eq!(activity.entries[0].deleted_allocated_bytes, 5 * 4096);
        assert_eq!(activity.skipped_records, 0);
        assert_eq!(
            serde_json::to_value(&activity.entries[0])
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn root_and_children_are_one_entry_and_root_survives_child_eviction() {
        let f = Fixture::new();
        f.write(ROOT, root("partial", 4096));
        f.write(CHILD, child("completed", 4096));
        let activity = f.read();
        assert_eq!(
            activity.entries,
            vec![ActivityEntry {
                id: ROOT.into(),
                completed_at_ms: 2000,
                deleted_allocated_bytes: 4096
            }]
        );
        fs::remove_file(f.0.join(format!("{CHILD}.json"))).unwrap();
        assert_eq!(f.read(), activity);
        f.write(ROOT, root("cancelled", 4096));
        assert_eq!(f.read(), activity);
    }

    #[test]
    fn large_root_ledger_receipts_are_readable_beyond_old_status_reader_limit() {
        let f = Fixture::new();
        let mut value = root("completed", 4096);
        value["padding"] = json!("x".repeat(256 * 1024));
        f.write(ROOT, value);
        assert_eq!(f.read().entries.len(), 1);
        assert_eq!(f.read().skipped_records, 0);
    }

    #[test]
    fn unlisted_child_or_conflicting_roots_cannot_double_count() {
        let f = Fixture::new();
        f.write(ROOT, root("completed", 4096));
        let mut extra = child("completed", 4096);
        extra["jobId"] = json!(SOLO);
        f.write(SOLO, extra);
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 2);
        let mut other = root("completed", 4096);
        other["jobId"] = json!(SOLO);
        other["operationId"] = json!(SOLO);
        other["childDetails"][0]["parentOperationId"] = json!(SOLO);
        f.write(SOLO, other);
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 2);
    }

    #[test]
    fn invalid_completion_or_base_schema_is_reported_instead_of_empty_success() {
        let f = Fixture::new();
        f.write(ROOT, json!({"jobId":ROOT}));
        f.write(CHILD, execute(CHILD, "completed", 4096, 0));
        f.write(SOLO, json!({"jobId":SOLO,"operation":"execute","state":"completed","completedAtMs":1234,"deletedAllocatedBytes":-1}));
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 3);
    }

    #[test]
    fn active_parent_does_not_leak_stale_root_or_finished_child_rows() {
        let f = Fixture::new();
        let mut value = root("running", 0);
        value["childDetails"][0]["settled"] = json!(false);
        f.write(ROOT, value);
        f.write(CHILD, child("completed", 4096));
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 1);
        f.write(ROOT, root("completed", 4096));
        assert_eq!(f.read().entries.len(), 1);
        assert_eq!(f.read().entries[0].id, ROOT);
    }

    #[test]
    fn persisted_nonterminal_unlinks_are_incomplete_and_repeat_reads_do_not_recover() {
        let f = Fixture::new();
        for root_bytes in [0, 4096] {
            let mut value = root("running", root_bytes);
            value["childDetails"][0]["state"] = json!("running");
            value["childDetails"][0]["settled"] = json!(false);
            value.as_object_mut().unwrap().remove("completedAtMs");
            let mut execution = child("running", 4096);
            execution.as_object_mut().unwrap().remove("completedAtMs");
            f.write(ROOT, value);
            f.write(CHILD, execution);
            let before: Vec<_> = [ROOT, CHILD]
                .into_iter()
                .map(|id| fs::read(f.0.join(format!("{id}.json"))).unwrap())
                .collect();
            let first = f.read();
            assert!(first.entries.is_empty());
            assert_eq!(first.skipped_records, if root_bytes > 0 { 2 } else { 1 });
            assert_eq!(f.read(), first);
            for (index, id) in [ROOT, CHILD].into_iter().enumerate() {
                assert_eq!(
                    fs::read(f.0.join(format!("{id}.json"))).unwrap(),
                    before[index]
                );
            }
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 2);
        }
        // A standalone execution has the same interrupted-publication boundary.
        fs::remove_file(f.0.join(format!("{ROOT}.json"))).unwrap();
        fs::remove_file(f.0.join(format!("{CHILD}.json"))).unwrap();
        f.write(SOLO, execute(SOLO, "measuring", 4096, 0));
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 1);
        f.write(SOLO, execute(SOLO, "running", 0, 0));
        assert_eq!(f.read().skipped_records, 0);
    }

    #[test]
    fn orphan_child_and_inconsistent_terminal_root_are_reported_as_skipped() {
        let f = Fixture::new();
        f.write(CHILD, child("completed", 4096));
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 1);
        f.write(ROOT, root("partial", 8192));
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 2);
        f.write(ROOT, root("partial", 4096));
        assert_eq!(f.read().skipped_records, 0);
    }

    #[test]
    fn incomplete_duplicate_or_mismatched_ledger_is_never_a_complete_total() {
        let f = Fixture::new();
        for mutation in 0..4 {
            let mut value = root("partial", 4096);
            match mutation {
                0 => value["childDetails"][0]["settled"] = json!(false),
                1 => {
                    let duplicate = value["childDetails"][0].clone();
                    value["childDetails"]
                        .as_array_mut()
                        .unwrap()
                        .push(duplicate);
                }
                2 => value["deletedAllocatedBytes"] = json!(8192),
                _ => value["childDetails"][0]["parentOperationId"] = json!(SOLO),
            }
            f.write(ROOT, value);
            assert!(f.read().entries.is_empty());
            assert_eq!(f.read().skipped_records, 1);
        }
    }

    #[test]
    fn malformed_receipts_preserve_readable_history_and_report_the_gap() {
        let f = Fixture::new();
        f.write(SOLO, execute(SOLO, "completed", 4096, 3000));
        fs::write(f.0.join(format!("{ROOT}.json")), b"{bad json").unwrap();
        f.write(CHILD, execute(SOLO, "completed", 4096, 2000));
        let activity = f.read();
        assert_eq!(activity.entries.len(), 1);
        assert_eq!(activity.skipped_records, 2);
        let serialized = serde_json::to_value(activity).unwrap();
        assert_eq!(serialized["skippedRecords"], 2);
        assert_eq!(serialized["truncated"], false);
    }

    #[test]
    fn matching_symlinks_and_supplementals_are_ignored_without_following() {
        let f = Fixture::new();
        fs::write(f.0.join("foreign"), b"not a receipt").unwrap();
        symlink(f.0.join("foreign"), f.0.join(format!("{ROOT}.json"))).unwrap();
        fs::write(f.0.join(format!("{CHILD}.plan.json")), b"not JSON").unwrap();
        fs::write(f.0.join(format!("{CHILD}.coordination.json")), b"not JSON").unwrap();
        assert_eq!(f.read().skipped_records, 0);
        assert!(f.read().entries.is_empty());
        let alias = f.0.join("alias");
        symlink(&f.0, &alias).unwrap();
        assert!(read_activity(&alias).is_err());
        assert!(read_activity(&alias.join("absent")).is_err());
        assert!(read_activity(&f.0.join("foreign")).is_err());
    }

    #[test]
    fn oversized_and_non_regular_primary_receipts_do_not_block_or_fake_empty() {
        let f = Fixture::new();
        let file = File::create(f.0.join(format!("{ROOT}.json"))).unwrap();
        file.set_len(MAX_RECEIPT_BYTES + 1).unwrap();
        let fifo = CString::new(f.0.join(format!("{CHILD}.json")).as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(f.read().skipped_records, 2);
    }

    #[test]
    fn zero_deletion_preview_and_running_work_are_not_cleanup_history() {
        let f = Fixture::new();
        f.write(SOLO, execute(SOLO, "completed", 0, 3000));
        f.write(
            ROOT,
            json!({"jobId":ROOT,"operation":"preview","state":"ready"}),
        );
        f.write(CHILD, execute(CHILD, "running", 0, 2000));
        assert!(f.read().entries.is_empty());
        assert_eq!(f.read().skipped_records, 0);
    }

    #[test]
    fn recent_history_is_bounded_sorted_and_read_only() {
        let f = Fixture::new();
        for index in 0..105 {
            let id = format!("{index:032x}");
            f.write(&id, execute(&id, "completed", 4096, index + 1));
        }
        let files = || {
            let mut files: Vec<_> = fs::read_dir(&f.0)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    (e.file_name(), fs::read(e.path()).unwrap())
                })
                .collect();
            files.sort();
            files
        };
        let before = files();
        let activity = f.read();
        assert!(activity.truncated);
        assert_eq!(activity.entries.len(), MAX_ACTIVITY);
        assert_eq!(activity.entries.first().unwrap().completed_at_ms, 105);
        assert_eq!(activity.entries.last().unwrap().completed_at_ms, 6);
        assert_eq!(files(), before);
        assert_eq!(f.read(), activity);
    }
}
