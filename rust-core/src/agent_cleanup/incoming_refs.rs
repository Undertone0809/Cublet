//! Fresh, bounded incoming-reference closure for one mutation batch.
//!
//! The roots are the validated local Cargo/Go owner and, when broader, the explicit
//! authorization scope. Every directory alias found there extends the frontier.
//! This is a scoped observation, not proof of whole-volume link absence, nor an
//! atomic exclusion of uncooperative writers. Unknown closure retains the whole
//! candidate; callers must never interpret an error as an empty reference set.
use super::{cloud_path, open_child, placeholder, safe_root, stat_child, Identity};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 500_000;
const MAX_DIRECTORIES: usize = 20_000;
const MAX_ROOTS: usize = 128;
const MAX_PATH_BYTES: usize = 64 * 1024 * 1024;
const MAX_LINK_DEPTH: usize = 64;
const MAX_DIRECTORY_DEPTH: usize = 256;
const MAX_SECONDS: u64 = 6;
const MAX_SAMPLES: usize = 4;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Observation {
    state: &'static str,
    complete: bool,
    scoped_roots: usize,
    root_samples: Vec<String>,
    external_directory_roots: usize,
    inspected_entries: usize,
    inspected_directories: usize,
    inspected_symlinks: usize,
    protected_identities: usize,
    protected_directory_referents: usize,
    max_entries: usize,
    max_directories: usize,
    max_roots: usize,
    max_path_bytes: usize,
    max_link_depth: usize,
    max_directory_depth: usize,
    max_seconds: u64,
    coverage: &'static str,
}
impl Default for Observation {
    fn default() -> Self {
        Self {
            state: "not_reached",
            complete: false,
            scoped_roots: 0,
            root_samples: Vec::new(),
            external_directory_roots: 0,
            inspected_entries: 0,
            inspected_directories: 0,
            inspected_symlinks: 0,
            protected_identities: 0,
            protected_directory_referents: 0,
            max_entries: MAX_ENTRIES,
            max_directories: MAX_DIRECTORIES,
            max_roots: MAX_ROOTS,
            max_path_bytes: MAX_PATH_BYTES,
            max_link_depth: MAX_LINK_DEPTH,
            max_directory_depth: MAX_DIRECTORY_DEPTH,
            max_seconds: MAX_SECONDS,
            coverage: "not_inspected",
        }
    }
}
impl Observation {
    pub(super) fn unavailable() -> Self {
        Self {
            state: "inspection_unavailable",
            ..Self::default()
        }
    }
}

pub(super) struct Snapshot {
    identities: HashSet<(u64, u64)>,
    directory_referents: Vec<PathBuf>,
    observation: Observation,
    directories: HashMap<PathBuf, Identity>,
    links: HashMap<PathBuf, (Identity, PathBuf)>,
    referents: HashMap<PathBuf, Identity>,
    initial_roots: HashMap<PathBuf, Identity>,
    revalidation_budget: Duration,
}
impl Snapshot {
    pub(super) fn protects(&self, path: &Path, identity: &Identity) -> bool {
        self.identities.contains(&(identity.dev, identity.ino))
            || self.directory_referents.iter().any(|p| path.starts_with(p))
    }
    pub(super) fn observation(&self) -> Observation {
        self.observation.clone()
    }
    // Optimistic consistency check immediately before this batch's mutation.
    // Every later batch still requires an entirely fresh inspect().
    pub(super) fn revalidate(&self) -> Result<(), String> {
        revalidate_anchors(
            &self.directories,
            &self.links,
            &self.referents,
            &self.initial_roots,
            Instant::now(),
            self.revalidation_budget,
        )
    }
}

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    directories: usize,
    roots: usize,
    path_bytes: usize,
    elapsed: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: MAX_ENTRIES,
            directories: MAX_DIRECTORIES,
            roots: MAX_ROOTS,
            path_bytes: MAX_PATH_BYTES,
            elapsed: Duration::from_secs(MAX_SECONDS),
        }
    }
}
struct Inspector {
    started: Instant,
    limits: Limits,
    device: u64,
    entries: usize,
    path_bytes: usize,
    roots: Vec<PathBuf>,
    frontier: VecDeque<(PathBuf, usize)>,
    visited: HashSet<(u64, u64)>,
    // All anchors are checked again after the entire alias frontier closes.
    directories: HashMap<PathBuf, Identity>,
    initial_roots: HashMap<PathBuf, Identity>,
    links: HashMap<PathBuf, (Identity, PathBuf)>,
    files: Vec<(PathBuf, (u64, u64))>,
    references: HashSet<(u64, u64)>,
    referents: HashMap<PathBuf, Identity>,
    directory_referents: HashSet<PathBuf>,
}
fn unknown(reason: impl AsRef<str>) -> String {
    format!(
        "Incoming reference closure incomplete; candidate retained: {}",
        reason.as_ref()
    )
}
fn stat_identity(stat: &libc::stat) -> Identity {
    Identity::stat(stat)
}
fn stat_placeholder(stat: &libc::stat) -> bool {
    #[cfg(target_os = "macos")]
    {
        stat.st_flags & 0x40000000 != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = stat;
        false
    }
}
fn ordinary(stat: &libc::stat, mode: libc::mode_t) -> bool {
    stat.st_mode & libc::S_IFMT == mode
}
fn path_charge(path: &Path) -> usize {
    path.as_os_str().as_bytes().len()
}
// Paths can contain non-UTF-8 bytes. Only observation samples are lossy strings.
fn metadata_identity(file: &File) -> Result<Identity, String> {
    let metadata = file.metadata().map_err(|e| unknown(e.to_string()))?;
    if placeholder(&metadata) {
        return Err(unknown("cloud/dataless directory"));
    }
    Ok(Identity::metadata(&metadata))
}
fn read_link(parent: &File, name: &OsStr) -> Result<PathBuf, String> {
    let name = super::cstr(name).map_err(unknown)?;
    let mut bytes = vec![0u8; 16 * 1024];
    let count = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if count < 0 {
        return Err(unknown(std::io::Error::last_os_error().to_string()));
    }
    if count as usize == bytes.len() {
        return Err(unknown("symlink target byte budget exceeded"));
    }
    bytes.truncate(count as usize);
    if bytes.is_empty() {
        return Err(unknown("empty symlink target"));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

// Enumerate through a duplicate descriptor rather than reopening a path that
// could have become a symlink. fdopendir owns its duplicate, closed by this RAII.
struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
#[cfg(target_os = "macos")]
unsafe fn errno_pointer() -> *mut libc::c_int {
    libc::__error()
}
#[cfg(not(target_os = "macos"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    libc::__errno_location()
}
fn names(
    directory: &File,
    max: usize,
    started: Instant,
    elapsed: Duration,
) -> Result<Vec<OsString>, String> {
    let fd = unsafe { libc::dup(directory.as_raw_fd()) };
    if fd < 0 {
        return Err(unknown(std::io::Error::last_os_error().to_string()));
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(unknown(std::io::Error::last_os_error().to_string()));
    }
    let stream = DirectoryStream(stream);
    let mut result = Vec::new();
    loop {
        if started.elapsed() > elapsed {
            return Err(unknown("time budget exceeded"));
        }
        unsafe {
            *errno_pointer() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = unsafe { *errno_pointer() };
            if error != 0 {
                return Err(unknown(
                    std::io::Error::from_raw_os_error(error).to_string(),
                ));
            }
            break;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if result.len() >= max {
            return Err(unknown("entry budget exceeded"));
        }
        result.push(OsString::from_vec(bytes.to_vec()));
    }
    Ok(result)
}
impl Inspector {
    fn check(&self) -> Result<(), String> {
        if self.started.elapsed() > self.limits.elapsed {
            return Err(unknown("time budget exceeded"));
        }
        Ok(())
    }
    fn charge(&mut self, path: &Path) -> Result<(), String> {
        self.check()?;
        self.path_bytes = self.path_bytes.saturating_add(path_charge(path));
        if self.path_bytes > self.limits.path_bytes {
            return Err(unknown("path byte budget exceeded"));
        }
        Ok(())
    }
    fn root(&mut self, path: PathBuf) -> Result<(), String> {
        if path == Path::new("/") {
            return Err(unknown(
                "whole-volume root is outside bounded consumer closure",
            ));
        }
        if self.roots.iter().any(|p| path.starts_with(p)) {
            return Ok(());
        }
        if self.roots.len() >= self.limits.roots {
            return Err(unknown("root/frontier budget exceeded"));
        }
        self.charge(&path)?;
        let (_, handle) = safe_root(&path).map_err(unknown)?;
        if metadata_identity(&handle)?.dev != self.device {
            return Err(unknown("cross-volume directory alias"));
        }
        self.roots.push(path.clone());
        self.frontier.push_back((path, 0));
        Ok(())
    }
    fn directory_anchor(&mut self, path: &Path, identity: &Identity) -> Result<(), String> {
        if let Some(previous) = self.directories.get(path) {
            if previous != identity {
                return Err(unknown("directory changed during inspection"));
            }
        } else {
            if self.directories.len() >= self.limits.directories {
                return Err(unknown("directory budget exceeded"));
            }
            self.charge(path)?;
            self.directories.insert(path.into(), identity.clone());
        }
        Ok(())
    }
    fn link(
        &mut self,
        path: &Path,
        parent: &File,
        name: &OsStr,
        before: &libc::stat,
    ) -> Result<PathBuf, String> {
        self.check()?;
        if stat_placeholder(before) {
            return Err(unknown("cloud/dataless link"));
        }
        let target = read_link(parent, name)?;
        let after = stat_child(parent, name).map_err(unknown)?;
        let identity = stat_identity(before);
        if identity != stat_identity(&after) || !ordinary(&after, libc::S_IFLNK) {
            return Err(unknown("symlink changed while reading"));
        }
        if let Some((old_identity, old_target)) = self.links.get(path) {
            if old_identity != &identity || old_target != &target {
                return Err(unknown("symlink retargeted during inspection"));
            }
        } else {
            if self.links.len() >= self.limits.entries {
                return Err(unknown("symlink budget exceeded"));
            }
            self.charge(path)?;
            self.charge(&target)?;
            self.links.insert(path.into(), (identity, target.clone()));
        }
        Ok(target)
    }
    // Resolve every path component explicitly with no-follow descriptor checks.
    // Lexical '..' is applied only after preceding aliases are resolved: a/../x
    // must use the parent of a's referent, not the lexical parent of alias a.
    fn resolve(&mut self, path: &Path) -> Result<(PathBuf, Identity, bool), String> {
        let mut pending: VecDeque<OsString> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(v) => Some(v.to_os_string()),
                Component::ParentDir => Some(OsString::from("..")),
                _ => None,
            })
            .collect();
        if !path.is_absolute() {
            return Err(unknown("relative resolver input"));
        }
        let mut current = PathBuf::from("/");
        let mut seen = HashSet::new();
        let mut link_depth = 0;
        loop {
            self.check()?;
            let Some(name) = pending.pop_front() else {
                let (_, directory) = safe_root(&current).map_err(unknown)?;
                let identity = metadata_identity(&directory)?;
                if identity.dev != self.device {
                    return Err(unknown("cross-volume referent"));
                }
                return Ok((current, identity, true));
            };
            if name == ".." {
                current.pop();
                if current.as_os_str().is_empty() {
                    current.push("/");
                }
                continue;
            }
            let (_, parent) = safe_root(&current).map_err(unknown)?;
            let parent_before = metadata_identity(&parent)?;
            let stat = stat_child(&parent, &name).map_err(unknown)?;
            if stat_placeholder(&stat) {
                return Err(unknown("cloud/dataless referent"));
            }
            let child = current.join(&name);
            if cloud_path(&child) {
                return Err(unknown("cloud referent"));
            }
            if ordinary(&stat, libc::S_IFLNK) {
                link_depth += 1;
                if link_depth > MAX_LINK_DEPTH {
                    return Err(unknown("symlink depth budget exceeded"));
                }
                // Include the unresolved suffix: revisiting one symlink with a
                // different suffix is legal; the depth bound still terminates it.
                if !seen.insert((child.clone(), pending.clone())) {
                    return Err(unknown("symlink cycle"));
                }
                let target = self.link(&child, &parent, &name, &stat)?;
                let components: Vec<_> = target
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(v) => Some(v.to_os_string()),
                        Component::ParentDir => Some(OsString::from("..")),
                        _ => None,
                    })
                    .collect();
                if target.is_absolute() {
                    current = PathBuf::from("/");
                }
                for component in components.into_iter().rev() {
                    pending.push_front(component);
                }
            } else if ordinary(&stat, libc::S_IFDIR) {
                let opened = open_child(&parent, &name).map_err(unknown)?;
                let identity = metadata_identity(&opened)?;
                if identity != stat_identity(&stat) {
                    return Err(unknown("referent directory replaced"));
                }
                current = child;
                if pending.is_empty() {
                    if identity.dev != self.device {
                        return Err(unknown("cross-volume directory referent"));
                    }
                    if metadata_identity(&parent)? != parent_before {
                        return Err(unknown("referent parent changed"));
                    }
                    return Ok((current, identity, true));
                }
            } else if ordinary(&stat, libc::S_IFREG) && pending.is_empty() {
                let identity = stat_identity(&stat);
                if identity.dev != self.device {
                    return Err(unknown("cross-volume file referent"));
                }
                if stat_identity(&stat_child(&parent, &name).map_err(unknown)?) != identity {
                    return Err(unknown("file referent changed"));
                }
                if metadata_identity(&parent)? != parent_before {
                    return Err(unknown("referent parent changed"));
                }
                return Ok((child, identity, false));
            } else {
                return Err(unknown("unresolved or nonordinary referent"));
            }
            if metadata_identity(&parent)? != parent_before {
                return Err(unknown("referent parent changed"));
            }
        }
    }
    fn traverse(&mut self) -> Result<(), String> {
        while let Some((path, depth)) = self.frontier.pop_front() {
            self.check()?;
            if depth > MAX_DIRECTORY_DEPTH {
                return Err(unknown("directory depth budget exceeded"));
            }
            let (_, handle) = safe_root(&path).map_err(unknown)?;
            let before = metadata_identity(&handle)?;
            if before.dev != self.device {
                return Err(unknown("cross-volume directory"));
            }
            self.directory_anchor(&path, &before)?;
            if !self.visited.insert((before.dev, before.ino)) {
                continue;
            }
            // Permission-denied closures are unknown even under a privileged test
            // runner that could bypass mode bits and read the directory anyway.
            if handle
                .metadata()
                .map_err(|e| unknown(e.to_string()))?
                .permissions()
                .mode()
                & 0o444
                == 0
            {
                return Err(unknown("unreadable directory"));
            }
            let remaining = self.limits.entries.saturating_sub(self.entries);
            for name in names(&handle, remaining, self.started, self.limits.elapsed)? {
                self.entries += 1;
                let child = path.join(&name);
                self.charge(&child)?;
                let stat = stat_child(&handle, &name).map_err(unknown)?;
                if stat.st_dev as u64 != self.device {
                    return Err(unknown("cross-volume entry"));
                }
                if stat_placeholder(&stat) || cloud_path(&child) {
                    return Err(unknown("cloud/dataless entry"));
                }
                if ordinary(&stat, libc::S_IFDIR) {
                    let opened = open_child(&handle, &name).map_err(unknown)?;
                    let identity = metadata_identity(&opened)?;
                    if identity != stat_identity(&stat) {
                        return Err(unknown("directory entry replaced"));
                    }
                    self.directory_anchor(&child, &identity)?;
                    self.frontier.push_back((child, depth + 1));
                } else if ordinary(&stat, libc::S_IFLNK) {
                    // Bind the original link too; resolve observes every chained
                    // link (including aliases outside initial roots).
                    self.link(&child, &handle, &name, &stat)?;
                    let (referent, identity, directory) = self.resolve(&child)?;
                    self.references.insert((identity.dev, identity.ino));
                    if let Some(previous) =
                        self.referents.insert(referent.clone(), identity.clone())
                    {
                        if previous != identity {
                            return Err(unknown("referent changed during inspection"));
                        }
                    }
                    if directory {
                        if !self
                            .directory_referents
                            .iter()
                            .any(|p| referent.starts_with(p))
                        {
                            self.directory_referents
                                .retain(|p| !p.starts_with(&referent));
                            self.directory_referents.insert(referent.clone());
                        }
                        self.root(referent)?;
                    }
                } else if ordinary(&stat, libc::S_IFREG) {
                    self.files.push((child, (stat.st_dev as u64, stat.st_ino)));
                } else {
                    // Sockets/FIFOs/devices cannot hide directory entries. They
                    // are never traversed or opened and confer no delete authority.
                }
            }
            if metadata_identity(&handle)? != before {
                return Err(unknown("directory changed while enumerating"));
            }
            let (_, rebound) = safe_root(&path).map_err(unknown)?;
            if metadata_identity(&rebound)? != before {
                return Err(unknown("directory path changed while enumerating"));
            }
        }
        Ok(())
    }
    fn revalidate(&self) -> Result<(), String> {
        revalidate_anchors(
            &self.directories,
            &self.links,
            &self.referents,
            &self.initial_roots,
            self.started,
            self.limits.elapsed,
        )
    }
}

fn revalidate_anchors(
    directories: &HashMap<PathBuf, Identity>,
    links: &HashMap<PathBuf, (Identity, PathBuf)>,
    referents: &HashMap<PathBuf, Identity>,
    initial_roots: &HashMap<PathBuf, Identity>,
    started: Instant,
    elapsed: Duration,
) -> Result<(), String> {
    let check_time = || {
        if started.elapsed() > elapsed {
            Err(unknown("anchor revalidation time budget exceeded"))
        } else {
            Ok(())
        }
    };
    // Rebind original paths as well as held descriptors: a stable old handle
    // cannot authorize a closure observed under a replacement outer ancestor.
    for (path, original) in initial_roots {
        check_time()?;
        let (_, rebound) = safe_root(path).map_err(unknown)?;
        if metadata_identity(&rebound)? != *original
            || directories
                .get(path)
                .is_some_and(|inspected| inspected != original)
        {
            return Err(unknown(
                "original consumer/cache/authorization root rebound",
            ));
        }
    }
    for (path, before) in directories {
        check_time()?;
        let (_, handle) = safe_root(path).map_err(unknown)?;
        if metadata_identity(&handle)? != *before {
            return Err(unknown("directory changed before closure completed"));
        }
    }
    for (path, (before, target)) in links {
        check_time()?;
        let (_, parent) = safe_root(
            path.parent()
                .ok_or_else(|| unknown("link parent missing"))?,
        )
        .map_err(unknown)?;
        let name = path
            .file_name()
            .ok_or_else(|| unknown("link name missing"))?;
        if stat_identity(&stat_child(&parent, name).map_err(unknown)?) != *before
            || read_link(&parent, name)? != *target
        {
            return Err(unknown("symlink changed before closure completed"));
        }
    }
    for (path, before) in referents {
        check_time()?;
        if path == Path::new("/") {
            return Err(unknown("whole-volume directory referent"));
        }
        let (_, parent) = safe_root(
            path.parent()
                .ok_or_else(|| unknown("referent parent missing"))?,
        )
        .map_err(unknown)?;
        let name = path
            .file_name()
            .ok_or_else(|| unknown("referent name missing"))?;
        if stat_identity(&stat_child(&parent, name).map_err(unknown)?) != *before {
            return Err(unknown("referent changed before closure completed"));
        }
    }
    Ok(())
}

pub(super) fn inspect(
    kind: &str,
    cache_root: &Path,
    authorization_scope: Option<&Path>,
) -> Result<Snapshot, String> {
    inspect_with_limits(kind, cache_root, authorization_scope, Limits::default())
}
fn inspect_with_limits(
    kind: &str,
    cache_root: &Path,
    authorization_scope: Option<&Path>,
    limits: Limits,
) -> Result<Snapshot, String> {
    inspect_with_limits_and_hook(kind, cache_root, authorization_scope, limits, || {})
}
fn inspect_with_limits_and_hook(
    kind: &str,
    cache_root: &Path,
    authorization_scope: Option<&Path>,
    limits: Limits,
    before_traversal: impl FnOnce(),
) -> Result<Snapshot, String> {
    let started = Instant::now();
    let (cache_root, handle) = safe_root(cache_root).map_err(unknown)?;
    let cache_identity = metadata_identity(&handle)?;
    let (owner, manifest_name) = match kind {
        "rust_target" => {
            let target = cache_root
                .ancestors()
                .find(|p| p.file_name() == Some(OsStr::new("target")))
                .ok_or_else(|| unknown("Cargo target owner unavailable"))?;
            (
                target
                    .parent()
                    .ok_or_else(|| unknown("Cargo owner parent unavailable"))?
                    .to_path_buf(),
                OsString::from("Cargo.toml"),
            )
        }
        "go_build_cache" => {
            let cache = cache_root
                .ancestors()
                .find(|p| p.file_name() == Some(OsStr::new("go-build")))
                .ok_or_else(|| unknown("Go cache provenance unavailable"))?;
            let mut found = None;
            for (depth, candidate) in cache.ancestors().skip(1).enumerate() {
                if started.elapsed() > limits.elapsed {
                    return Err(unknown("time budget exceeded"));
                }
                if depth >= 64 {
                    return Err(unknown("Go owner ancestry budget exceeded"));
                }
                let (_, directory) = safe_root(candidate).map_err(unknown)?;
                for name in ["go.work", "go.mod"] {
                    let marker = super::cstr(OsStr::new(name)).map_err(unknown)?;
                    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                    let rc = unsafe {
                        libc::fstatat(
                            directory.as_raw_fd(),
                            marker.as_ptr(),
                            stat.as_mut_ptr(),
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    };
                    if rc != 0 {
                        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
                            return Err(unknown("Go owner manifest inspection unavailable"));
                        }
                        continue;
                    }
                    let stat = unsafe { stat.assume_init() };
                    if !ordinary(&stat, libc::S_IFREG) || stat_placeholder(&stat) {
                        return Err(unknown("Go owner manifest is not an ordinary local file"));
                    }
                    found = Some((candidate.to_path_buf(), OsString::from(name)));
                    break;
                }
                if found.is_some() {
                    break;
                }
            }
            found.ok_or_else(|| {
                unknown("no validated Go consumer owner; global cache provenance unknown")
            })?
        }
        _ => return Err(unknown("unsupported cache kind")),
    };
    let (_, owner_handle) = safe_root(&owner).map_err(unknown)?;
    let owner_identity = metadata_identity(&owner_handle)?;
    if owner_identity.dev != cache_identity.dev {
        return Err(unknown("cross-volume consumer owner"));
    }
    let manifest = stat_child(&owner_handle, &manifest_name).map_err(unknown)?;
    if !ordinary(&manifest, libc::S_IFREG) || stat_placeholder(&manifest) {
        return Err(unknown("consumer manifest is not an ordinary local file"));
    }
    let mut inspector = Inspector {
        started,
        limits,
        device: cache_identity.dev,
        entries: 0,
        path_bytes: 0,
        roots: Vec::new(),
        frontier: VecDeque::new(),
        visited: HashSet::new(),
        directories: HashMap::new(),
        initial_roots: HashMap::new(),
        links: HashMap::new(),
        files: Vec::new(),
        references: HashSet::new(),
        referents: HashMap::new(),
        directory_referents: HashSet::new(),
    };
    inspector
        .initial_roots
        .insert(cache_root.clone(), cache_identity.clone());
    inspector
        .initial_roots
        .insert(owner.clone(), owner_identity.clone());
    // Include the ordinary provenance marker in the sealed file anchors.
    inspector
        .referents
        .insert(owner.join(&manifest_name), stat_identity(&manifest));
    let scope = match authorization_scope {
        Some(scope) => {
            let (scope, scope_handle) = safe_root(scope).map_err(unknown)?;
            if !cache_root.starts_with(&scope) {
                return Err(unknown(
                    "authorization scope does not contain selected cache",
                ));
            }
            let scope_identity = metadata_identity(&scope_handle)?;
            if scope_identity.dev != cache_identity.dev {
                return Err(unknown("cross-volume authorization scope"));
            }
            inspector
                .initial_roots
                .insert(scope.clone(), scope_identity);
            Some(scope)
        }
        None => None,
    };
    // A broader authorized root subsumes the owner; a cache-only authorization
    // never narrows away consumers from the validated owner's boundary.
    if let Some(scope) = scope.filter(|p| owner.starts_with(p)) {
        inspector.root(scope)?;
    }
    inspector.root(owner.clone())?;
    let initial_roots = inspector.roots.len();
    before_traversal();
    inspector.traverse()?;
    inspector.revalidate()?;
    if metadata_identity(&owner_handle)? != owner_identity
        || stat_identity(&stat_child(&owner_handle, &manifest_name).map_err(unknown)?)
            != stat_identity(&manifest)
        || metadata_identity(&handle)? != cache_identity
    {
        return Err(unknown("owner/cache provenance changed during inspection"));
    }
    // A directory may be observed through two physical names (for example a
    // same-device bind alias). Project its identity back to all scanned names
    // before gathering descendant identities; a path-only prefix is insufficient.
    let equivalent_directories: Vec<_> = inspector
        .directories
        .iter()
        .filter(|(_, identity)| inspector.references.contains(&(identity.dev, identity.ino)))
        .map(|(path, _)| path.clone())
        .collect();
    for path in equivalent_directories {
        inspector.check()?;
        if !inspector
            .directory_referents
            .iter()
            .any(|p| path.starts_with(p))
        {
            inspector
                .directory_referents
                .retain(|p| !p.starts_with(&path));
            inspector.directory_referents.insert(path);
        }
    }
    for (path, identity) in &inspector.files {
        inspector.check()?;
        if inspector
            .directory_referents
            .iter()
            .any(|p| path.starts_with(p))
        {
            inspector.references.insert(*identity);
        }
    }
    let observation = Observation {
        state: "scoped_closure_inspected", complete: true, scoped_roots: inspector.roots.len(),
        root_samples: inspector.roots.iter().take(MAX_SAMPLES).map(|p| p.to_string_lossy().into_owned()).collect(),
        external_directory_roots: inspector.roots.len() - initial_roots,
        inspected_entries: inspector.entries, inspected_directories: inspector.directories.len(), inspected_symlinks: inspector.links.len(),
        protected_identities: inspector.references.len(), protected_directory_referents: inspector.directory_referents.len(),
        max_entries: limits.entries, max_directories: limits.directories, max_roots: limits.roots, max_path_bytes: limits.path_bytes,
        max_link_depth: MAX_LINK_DEPTH, max_directory_depth: MAX_DIRECTORY_DEPTH, max_seconds: limits.elapsed.as_secs(),
        coverage: "validated_consumer_owner_and_broader_authorized_scope_plus_directory_alias_frontier; scoped_metadata_observation_only; no_whole_volume_or_atomic_absence_guarantee",
    };
    Ok(Snapshot {
        identities: inspector.references,
        directory_referents: inspector.directory_referents.into_iter().collect(),
        observation,
        directories: inspector.directories,
        links: inspector.links,
        referents: inspector.referents,
        initial_roots: inspector.initial_roots,
        revalidation_budget: limits.elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct Fixture {
        root: PathBuf,
        owner: PathBuf,
        cache: PathBuf,
        artifact: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!(
                    "cublet-incoming-refs-{}",
                    super::super::id().unwrap()
                ));
            let owner = root.join("owner");
            let cache = owner.join("target");
            let artifact = cache.join("debug/deps/Rust.o");
            fs::create_dir_all(artifact.parent().unwrap()).unwrap();
            fs::write(
                owner.join("Cargo.toml"),
                b"[package]\nname='synthetic'\nversion='0.0.0'\n",
            )
            .unwrap();
            fs::write(&artifact, b"synthetic eligible artifact").unwrap();
            Self {
                root,
                owner,
                cache,
                artifact,
            }
        }
        fn inspect(&self) -> Result<Snapshot, String> {
            inspect("rust_target", &self.cache, None)
        }
        fn identity(&self) -> Identity {
            Identity::metadata(&fs::symlink_metadata(&self.artifact).unwrap())
        }
        fn protected(&self, snapshot: &Snapshot) -> bool {
            snapshot.protects(&self.artifact, &self.identity())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn static_incoming_link_outside_cache_protects_and_unrelated_control_is_eligible() {
        let fixture = Fixture::new();
        assert!(!fixture.protected(&fixture.inspect().unwrap()));
        let ignored = fixture.owner.join("ignored-consumers");
        fs::create_dir(&ignored).unwrap();
        fs::write(fixture.owner.join(".gitignore"), b"ignored-consumers/\n").unwrap();
        symlink(&fixture.artifact, ignored.join("input")).unwrap();
        let snapshot = fixture.inspect().unwrap();
        assert!(fixture.protected(&snapshot));
        assert!(snapshot.observation().complete);
        assert_eq!(snapshot.observation().scoped_roots, 1);
        assert_eq!(snapshot.observation().inspected_symlinks, 1);
    }

    #[test]
    fn fresh_snapshot_catches_post_preview_creation_and_retargeting() {
        let fixture = Fixture::new();
        assert!(!fixture.protected(&fixture.inspect().unwrap()));
        let consumer = fixture.owner.join("consumer");
        symlink(&fixture.artifact, &consumer).unwrap();
        assert!(fixture.protected(&fixture.inspect().unwrap()));
        fs::remove_file(&consumer).unwrap();
        let other = fixture.owner.join("ordinary-input");
        fs::write(&other, b"other").unwrap();
        symlink(&other, &consumer).unwrap();
        assert!(!fixture.protected(&fixture.inspect().unwrap()));
        fs::remove_file(&consumer).unwrap();
        symlink(&fixture.artifact, &consumer).unwrap();
        assert!(fixture.protected(&fixture.inspect().unwrap()));
    }

    #[test]
    fn directory_referent_covers_narrow_cache_scope_and_hardlinked_identity() {
        let fixture = Fixture::new();
        symlink(
            fixture.cache.join("debug"),
            fixture.owner.join("directory-consumer"),
        )
        .unwrap();
        let snapshot = inspect("rust_target", fixture.artifact.parent().unwrap(), None).unwrap();
        assert!(fixture.protected(&snapshot));
        let another_name = fixture.owner.join("another-hardlink-name");
        fs::hard_link(&fixture.artifact, &another_name).unwrap();
        let identity = Identity::metadata(&fs::symlink_metadata(&another_name).unwrap());
        assert!(snapshot.protects(&another_name, &identity));
    }

    #[test]
    fn incoming_link_to_another_hardlink_name_protects_artifact_identity() {
        let fixture = Fixture::new();
        let alias = fixture.owner.join("ordinary-hardlink");
        fs::hard_link(&fixture.artifact, &alias).unwrap();
        symlink(&alias, fixture.owner.join("consumer")).unwrap();
        assert!(fixture.protected(&fixture.inspect().unwrap()));
    }

    #[test]
    fn link_chains_and_parent_after_directory_alias_are_resolved_in_order() {
        let fixture = Fixture::new();
        let external = fixture.root.join("external/deep");
        fs::create_dir_all(&external).unwrap();
        symlink(&external, fixture.owner.join("directory-alias")).unwrap();
        symlink(&fixture.artifact, fixture.root.join("external/input")).unwrap();
        symlink("directory-alias/../input", fixture.owner.join("chain-a")).unwrap();
        symlink("chain-a", fixture.owner.join("chain-b")).unwrap();
        let snapshot = fixture.inspect().unwrap();
        assert!(fixture.protected(&snapshot));
        assert!(snapshot.observation().external_directory_roots >= 1);
    }

    #[test]
    fn directory_alias_frontier_catches_new_external_incoming_link() {
        let fixture = Fixture::new();
        let external = fixture.root.join("external");
        fs::create_dir(&external).unwrap();
        symlink(&external, fixture.owner.join("consumer-directory")).unwrap();
        assert!(!fixture.protected(&fixture.inspect().unwrap()));
        symlink(&fixture.artifact, external.join("new-input")).unwrap();
        assert!(fixture.protected(&fixture.inspect().unwrap()));
    }

    #[test]
    fn broader_explicit_scope_covers_sibling_consumer_and_cache_scope_does_not_narrow_owner() {
        let fixture = Fixture::new();
        symlink(&fixture.artifact, fixture.root.join("sibling-input")).unwrap();
        assert!(!fixture.protected(&fixture.inspect().unwrap()));
        let broad = inspect("rust_target", &fixture.cache, Some(&fixture.root)).unwrap();
        assert!(fixture.protected(&broad));
        symlink(&fixture.artifact, fixture.owner.join("owner-input")).unwrap();
        let narrow = inspect("rust_target", &fixture.cache, Some(&fixture.cache)).unwrap();
        assert!(fixture.protected(&narrow));
    }

    #[test]
    fn non_utf8_paths_are_byte_preserving_and_observation_serializes() {
        let fixture = Fixture::new();
        let target = fixture
            .artifact
            .parent()
            .unwrap()
            .join(OsString::from_vec(b"Rust-\xfe.o".to_vec()));
        assert!(target.as_os_str().as_bytes().ends_with(b"Rust-\xfe.o"));
        let creation = fs::write(&target, b"bytes");
        let link = fixture.owner.join("raw-byte-consumer");
        // Symlink contents are opaque bytes even when APFS rejects that byte
        // sequence as a filename. Observe those exact bytes through readlinkat.
        symlink(&target, &link).unwrap();
        let (_, parent) = safe_root(&fixture.owner).unwrap();
        assert_eq!(
            read_link(&parent, OsStr::new("raw-byte-consumer"))
                .unwrap()
                .as_os_str()
                .as_bytes(),
            target.as_os_str().as_bytes()
        );
        match creation {
            Ok(()) => {
                let snapshot = fixture.inspect().unwrap();
                assert!(snapshot.protects(
                    &target,
                    &Identity::metadata(&fs::metadata(&target).unwrap())
                ));
                serde_json::to_value(snapshot.observation()).unwrap();
            }
            Err(error) => {
                // An uncreatable byte path is unknown, never evidence that its
                // consumer is absent or that remaining artifacts can be deleted.
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::EILSEQ),
                    "unexpected filename rejection: {error}"
                );
                assert!(fixture.inspect().is_err());
                fs::remove_file(&link).unwrap();
                let snapshot = fixture.inspect().unwrap();
                assert!(!fixture.protected(&snapshot));
                serde_json::to_value(snapshot.observation()).unwrap();
            }
        }
    }

    #[test]
    fn sealed_snapshot_rejects_new_link_and_retarget_before_mutation() {
        let fixture = Fixture::new();
        let snapshot = fixture.inspect().unwrap();
        snapshot.revalidate().unwrap();
        let link = fixture.owner.join("new-consumer");
        symlink(&fixture.artifact, &link).unwrap();
        assert!(snapshot.revalidate().is_err());
        let snapshot = fixture.inspect().unwrap();
        snapshot.revalidate().unwrap();
        fs::remove_file(&link).unwrap();
        symlink(fixture.owner.join("Cargo.toml"), &link).unwrap();
        assert!(snapshot.revalidate().is_err());
    }

    #[test]
    fn sealed_snapshot_rejects_new_external_alias_consumer_and_rebound_original_root() {
        let fixture = Fixture::new();
        let external = fixture.root.join("external");
        fs::create_dir(&external).unwrap();
        symlink(&external, fixture.owner.join("alias-directory")).unwrap();
        let snapshot = fixture.inspect().unwrap();
        snapshot.revalidate().unwrap();
        symlink(&fixture.artifact, external.join("new-consumer")).unwrap();
        assert!(snapshot.revalidate().is_err());
        let snapshot = fixture.inspect().unwrap();
        fs::rename(&fixture.owner, fixture.root.join("held-old-owner")).unwrap();
        fs::create_dir_all(fixture.cache.join("debug/deps")).unwrap();
        fs::write(fixture.owner.join("Cargo.toml"), b"replacement").unwrap();
        assert!(snapshot.revalidate().is_err());
    }

    #[test]
    fn original_ancestor_swap_between_capture_and_traversal_is_unknown() {
        let fixture = Fixture::new();
        for broader_authorization in [false, true] {
            let outer = fixture.root.join(format!("outer-{broader_authorization}"));
            let owner = outer.join("project");
            let cache = owner.join("target");
            fs::create_dir_all(cache.join("debug/deps")).unwrap();
            fs::write(owner.join("Cargo.toml"), b"original manifest").unwrap();
            fs::write(cache.join("debug/deps/Rust.o"), b"original artifact").unwrap();
            let result = inspect_with_limits_and_hook(
                "rust_target",
                &cache,
                broader_authorization.then_some(outer.as_path()),
                Limits::default(),
                || {
                    fs::rename(
                        &outer,
                        fixture
                            .root
                            .join(format!("old-outer-{broader_authorization}")),
                    )
                    .unwrap();
                    fs::create_dir_all(cache.join("debug/deps")).unwrap();
                    fs::write(owner.join("Cargo.toml"), b"replacement manifest").unwrap();
                    fs::write(cache.join("debug/deps/Rust.o"), b"replacement artifact").unwrap();
                },
            );
            assert!(result
                .err()
                .unwrap()
                .contains("original consumer/cache/authorization root rebound"));
        }
    }

    #[test]
    fn cycles_dangling_links_and_budget_exhaustion_are_unknown_not_empty() {
        let fixture = Fixture::new();
        let a = fixture.owner.join("cycle-a");
        let b = fixture.owner.join("cycle-b");
        symlink("cycle-b", &a).unwrap();
        symlink("cycle-a", &b).unwrap();
        assert!(fixture.inspect().err().unwrap().contains("cycle"));
        fs::remove_file(&a).unwrap();
        fs::remove_file(&b).unwrap();
        symlink("missing-input", &a).unwrap();
        assert!(fixture.inspect().is_err());
        fs::remove_file(&a).unwrap();
        for limits in [
            Limits {
                entries: 1,
                ..Limits::default()
            },
            Limits {
                directories: 1,
                ..Limits::default()
            },
            Limits {
                path_bytes: 1,
                ..Limits::default()
            },
            Limits {
                roots: 0,
                ..Limits::default()
            },
            Limits {
                elapsed: Duration::ZERO,
                ..Limits::default()
            },
        ] {
            assert!(inspect_with_limits("rust_target", &fixture.cache, None, limits).is_err());
        }
    }

    #[test]
    fn unreadable_subtree_and_cloud_named_alias_retain_whole_candidate() {
        let fixture = Fixture::new();
        let unreadable = fixture.owner.join("unreadable");
        fs::create_dir(&unreadable).unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(fixture.inspect().is_err());
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
        let cloud = fixture.root.join("Library/CloudStorage/synthetic");
        fs::create_dir_all(&cloud).unwrap();
        symlink(cloud, fixture.owner.join("cloud-alias")).unwrap();
        assert!(fixture.inspect().is_err());
    }

    #[test]
    fn go_requires_physical_consumer_manifest_and_has_positive_local_case() {
        let fixture = Fixture::new();
        let cache = fixture.root.join("go-project/go-build");
        fs::create_dir_all(cache.join("ab")).unwrap();
        assert!(inspect("go_build_cache", &cache, Some(&fixture.root)).is_err());
        fs::write(
            cache.parent().unwrap().join("go.mod"),
            b"module synthetic\ngo 1.20\n",
        )
        .unwrap();
        let artifact = cache.join("ab/archive-d");
        fs::write(&artifact, b"synthetic Go archive").unwrap();
        symlink(&artifact, cache.parent().unwrap().join("consumer")).unwrap();
        let snapshot = inspect("go_build_cache", &cache, Some(cache.parent().unwrap())).unwrap();
        assert!(snapshot.protects(
            &artifact,
            &Identity::metadata(&fs::metadata(&artifact).unwrap())
        ));
    }

    #[test]
    fn observable_directory_and_symlink_churn_fail_final_revalidation() {
        let fixture = Fixture::new();
        let identity = fixture.identity();
        let mut inspector = Inspector {
            started: Instant::now(),
            limits: Limits::default(),
            device: identity.dev,
            entries: 0,
            path_bytes: 0,
            roots: Vec::new(),
            frontier: VecDeque::new(),
            visited: HashSet::new(),
            directories: HashMap::new(),
            initial_roots: HashMap::new(),
            links: HashMap::new(),
            files: Vec::new(),
            references: HashSet::new(),
            referents: HashMap::new(),
            directory_referents: HashSet::new(),
        };
        inspector.root(fixture.owner.clone()).unwrap();
        inspector.traverse().unwrap();
        symlink(&fixture.artifact, fixture.owner.join("new-after-traversal")).unwrap();
        assert!(inspector.revalidate().is_err());
        fs::remove_file(fixture.owner.join("new-after-traversal")).unwrap();
        let link = fixture.owner.join("retarget-me");
        symlink(&fixture.artifact, &link).unwrap();
        let (_, parent) = safe_root(&fixture.owner).unwrap();
        let before = stat_child(&parent, OsStr::new("retarget-me")).unwrap();
        inspector.links.clear();
        inspector.directories.clear();
        inspector
            .link(&link, &parent, OsStr::new("retarget-me"), &before)
            .unwrap();
        fs::remove_file(&link).unwrap();
        symlink(fixture.owner.join("Cargo.toml"), &link).unwrap();
        assert!(inspector.revalidate().is_err());
    }
}
