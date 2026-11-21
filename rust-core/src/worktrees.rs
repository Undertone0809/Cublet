use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

static REMOVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub project: String,
    pub branch: String,
    pub head: String,
    pub common_dir: String,
    pub registration_id: String,
    pub bytes: u64,
    pub changed_files: u64,
    pub ignored_files: u64,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub dirty: Option<bool>,
    pub locked: bool,
    pub main: bool,
    pub can_remove: bool,
    pub reason: String,
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("/usr/bin/git");
    cmd.env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(dir)
        .args(args);
    let output = crate::processes::bounded_output(&mut cmd, Duration::from_secs(15))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

fn confined(home: &Path, path: &Path) -> Result<PathBuf, String> {
    let home = home.canonicalize().map_err(|e| e.to_string())?;
    let root = home.join("worktrees");
    if fs::symlink_metadata(&root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Worktrees root is a symlink".into());
    }
    let canonical = path.canonicalize().map_err(|e| e.to_string())?;
    if canonical == root || !canonical.starts_with(&root) {
        return Err("Outside Codex worktrees".into());
    }
    // Reject symlink components even if their targets happen to stay inside the root.
    let lexical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        return Err("Absolute path required".into());
    };
    let relative = lexical
        .strip_prefix(&root)
        .map_err(|_| "Path must use canonical Codex home")?;
    let mut current = root;
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err("Invalid path component".into());
        }
        current.push(component);
        if fs::symlink_metadata(&current)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("Symlink worktree is protected".into());
        }
    }
    Ok(canonical)
}

fn epoch_ms(value: SystemTime) -> Option<i64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
}

fn worktree_stats(path: &Path) -> Result<(u64, Option<i64>, Option<i64>), String> {
    let root = fs::metadata(path).map_err(|e| e.to_string())?;
    let created_at_ms = root.created().ok().and_then(epoch_ms);
    let mut bytes = 0;
    let mut updated_at_ms = root.modified().ok().and_then(epoch_ms);
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|e| e.to_string())?;
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        if let Some(modified) = meta.modified().ok().and_then(epoch_ms) {
            updated_at_ms = Some(updated_at_ms.map_or(modified, |current| current.max(modified)));
        }
        if meta.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                bytes += meta.blocks() * 512;
            }
            #[cfg(not(unix))]
            {
                bytes += meta.len();
            }
        }
    }
    Ok((bytes, created_at_ms, updated_at_ms))
}

fn status_counts(output: &str) -> (u64, u64) {
    let mut changed = 0;
    let mut ignored = 0;
    for entry in output.split('\0').filter(|entry| !entry.is_empty()) {
        let bytes = entry.as_bytes();
        if bytes.get(2) != Some(&b' ') {
            continue;
        }
        if entry.starts_with("!!") {
            ignored += 1;
        } else {
            changed += 1;
        }
    }
    (changed, ignored)
}

fn linked_registration_id(path: &Path) -> Result<String, String> {
    let git_path = path.join(".git");
    let metadata = fs::symlink_metadata(&git_path).map_err(|e| e.to_string())?;
    if metadata.is_dir() {
        return Ok(String::new());
    }
    let contents = fs::read_to_string(&git_path).map_err(|e| e.to_string())?;
    let target = contents
        .trim()
        .strip_prefix("gitdir:")
        .ok_or("Invalid linked worktree .git file")?
        .trim();
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        git_path
            .parent()
            .ok_or("Invalid linked worktree .git path")?
            .join(target)
    };
    Ok(target
        .canonicalize()
        .map_err(|e| e.to_string())?
        .to_str()
        .ok_or("Non-UTF8 Git registration path")
        .map(str::to_owned)?)
}

fn inspect(home: &Path, path: &Path) -> Result<Worktree, String> {
    let path = confined(home, path)?;
    let mut item = Worktree {
        path: path.to_str().ok_or("Non-UTF8 path")?.into(),
        project: String::new(),
        branch: String::new(),
        head: String::new(),
        common_dir: String::new(),
        registration_id: String::new(),
        bytes: 0,
        changed_files: 0,
        ignored_files: 0,
        created_at_ms: None,
        updated_at_ms: None,
        dirty: None,
        locked: false,
        main: false,
        can_remove: false,
        reason: String::new(),
    };
    let (bytes, created_at_ms, updated_at_ms) = worktree_stats(&path)?;
    item.bytes = bytes;
    item.created_at_ms = created_at_ms;
    item.updated_at_ms = updated_at_ms;
    if fs::symlink_metadata(path.join(".git"))
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Symlink .git is protected".into());
    }
    item.registration_id = linked_registration_id(&path)?;
    let common = git_output(
        &path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = Path::new(common.trim())
        .canonicalize()
        .map_err(|e| e.to_string())?;
    item.common_dir = common.to_str().ok_or("Non-UTF8 Git path")?.into();
    item.project = common
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let records = git_output(&path, &["worktree", "list", "--porcelain", "-z"])?;
    let record = records
        .split("\0\0")
        .find(|r| {
            r.split('\0').next().is_some_and(|s| {
                s.strip_prefix("worktree ")
                    .is_some_and(|p| Path::new(p).canonicalize().ok().as_ref() == Some(&path))
            })
        })
        .ok_or("Not a registered Git worktree")?;
    item.main = fs::symlink_metadata(path.join(".git"))
        .map_err(|e| e.to_string())?
        .is_dir();
    for field in record.split('\0') {
        if let Some(branch) = field.strip_prefix("branch ") {
            item.branch = branch.trim_start_matches("refs/heads/").into();
        }
        if let Some(head) = field.strip_prefix("HEAD ") {
            item.head = head.into();
        }
        if field == "locked" || field.starts_with("locked ") {
            item.locked = true;
        }
        if field == "prunable" || field.starts_with("prunable ") {
            item.reason = "Prunable or unavailable registration".into();
        }
    }
    if item.branch.is_empty() {
        item.branch = "(detached HEAD)".into();
    }
    let status = git_output(
        &path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored",
            "--ignore-submodules=none",
        ],
    )?;
    let (changed_files, ignored_files) = status_counts(&status);
    item.changed_files = changed_files;
    item.ignored_files = ignored_files;
    item.dirty = Some(changed_files > 0 || ignored_files > 0);
    if item.main {
        item.reason = "Main worktree is protected".into();
    } else if item.locked {
        item.reason = "Locked worktree is protected".into();
    }
    // The confirmation dialog is the user-facing safety gate. Git remains the authority
    // for removal, but dirty, ignored, detached and hidden-index files are not blockers.
    item.can_remove = item.reason.is_empty();
    Ok(item)
}

fn list_at(home: &Path) -> Result<Vec<Worktree>, String> {
    let home = home.canonicalize().map_err(|e| e.to_string())?;
    let root = home.join("worktrees");
    if !root.exists() {
        return Ok(vec![]);
    }
    if fs::symlink_metadata(&root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Worktrees root is a symlink; not scanned".into());
    }
    let mut items = Vec::new();
    for top in fs::read_dir(&root).map_err(|e| e.to_string())? {
        let top = top.map_err(|e| e.to_string())?;
        let top_type = top.file_type().map_err(|e| e.to_string())?;
        // The Codex worktrees directory also contains marker files and empty
        // scratch folders. Neither is a Git worktree and neither belongs in
        // the destructive worktree UI.
        if !top_type.is_dir() || top_type.is_symlink() {
            continue;
        }
        let mut walker = WalkDir::new(top.path()).follow_links(false).into_iter();
        while let Some(entry) = walker.next() {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if entry.file_type().is_dir() && fs::symlink_metadata(path.join(".git")).is_ok() {
                // Invalid, stale or unregistered folders are deliberately
                // omitted instead of being presented as fake worktrees.
                if let Ok(item) = inspect(&home, path) {
                    items.push(item);
                }
                walker.skip_current_dir();
            }
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.bytes));
    Ok(items)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRemovalResult {
    pub removed: u32,
    pub failed: Vec<String>,
}

fn validate_expected(home: &Path, expected: &Worktree) -> Result<Worktree, String> {
    let fresh = inspect(home, Path::new(&expected.path))?;
    if !fresh.can_remove {
        return Err(fresh.reason);
    }
    if fresh.head != expected.head
        || fresh.common_dir != expected.common_dir
        || fresh.branch != expected.branch
        || fresh.registration_id != expected.registration_id
        || fresh.created_at_ms != expected.created_at_ms
    {
        return Err("Worktree registration changed; rescan and select again".into());
    }
    Ok(fresh)
}

fn remove_one_locked(fresh: &Worktree) -> Result<(), String> {
    // The user has confirmed the destructive action. Git remains the authority; --force
    // is intentional here so dirty and ignored files are removed without blind recursion.
    let common = Path::new(&fresh.common_dir);
    git_output(
        common,
        &["worktree", "remove", "--force", "--", &fresh.path],
    )?;
    if Path::new(&fresh.path).exists() {
        return Err("Git returned but worktree remains; rescan".into());
    }
    let records = git_output(common, &["worktree", "list", "--porcelain", "-z"])?;
    if records
        .split('\0')
        .any(|field| field == format!("worktree {}", fresh.path))
    {
        return Err("Git registration remains; rescan".into());
    }
    Ok(())
}

fn remove_at(home: &Path, expected: &Worktree) -> Result<(), String> {
    let _lock = REMOVE_LOCK
        .try_lock()
        .map_err(|_| "Another worktree removal is in progress")?;
    let fresh = validate_expected(home, expected)?;
    remove_one_locked(&fresh)
}

fn remove_many_at(home: &Path, expected: &[Worktree]) -> Result<WorktreeRemovalResult, String> {
    if expected.is_empty() {
        return Err("No worktrees selected".into());
    }
    let _lock = REMOVE_LOCK
        .try_lock()
        .map_err(|_| "Another worktree removal is in progress")?;
    let mut paths = HashSet::new();
    for item in expected {
        if !paths.insert(item.path.as_str()) {
            return Err("A worktree was selected more than once".into());
        }
    }
    // Reinspect every item before the first mutation so a stale batch cannot partially start.
    let fresh_items = expected
        .iter()
        .map(|item| validate_expected(home, item))
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = WorktreeRemovalResult {
        removed: 0,
        failed: Vec::new(),
    };
    for fresh in fresh_items {
        let path = fresh.path.clone();
        match remove_one_locked(&fresh) {
            Ok(()) => result.removed += 1,
            Err(error) => result.failed.push(format!("{path}: {error}")),
        }
    }
    Ok(result)
}

fn require_confirmation(confirmed: bool) -> Result<(), String> {
    if confirmed {
        Ok(())
    } else {
        Err("Explicit Worktree deletion confirmation is required".into())
    }
}

pub fn list_worktrees() -> Result<Vec<Worktree>, String> {
    list_at(&crate::scanner::home_codex_path()?)
}

pub fn remove_worktree(worktree: Worktree, confirmed: bool) -> Result<(), String> {
    require_confirmation(confirmed)?;
    crate::scanner::ensure_writer_stopped()?;
    remove_at(&crate::scanner::home_codex_path()?, &worktree)
}

pub fn remove_worktrees(
    worktrees: Vec<Worktree>,
    confirmed: bool,
) -> Result<WorktreeRemovalResult, String> {
    require_confirmation(confirmed)?;
    crate::scanner::ensure_writer_stopped()?;
    remove_many_at(&crate::scanner::home_codex_path()?, &worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[test]
    fn protects_detached_unique_commits_and_ignored_locked_stale_paths() {
        let home =
            std::env::temp_dir().join(format!("codex-worktree-safety-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("worktrees")).unwrap();
        let home = home.canonicalize().unwrap();
        let main = home.join("main");
        fs::create_dir(&main).unwrap();
        git(&main, &["init"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "main",
            ],
        );
        let worktree = home.join("worktrees/detached");
        git(
            &main,
            &["worktree", "add", "--detach", worktree.to_str().unwrap()],
        );
        git(
            &worktree,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "unique",
            ],
        );
        let unanchored = inspect(&home, &worktree).unwrap();
        assert!(
            unanchored.can_remove,
            "Detached worktrees can be removed after confirmation"
        );
        assert!(unanchored.created_at_ms.is_some());
        assert!(unanchored.updated_at_ms.is_some());
        assert!(!unanchored.registration_id.is_empty());
        git(&worktree, &["branch", "saved-detached"]);
        assert!(inspect(&home, &worktree).unwrap().can_remove);
        git(&main, &["worktree", "lock", worktree.to_str().unwrap()]);
        assert!(!inspect(&home, &worktree).unwrap().can_remove);
        git(&main, &["worktree", "unlock", worktree.to_str().unwrap()]);
        fs::write(main.join(".git/info/exclude"), ".env\n").unwrap();
        fs::write(worktree.join(".env"), "secret").unwrap();
        let ignored = inspect(&home, &worktree).unwrap();
        assert!(ignored.can_remove);
        assert!(ignored.ignored_files >= 1);
        fs::remove_file(worktree.join(".env")).unwrap();
        fs::write(worktree.join("tracked"), "original").unwrap();
        git(&worktree, &["add", "tracked"]);
        git(
            &worktree,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "tracked",
            ],
        );
        git(&worktree, &["branch", "saved-tracked"]);
        git(
            &worktree,
            &["update-index", "--assume-unchanged", "tracked"],
        );
        fs::write(worktree.join("tracked"), "hidden edit").unwrap();
        let assumed = inspect(&home, &worktree).unwrap();
        assert!(
            assumed.can_remove,
            "assume-unchanged does not block confirmed removal"
        );
        git(
            &worktree,
            &["update-index", "--no-assume-unchanged", "tracked"],
        );
        fs::write(worktree.join("tracked"), "original").unwrap();
        git(&worktree, &["update-index", "--skip-worktree", "tracked"]);
        fs::write(worktree.join("tracked"), "hidden edit").unwrap();
        let skipped = inspect(&home, &worktree).unwrap();
        assert!(
            skipped.can_remove,
            "skip-worktree does not block confirmed removal"
        );
        git(
            &worktree,
            &["update-index", "--no-skip-worktree", "tracked"],
        );
        fs::write(worktree.join("tracked"), "original").unwrap();
        let mut stale = inspect(&home, &worktree).unwrap();
        stale.head = "outdated".into();
        assert!(remove_at(&home, &stale).is_err());
        let mut stale_registration = inspect(&home, &worktree).unwrap();
        stale_registration.registration_id = "stale-registration".into();
        assert!(remove_at(&home, &stale_registration).is_err());
        assert!(require_confirmation(false).is_err());
        assert!(require_confirmation(true).is_ok());
        assert!(confined(&home, &main).is_err());
        assert!(confined(&home, &home.join("worktrees/../main")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&worktree, home.join("worktrees/link")).unwrap();
            assert!(confined(&home, &home.join("worktrees/link")).is_err());
        }
        fs::create_dir(home.join("worktrees/orphan")).unwrap();
        fs::write(home.join("worktrees/orphan/large-file"), vec![1u8; 16384]).unwrap();
        fs::write(home.join("worktrees/.metadata_never_index"), b"metadata").unwrap();
        let items = list_at(&home).unwrap();
        assert!(items
            .iter()
            .any(|w| w.path == worktree.display().to_string()));
        assert!(!items.iter().any(|w| w.path.ends_with("/orphan")));
        assert!(!items
            .iter()
            .any(|w| w.path.ends_with("/.metadata_never_index")));
        fs::remove_dir_all(home).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("/usr/bin/git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[test]
    fn individual_clean_worktree_removed_without_touching_main_or_sibling() {
        let home = std::env::temp_dir().join(format!("codex-worktrees-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("worktrees/main")).unwrap();
        let main = home.join("worktrees/main");
        git(&main, &["init"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        let clean = home.join("worktrees/one/project");
        let dirty = home.join("worktrees/two/project");
        fs::create_dir_all(clean.parent().unwrap()).unwrap();
        fs::create_dir_all(dirty.parent().unwrap()).unwrap();
        git(
            &main,
            &["worktree", "add", "-b", "one", clean.to_str().unwrap()],
        );
        git(
            &main,
            &["worktree", "add", "-b", "two", dirty.to_str().unwrap()],
        );
        fs::write(dirty.join("untracked.txt"), "keep me").unwrap();
        let items = list_at(&home).unwrap();
        assert_eq!(items.len(), 3);
        let candidate = items
            .iter()
            .find(|w| w.path == clean.canonicalize().unwrap().to_str().unwrap())
            .cloned()
            .unwrap();
        assert!(candidate.can_remove, "{:?}", candidate);
        assert!(
            !items
                .iter()
                .find(|w| w.path == main.canonicalize().unwrap().to_str().unwrap())
                .unwrap()
                .can_remove
        );
        let dirty_item = items
            .iter()
            .find(|w| w.path == dirty.canonicalize().unwrap().to_str().unwrap())
            .cloned()
            .unwrap();
        assert!(
            dirty_item.can_remove,
            "dirty worktrees are removable after confirmation"
        );
        assert_eq!(dirty_item.changed_files, 1);
        let result = remove_many_at(&home, &[candidate.clone(), dirty_item.clone()]).unwrap();
        assert_eq!(result.removed, 2);
        assert!(result.failed.is_empty());
        assert!(!clean.exists());
        assert!(!dirty.exists());
        assert!(main.exists());
        let list = Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let list_text = String::from_utf8_lossy(&list.stdout);
        assert!(!list_text.contains(clean.to_str().unwrap()));
        assert!(!list_text.contains(dirty.to_str().unwrap()));
        fs::remove_dir_all(home).unwrap();
    }
}
