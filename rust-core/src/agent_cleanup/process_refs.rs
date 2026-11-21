//! Byte-preserving, bounded process references to a validated build cache.
//!
//! `ps` supplies PID enumeration and conservative hints only. It is never parsed
//! as argv. Native byte argument-token supersets/cwd snapshots are bound to PID
//! birth before and after the reads. This is an observation, not exclusion of future process/argument races.
use super::{GuardDisposition, GuardFailure};
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_PS_BYTES: usize = 16 * 1024 * 1024;
const MAX_PIDS: usize = 32_768;
const MAX_ARG_BYTES: usize = 8 * 1024 * 1024;
const MAX_ARGS: usize = 32_768;
const INSPECTION_TIME: Duration = Duration::from_secs(3);
const COVERAGE: &str = "bounded ps-enumerated effective/real/saved current-user PID/birth and repeated vnode-identity-bound cwd with byte argument-token supersets including kernel executable; leading empty argv is indistinguishable from kernel padding and may add environment tokens; ordinary path operands only; ENOTDIR/ENAMETOOLONG tokens outside owner without cache/owner hints or resolved cache prefixes are excluded as non-path tokens; all-three-UID foreign processes without relevant hints are outside byte-argument provenance; opaque languages and future PID/UID/process changes are outside this observation; existing global command/open-path guards remain required";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Observation {
    pub enumerated_pids: u64,
    pub inspected_current_user: u64,
    pub outside_user_provenance: u64,
    pub disappeared_pids: u64,
    pub retried_pids: u64,
    pub path_operands: u64,
    pub non_path_tokens_excluded: u64,
    pub coverage_boundary: &'static str,
}
impl Default for Observation {
    fn default() -> Self {
        Self {
            enumerated_pids: 0,
            inspected_current_user: 0,
            outside_user_provenance: 0,
            disappeared_pids: 0,
            retried_pids: 0,
            path_operands: 0,
            non_path_tokens_excluded: 0,
            coverage_boundary: COVERAGE,
        }
    }
}
fn unavailable(message: &str) -> GuardFailure {
    GuardFailure {
        disposition: GuardDisposition::InspectionUnavailable,
        message: format!("Process reference inspection unavailable; retained: {message}"),
    }
}
fn pid_unavailable(pid: i32, stage: &str) -> GuardFailure {
    // PID and fixed stage are bounded diagnostics; no command/argument/path data.
    unavailable(&format!("pid={pid} stage={stage}"))
}
fn active(message: &str) -> GuardFailure {
    GuardFailure {
        disposition: GuardDisposition::ObservedActive,
        message: format!("Live process references selected build cache; retained: {message}"),
    }
}

struct Provenance {
    root: PathBuf,
    target: PathBuf,
    owner: PathBuf,
    cargo: bool,
}
impl Provenance {
    fn load(root: &Path) -> Result<Self, GuardFailure> {
        let root =
            fs::canonicalize(root).map_err(|_| unavailable("cache identity cannot be resolved"))?;
        let target = root
            .ancestors()
            .find(|p| {
                matches!(
                    p.file_name().and_then(OsStr::to_str),
                    Some("target" | "go-build")
                )
            })
            .ok_or_else(|| unavailable("no recognizable build cache ancestor"))?
            .to_path_buf();
        let cargo = target.file_name() == Some(OsStr::new("target"));
        let parent = target
            .parent()
            .ok_or_else(|| unavailable("cache owner has no parent"))?;
        let ordinary_manifest = |directory: &Path, names: &[&str]| -> Result<bool, GuardFailure> {
            let mut found = false;
            for name in names {
                match fs::symlink_metadata(directory.join(name)) {
                    Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                        found = true
                    }
                    Ok(_) => return Err(unavailable("cache owner manifest is not ordinary")),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(unavailable("cache owner manifest inspection failed")),
                }
            }
            Ok(found)
        };
        let owner = if cargo {
            if !ordinary_manifest(parent, &["Cargo.toml"])? {
                return Err(unavailable("Cargo target owner manifest unavailable"));
            }
            parent.to_path_buf()
        } else {
            let mut owner = None;
            for directory in parent.ancestors().take(16) {
                if ordinary_manifest(directory, &["go.mod", "go.work"])? {
                    owner = Some(directory.to_path_buf());
                    break;
                }
            }
            owner.ok_or_else(|| {
                unavailable("Go cache has no validated ordinary module/workspace ancestor")
            })?
        };
        Ok(Self {
            root,
            target,
            owner,
            cargo,
        })
    }
    fn relative_cache_hint(&self, bytes: &[u8]) -> bool {
        contains(bytes, if self.cargo { b"target/" } else { b"go-build/" })
    }
    fn references(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
            || (self.root.starts_with(path) && path.starts_with(&self.target))
    }
    fn hint(&self, bytes: &[u8]) -> bool {
        contains(bytes, self.root.as_os_str().as_bytes())
            || contains(bytes, self.owner.as_os_str().as_bytes())
    }
}
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Birth {
    sec: u64,
    usec: u64,
    current_user: bool,
}
#[derive(Clone, Copy, Debug)]
enum InspectError {
    Gone,
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkingDirectory {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
fn metadata_device(metadata: &fs::Metadata) -> u64 {
    // Darwin's proc vnode device is u32, while stat's signed dev_t can be
    // sign-extended by MetadataExt. Compare the same native 32 device bits.
    #[cfg(target_os = "macos")]
    {
        metadata.dev() as u32 as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        metadata.dev()
    }
}
fn bind_working_directory(
    path: &Path,
    dev: u64,
    ino: u64,
) -> Result<WorkingDirectory, InspectError> {
    if !path.is_absolute() || ino == 0 {
        return Err(InspectError::Unavailable);
    }
    let path = fs::canonicalize(path).map_err(|_| InspectError::Unavailable)?;
    let metadata = fs::metadata(&path).map_err(|_| InspectError::Unavailable)?;
    if !metadata.is_dir() || metadata_device(&metadata) != dev || metadata.ino() != ino {
        return Err(InspectError::Unavailable);
    }
    Ok(WorkingDirectory { path, dev, ino })
}
trait Inspector {
    fn birth(&mut self, pid: i32) -> Result<Birth, InspectError>;
    fn cwd(&mut self, pid: i32) -> Result<WorkingDirectory, InspectError>;
    fn argv(&mut self, pid: i32) -> Result<Vec<OsString>, InspectError>;
}
struct PidHint {
    pid: i32,
    current_user: bool,
    relevant: bool,
}
fn enumerate(ps: &[u8], provenance: &Provenance) -> Result<Vec<PidHint>, GuardFailure> {
    if ps.len() > MAX_PS_BYTES {
        return Err(unavailable("PID enumeration byte limit"));
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for line in ps.split(|b| *b == b'\n') {
        let line = line.trim_ascii_start();
        if line.is_empty() {
            continue;
        }
        let mut fields = line
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let pid = fields
            .next()
            .filter(|field| field.iter().all(u8::is_ascii_digit))
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<i32>().ok())
            .filter(|pid| *pid >= 0)
            .ok_or_else(|| unavailable("invalid PID enumeration"))?;
        let mut uids = [0u32; 3];
        for uid in &mut uids {
            let raw = fields
                .next()
                .ok_or_else(|| unavailable("missing structured process UID"))?;
            if !raw.iter().all(u8::is_ascii_digit) {
                return Err(unavailable("invalid structured process UID"));
            }
            *uid = std::str::from_utf8(raw)
                .ok()
                .and_then(|field| field.parse::<u32>().ok())
                .ok_or_else(|| unavailable("invalid structured process UID"))?;
        }
        if fields.next().is_none() {
            return Err(unavailable("missing structured process command"));
        }
        let uid = unsafe { libc::getuid() };
        let euid = unsafe { libc::geteuid() };
        let current_user = uids
            .iter()
            .any(|observed| *observed == uid || *observed == euid);
        if !seen.insert(pid) {
            return Err(unavailable("duplicate PID enumeration"));
        }
        if result.len() >= MAX_PIDS {
            return Err(unavailable("PID enumeration count limit"));
        }
        result.push(PidHint {
            pid,
            current_user,
            relevant: provenance.hint(line),
        });
    }
    if result.is_empty() {
        return Err(unavailable("empty PID enumeration"));
    }
    Ok(result)
}

pub(super) fn check(root: &Path, ps_output: &[u8]) -> Result<Observation, GuardFailure> {
    let provenance = Provenance::load(root)?;
    #[cfg(target_os = "macos")]
    {
        let mut inspector = Native::new()?;
        inspect(&provenance, ps_output, &mut inspector)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (provenance, ps_output);
        Err(unavailable(
            "native byte argv/cwd inspection requires macOS",
        ))
    }
}

fn inspect(
    provenance: &Provenance,
    ps_output: &[u8],
    inspector: &mut impl Inspector,
) -> Result<Observation, GuardFailure> {
    let started = Instant::now();
    let pids = enumerate(ps_output, provenance)?;
    let mut observation = Observation {
        enumerated_pids: pids.len() as u64,
        ..Observation::default()
    };
    for hint in pids {
        if started.elapsed() > INSPECTION_TIME {
            return Err(unavailable("native snapshot time limit"));
        }
        if hint.pid == std::process::id() as i32 {
            continue;
        }
        // Trusted structured PS observations distinguish a declared foreign-UID
        // coverage boundary from a native permission failure. No failed syscall
        // is used to infer ownership. Any one current real/effective/saved UID
        // requires the strict native snapshots below.
        if !hint.current_user {
            if hint.relevant {
                return Err(pid_unavailable(hint.pid, "foreign_uid_owner_hint"));
            }
            observation.outside_user_provenance += 1;
            continue;
        }
        let mut complete = false;
        for attempt in 0..2 {
            let before = match inspector.birth(hint.pid) {
                Ok(birth) => birth,
                Err(InspectError::Gone) => {
                    observation.disappeared_pids += 1;
                    complete = true;
                    break;
                }
                Err(InspectError::Unavailable) => {
                    // Ownership is unknown, so an unreadable PID cannot silently
                    // become an unrelated-user exception.
                    return Err(pid_unavailable(hint.pid, "ownership_birth"));
                }
            };
            if !before.current_user {
                // Observed current-user provenance changed between PS and native
                // inspection. Retain rather than reclassifying an unreadable or
                // reused PID as foreign.
                return Err(pid_unavailable(hint.pid, "ownership_changed_since_ps"));
            }
            let cwd = inspector.cwd(hint.pid);
            let argv = inspector.argv(hint.pid);
            let cwd_after = inspector.cwd(hint.pid);
            let after = inspector.birth(hint.pid);
            match after {
                Err(InspectError::Gone) => {
                    observation.disappeared_pids += 1;
                    complete = true;
                    break;
                }
                Ok(after) if after != before => {
                    if attempt == 0 {
                        observation.retried_pids += 1;
                        continue;
                    }
                    return Err(pid_unavailable(hint.pid, "identity_churn"));
                }
                Err(InspectError::Unavailable) => {
                    return Err(pid_unavailable(hint.pid, "birth_recheck"));
                }
                _ => {}
            }
            let cwd = cwd.map_err(|_| pid_unavailable(hint.pid, "cwd_identity"))?;
            let cwd_after = cwd_after.map_err(|_| pid_unavailable(hint.pid, "cwd_recheck"))?;
            if cwd_after != cwd {
                if attempt == 0 {
                    observation.retried_pids += 1;
                    continue;
                }
                return Err(pid_unavailable(hint.pid, "cwd_churn"));
            }
            let argv = argv.map_err(|_| pid_unavailable(hint.pid, "byte_argument_tokens"))?;
            if !cwd.path.is_absolute() || argv.is_empty() {
                return Err(pid_unavailable(hint.pid, "incomplete_snapshot"));
            }
            evaluate(provenance, &cwd.path, &argv, &mut observation, started).map_err(
                |mut failure| {
                    failure.message = format!("pid={} {}", hint.pid, failure.message);
                    failure
                },
            )?;
            observation.inspected_current_user += 1;
            complete = true;
            break;
        }
        if !complete || started.elapsed() > INSPECTION_TIME {
            return Err(unavailable("native process snapshot incomplete"));
        }
    }
    Ok(observation)
}

fn evaluate(
    provenance: &Provenance,
    cwd: &Path,
    argv: &[OsString],
    observation: &mut Observation,
    started: Instant,
) -> Result<(), GuardFailure> {
    let cwd = fs::canonicalize(cwd)
        .map_err(|_| unavailable("working directory identity cannot be resolved"))?;
    if provenance.references(&cwd) {
        return Err(active("working directory is inside cache"));
    }
    let project_cwd = cwd.starts_with(&provenance.owner);
    let program = Path::new(&argv[0])
        .file_name()
        .unwrap_or_default()
        .as_bytes();
    if project_cwd
        && ((provenance.cargo && matches!(program, b"cargo" | b"rustc" | b"rustdoc"))
            || (!provenance.cargo && program == b"go"))
    {
        return Err(active("compiler/build process in owning project"));
    }
    let mut bytes = 0usize;
    if argv.len() > MAX_ARGS + 1 {
        return Err(unavailable("argv count limit"));
    }
    for (operand_index, arg) in argv.iter().enumerate() {
        if started.elapsed() > INSPECTION_TIME {
            return Err(unavailable("path operand inspection time limit"));
        }
        let raw = arg.as_bytes();
        bytes = bytes
            .checked_add(raw.len())
            .ok_or_else(|| unavailable("argv size overflow"))?;
        if bytes > MAX_ARG_BYTES || raw.contains(&0) {
            return Err(unavailable("invalid or oversized byte argv"));
        }
        if raw.is_empty() {
            continue;
        }
        let operand = if raw.starts_with(b"--") {
            match raw.iter().position(|b| *b == b'=') {
                Some(equal) => &raw[equal + 1..],
                None => continue,
            }
        } else if raw.starts_with(b"-") {
            if (project_cwd && provenance.relative_cache_hint(raw)) || provenance.hint(raw) {
                return Err(unavailable(
                    "opaque relevant option requires interpretation",
                ));
            }
            continue;
        } else {
            raw
        };
        if operand.is_empty() {
            continue;
        }
        let path = Path::new(OsStr::from_bytes(operand));
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let resolved = match resolve_operand(&joined, started, provenance) {
            Ok(resolved) => resolved,
            Err(error) if exclude_non_path_token(provenance, project_cwd, raw, &error) => {
                observation.non_path_tokens_excluded += 1;
                continue;
            }
            Err(error) => {
                return Err(unavailable(&format!(
                    "operandIndex={operand_index} operandBytes={} stage={} errno={}",
                    operand.len(),
                    error.stage,
                    error.errno.unwrap_or(0)
                )))
            }
        };
        observation.path_operands += 1;
        if provenance.references(&resolved) {
            return Err(active("byte argv path operand resolves into cache"));
        }
        // Scripts/response/embedded option languages must not gain a clean
        // verdict by treating the entire expression as one nonexistent path.
        if !resolved.starts_with(&provenance.target)
            && (contains(operand, provenance.target.as_os_str().as_bytes())
                || (project_cwd && provenance.relative_cache_hint(operand)))
        {
            return Err(unavailable(
                "opaque relevant argument requires interpretation",
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct OperandFailure {
    stage: &'static str,
    errno: Option<i32>,
    cache_prefix_seen: bool,
}
impl OperandFailure {
    fn io(stage: &'static str, error: std::io::Error, cache_prefix_seen: bool) -> Self {
        Self {
            stage,
            errno: error.raw_os_error(),
            cache_prefix_seen,
        }
    }
    fn stage(stage: &'static str, cache_prefix_seen: bool) -> Self {
        Self {
            stage,
            errno: None,
            cache_prefix_seen,
        }
    }
}
/// Only two OS errors prove the exact ordinary pathname cannot currently
/// resolve. Opaque interpretation is outside this narrow observation only when
/// neither provenance nor any physical cache prefix makes the token relevant.
fn exclude_non_path_token(
    provenance: &Provenance,
    project_cwd: bool,
    raw: &[u8],
    error: &OperandFailure,
) -> bool {
    matches!(error.errno, Some(libc::ENOTDIR | libc::ENAMETOOLONG))
        && !project_cwd
        && !error.cache_prefix_seen
        && !provenance.hint(raw)
        && !contains(raw, provenance.target.as_os_str().as_bytes())
        && !provenance.relative_cache_hint(raw)
}
/// Resolve existing prefixes before consuming `..`: lexical normalization alone
/// is wrong for a symlink prefix. Missing tails are retained as possible outputs.
fn resolve_operand(
    path: &Path,
    started: Instant,
    provenance: &Provenance,
) -> Result<PathBuf, OperandFailure> {
    // Native macOS filesystems reject ill-formed UTF-8 components (EILSEQ).
    // Keep these raw bytes for inspection, but never infer a clean path from a
    // failed or lossy conversion. Other platforms may support byte filenames.
    #[cfg(target_os = "macos")]
    if std::str::from_utf8(path.as_os_str().as_bytes()).is_err() {
        return Err(OperandFailure {
            stage: "invalid_utf8",
            errno: Some(libc::EILSEQ),
            cache_prefix_seen: false,
        });
    }
    let mut resolved = PathBuf::new();
    let mut cache_prefix_seen = false;
    for component in path.components() {
        if started.elapsed() > INSPECTION_TIME {
            return Err(OperandFailure::stage(
                "operand_time_limit",
                cache_prefix_seen,
            ));
        }
        match component {
            Component::RootDir => resolved.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                cache_prefix_seen |= provenance.references(&resolved);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = fs::canonicalize(&resolved).map_err(|error| {
                            OperandFailure::io(
                                "operand_prefix_canonicalize",
                                error,
                                cache_prefix_seen,
                            )
                        })?;
                        cache_prefix_seen |= provenance.references(&resolved);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(OperandFailure::io(
                            "operand_component_metadata",
                            error,
                            cache_prefix_seen,
                        ))
                    }
                }
            }
            Component::Prefix(_) => {
                return Err(OperandFailure::stage(
                    "unsupported_operand_prefix",
                    cache_prefix_seen,
                ))
            }
        }
    }
    Ok(resolved)
}

/// KERN_PROCARGS2 carries argc, executable path, NUL padding, argv and possibly
/// environment. Its public format has no explicit argv-start offset: leading
/// empty argv strings are indistinguishable from the padding. Skipping padding
/// then reading argc strings therefore returns a conservative superset of all
/// genuine nonempty arguments (possibly adding environment tokens). Prepend the
/// separate kernel executable for build-tool classification, and inspect EVERY
/// token, including the apparent argv[0]. No raw tokens reach observations.
fn parse_procargs(bytes: &[u8]) -> Result<Vec<OsString>, InspectError> {
    if bytes.len() < std::mem::size_of::<i32>() || bytes.len() > MAX_ARG_BYTES {
        return Err(InspectError::Unavailable);
    }
    let argc = i32::from_ne_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| InspectError::Unavailable)?,
    );
    if argc <= 0 || argc as usize > MAX_ARGS {
        return Err(InspectError::Unavailable);
    }
    let mut cursor = 4usize;
    let executable_end = bytes[cursor..]
        .iter()
        .position(|b| *b == 0)
        .ok_or(InspectError::Unavailable)?
        + cursor;
    if executable_end == cursor {
        return Err(InspectError::Unavailable);
    }
    let executable = OsString::from_vec(bytes[cursor..executable_end].to_vec());
    cursor = executable_end + 1;
    while bytes.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut args = Vec::with_capacity(argc as usize + 1);
    args.push(executable);
    for _ in 0..argc {
        let remaining = bytes.get(cursor..).ok_or(InspectError::Unavailable)?;
        let end = remaining
            .iter()
            .position(|b| *b == 0)
            .ok_or(InspectError::Unavailable)?;
        args.push(OsString::from_vec(remaining[..end].to_vec()));
        cursor = cursor
            .checked_add(end + 1)
            .ok_or(InspectError::Unavailable)?;
    }
    Ok(args)
}

#[cfg(target_os = "macos")]
struct Native {
    argmax: usize,
    uid: libc::uid_t,
    euid: libc::uid_t,
}
#[cfg(target_os = "macos")]
impl Native {
    fn new() -> Result<Self, GuardFailure> {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&argmax);
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0
            || size != std::mem::size_of_val(&argmax)
            || !(256..=MAX_ARG_BYTES).contains(&(argmax as usize))
        {
            return Err(unavailable("bounded KERN_ARGMAX unavailable"));
        }
        Ok(Self {
            argmax: argmax as usize,
            uid: unsafe { libc::getuid() },
            euid: unsafe { libc::geteuid() },
        })
    }
    fn failure(pid: i32) -> InspectError {
        // EINVAL is also used for inaccessible/unsupported proc flavors. Only
        // a separate liveness probe proving ESRCH classifies disappearance.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            InspectError::Gone
        } else {
            InspectError::Unavailable
        }
    }
}
#[cfg(target_os = "macos")]
impl Inspector for Native {
    fn birth(&mut self, pid: i32) -> Result<Birth, InspectError> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info);
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            )
        };
        if got != size as i32 || info.pbi_pid != pid as u32 {
            return Err(Self::failure(pid));
        }
        Ok(Birth {
            sec: info.pbi_start_tvsec,
            usec: info.pbi_start_tvusec,
            current_user: [info.pbi_uid, info.pbi_ruid, info.pbi_svuid]
                .iter()
                .any(|uid| *uid == self.uid || *uid == self.euid),
        })
    }
    fn cwd(&mut self, pid: i32) -> Result<WorkingDirectory, InspectError> {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info);
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size as i32,
            )
        };
        if got != size as i32 {
            return Err(Self::failure(pid));
        }
        let bytes: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|b| *b as u8)
            .collect();
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or(InspectError::Unavailable)?;
        let path = PathBuf::from(OsString::from_vec(bytes[..end].to_vec()));
        if !path.is_absolute() {
            return Err(InspectError::Unavailable);
        }
        bind_working_directory(
            &path,
            info.pvi_cdir.vip_vi.vi_stat.vst_dev as u64,
            info.pvi_cdir.vip_vi.vi_stat.vst_ino,
        )
    }
    fn argv(&mut self, pid: i32) -> Result<Vec<OsString>, InspectError> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut bytes = vec![0u8; self.argmax];
        let mut size = bytes.len();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size > bytes.len() {
            return Err(Self::failure(pid));
        }
        bytes.truncate(size);
        parse_procargs(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cublet-process-refs-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(
                path.join("Cargo.toml"),
                b"[package]\nname=\"owned-fixture\"\nversion=\"0.0.0\"\n",
            )
            .unwrap();
            fs::create_dir_all(path.join("target/debug/deps")).unwrap();
            fs::write(path.join("target/debug/deps/input.o"), b"owned fixture").unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn provenance(&self) -> Provenance {
            Provenance::load(&self.0.join("target")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn os(bytes: &[u8]) -> OsString {
        OsString::from_vec(bytes.to_vec())
    }
    fn disposition(result: Result<Observation, GuardFailure>) -> GuardDisposition {
        result.unwrap_err().disposition
    }
    fn run(f: &Fixture, cwd: &Path, args: &[&[u8]]) -> Result<Observation, GuardFailure> {
        let mut observation = Observation::default();
        evaluate(
            &f.provenance(),
            cwd,
            &args.iter().map(|a| os(a)).collect::<Vec<_>>(),
            &mut observation,
            Instant::now(),
        )?;
        Ok(observation)
    }
    fn procargs(args: &[&[u8]]) -> Vec<u8> {
        let mut bytes = (args.len() as i32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/python3\0\0\0");
        for arg in args {
            bytes.extend_from_slice(arg);
            bytes.push(0);
        }
        bytes.extend_from_slice(b"ENV=must-not-become-argv\0");
        bytes
    }

    #[test]
    fn parser_preserves_interior_empty_non_utf8_space_and_bounded_token_count() {
        let args: &[&[u8]] = &[
            b"python3",
            b"",
            b"target/debug/deps/space \xff.o",
            b"--input=target/debug/deps/input.o",
        ];
        assert_eq!(
            parse_procargs(&procargs(args)).unwrap(),
            std::iter::once(os(b"/bin/python3"))
                .chain(args.iter().map(|a| os(a)))
                .collect::<Vec<_>>()
        );
        assert!(parse_procargs(&0_i32.to_ne_bytes()).is_err());
        assert!(parse_procargs(&(-1_i32).to_ne_bytes()).is_err());
        assert!(parse_procargs(&[1, 0]).is_err());
        let mut truncated = 2_i32.to_ne_bytes().to_vec();
        truncated.extend_from_slice(b"/bin/tool\0tool\0unterminated");
        assert!(parse_procargs(&truncated).is_err());
        let mut no_executable = 1_i32.to_ne_bytes().to_vec();
        no_executable.extend_from_slice(b"\0tool\0");
        assert!(parse_procargs(&no_executable).is_err());
    }

    #[test]
    fn leading_empty_argv_keeps_first_apparent_path_and_kernel_builder_identity() {
        let f = Fixture::new();
        let tokens = parse_procargs(&procargs(&[b"", b"target/debug/deps/input.o"])).unwrap();
        assert_eq!(tokens[0], os(b"/bin/python3"));
        assert_eq!(tokens[1], os(b"target/debug/deps/input.o"));
        assert_eq!(tokens[2], os(b"ENV=must-not-become-argv"));
        let mut observation = Observation::default();
        assert_eq!(
            evaluate(
                &f.provenance(),
                &f.0,
                &tokens,
                &mut observation,
                Instant::now()
            )
            .unwrap_err()
            .disposition,
            GuardDisposition::ObservedActive
        );
        let mut builder = 2_i32.to_ne_bytes().to_vec();
        builder.extend_from_slice(b"/usr/bin/cargo\0\0\0\0build\0ENV=value\0");
        let tokens = parse_procargs(&builder).unwrap();
        assert_eq!(tokens[0], os(b"/usr/bin/cargo"));
        assert_eq!(
            evaluate(
                &f.provenance(),
                &f.0,
                &tokens,
                &mut observation,
                Instant::now()
            )
            .unwrap_err()
            .disposition,
            GuardDisposition::ObservedActive
        );
        let mut missing = 1_i32.to_ne_bytes().to_vec();
        missing.extend_from_slice(b"/bin/tool\0\0\0");
        assert!(parse_procargs(&missing).is_err());
    }

    #[test]
    fn unopened_relative_argument_and_option_retain_but_repo_residency_does_not() {
        let f = Fixture::new();
        assert_eq!(
            disposition(run(&f, &f.0, &[b"python3", b"target/debug/deps/input.o"])),
            GuardDisposition::ObservedActive
        );
        assert_eq!(
            disposition(run(
                &f,
                &f.0,
                &[b"python3", b"--input=target/debug/deps/input.o"]
            )),
            GuardDisposition::ObservedActive
        );
        assert!(run(
            &f,
            &f.0,
            &[
                b"python3",
                b"-c",
                b"import time; time.sleep(5)",
                b"unrelated.txt"
            ]
        )
        .is_ok());
        fs::write(f.0.join("source.rs"), b"source").unwrap();
        let source = f.0.join("source.rs");
        assert!(run(&f, &f.0, &[b"python3", source.as_os_str().as_bytes()]).is_ok());
        assert_eq!(
            disposition(run(&f, &f.0.join("target/debug"), &[b"python3"])),
            GuardDisposition::ObservedActive
        );
        assert_eq!(
            disposition(run(&f, &f.0, &[b"cargo", b"build"])),
            GuardDisposition::ObservedActive
        );
    }

    #[test]
    fn paths_preserve_spaces_non_utf8_parent_and_symlink_prefix_identity() {
        let f = Fixture::new();
        let spaced = f.0.join("target/debug/deps/space input.o");
        fs::write(&spaced, b"owned bytes").unwrap();
        assert_eq!(
            disposition(run(
                &f,
                &f.0,
                &[b"consumer", b"target/debug/deps/space input.o"]
            )),
            GuardDisposition::ObservedActive
        );
        let path = f.0.join("target/debug/deps").join(os(b"space \xff.o"));
        #[cfg(target_os = "macos")]
        {
            let write = fs::write(&path, b"owned bytes");
            assert!(
                write.is_err(),
                "native macOS fixture must reject invalid UTF-8 filename"
            );
            assert_eq!(
                disposition(run(
                    &f,
                    &f.0,
                    &[b"consumer", b"target/debug/deps/space \xff.o"]
                )),
                GuardDisposition::InspectionUnavailable
            );
            assert_eq!(
                disposition(run(&f, &f.0, &[b"consumer", path.as_os_str().as_bytes()])),
                GuardDisposition::InspectionUnavailable
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            fs::write(&path, b"owned bytes").unwrap();
            assert_eq!(
                disposition(run(
                    &f,
                    &f.0,
                    &[b"consumer", b"target/debug/deps/space \xff.o"]
                )),
                GuardDisposition::ObservedActive
            );
            assert_eq!(
                disposition(run(&f, &f.0, &[b"consumer", path.as_os_str().as_bytes()])),
                GuardDisposition::ObservedActive
            );
        }
        symlink(f.0.join("target/debug/deps"), f.0.join("alias")).unwrap();
        assert_eq!(
            disposition(run(&f, &f.0, &[b"consumer", b"alias/../deps/input.o"])),
            GuardDisposition::ObservedActive
        );
        let outside = f.0.join("outside");
        fs::create_dir(&outside).unwrap();
        assert_eq!(
            disposition(run(
                &f,
                &outside,
                &[b"consumer", b"../target/debug/deps/input.o"]
            )),
            GuardDisposition::ObservedActive
        );
        assert_eq!(
            disposition(run(&f, &f.0, &[b"consumer", b"target/debug/deps/future.o"])),
            GuardDisposition::ObservedActive
        );
    }

    #[test]
    fn opaque_relevant_forms_retain_as_unknown_without_lossy_decode() {
        let f = Fixture::new();
        assert_eq!(
            disposition(run(
                &f,
                &f.0,
                &[b"consumer", b"-Ldependency=target/debug/deps"]
            )),
            GuardDisposition::InspectionUnavailable
        );
        assert_eq!(
            disposition(run(
                &f,
                &f.0,
                &[b"python3", b"-c", b"open('target/debug/deps/input.o\xff')"]
            )),
            GuardDisposition::InspectionUnavailable
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            disposition(run(&f, &f.0, &[b"consumer", b"unrelated \xff"])),
            GuardDisposition::InspectionUnavailable
        );
        #[cfg(not(target_os = "macos"))]
        assert!(run(&f, &f.0, &[b"consumer", b"unrelated \xff"]).is_ok());
    }

    #[test]
    fn non_path_errors_are_excluded_only_outside_owner_and_without_cache_provenance() {
        let f = Fixture::new();
        let outside = Fixture::new();
        let long = vec![b'x'; 300];
        let observed = run(&f, &outside.0, &[b"consumer", &long]).unwrap();
        assert_eq!(observed.non_path_tokens_excluded, 1);
        assert_eq!(
            disposition(run(&f, &f.0, &[b"consumer", &long])),
            GuardDisposition::InspectionUnavailable
        );
        fs::write(outside.0.join("opaque-value"), b"ordinary file").unwrap();
        let observed = run(&f, &outside.0, &[b"consumer", b"opaque-value/suffix"]).unwrap();
        assert_eq!(observed.non_path_tokens_excluded, 1);
        fs::write(f.0.join("ordinary-file"), b"ordinary file").unwrap();
        let owner_hint = f.0.join("ordinary-file/suffix");
        assert_eq!(
            disposition(run(
                &f,
                &outside.0,
                &[b"consumer", owner_hint.as_os_str().as_bytes()]
            )),
            GuardDisposition::InspectionUnavailable
        );
        fs::write(outside.0.join("target/non-directory"), b"ordinary file").unwrap();
        assert_eq!(
            disposition(run(
                &f,
                &outside.0,
                &[b"consumer", b"target/non-directory/suffix"]
            )),
            GuardDisposition::InspectionUnavailable
        );
        let selected_hint = f.0.join("target/debug/deps/input.o/suffix");
        assert_eq!(
            disposition(run(
                &f,
                &outside.0,
                &[b"consumer", selected_hint.as_os_str().as_bytes()]
            )),
            GuardDisposition::InspectionUnavailable
        );
        symlink(f.0.join("target"), outside.0.join("alias")).unwrap();
        // No raw target/owner/cache text: the successfully resolved symlink
        // prefix is what makes this otherwise ENOTDIR token relevant.
        assert_eq!(
            disposition(run(
                &f,
                &outside.0,
                &[b"consumer", b"alias/debug/deps/input.o/suffix"]
            )),
            GuardDisposition::InspectionUnavailable
        );
    }

    #[test]
    fn non_path_rule_never_excludes_permission_io_encoding_loop_or_budget_errors() {
        let f = Fixture::new();
        let provenance = f.provenance();
        for errno in [
            libc::EACCES,
            libc::EPERM,
            libc::EIO,
            libc::ELOOP,
            libc::EILSEQ,
        ] {
            let error = OperandFailure::io(
                "injected_metadata",
                std::io::Error::from_raw_os_error(errno),
                false,
            );
            assert!(!exclude_non_path_token(
                &provenance,
                false,
                b"opaque",
                &error
            ));
        }
        assert!(!exclude_non_path_token(
            &provenance,
            false,
            b"opaque",
            &OperandFailure::stage("operand_time_limit", false)
        ));
        for errno in [libc::ENOTDIR, libc::ENAMETOOLONG] {
            let error = OperandFailure::io(
                "injected_metadata",
                std::io::Error::from_raw_os_error(errno),
                false,
            );
            assert!(exclude_non_path_token(
                &provenance,
                false,
                b"opaque",
                &error
            ));
            assert!(!exclude_non_path_token(
                &provenance,
                true,
                b"opaque",
                &error
            ));
            assert!(!exclude_non_path_token(
                &provenance,
                false,
                b"target/opaque",
                &error
            ));
            assert!(!exclude_non_path_token(
                &provenance,
                false,
                b"opaque",
                &OperandFailure {
                    cache_prefix_seen: true,
                    ..error
                }
            ));
        }
    }

    fn birth(sec: u64) -> Birth {
        Birth {
            sec,
            usec: 0,
            current_user: true,
        }
    }
    struct Fake {
        births: VecDeque<Result<Birth, InspectError>>,
        cwd: Result<WorkingDirectory, InspectError>,
        cwd_changes: VecDeque<Result<WorkingDirectory, InspectError>>,
        argv: Result<Vec<OsString>, InspectError>,
    }
    impl Inspector for Fake {
        fn birth(&mut self, _: i32) -> Result<Birth, InspectError> {
            self.births.pop_front().expect("unexpected birth read")
        }
        fn cwd(&mut self, _: i32) -> Result<WorkingDirectory, InspectError> {
            self.cwd_changes
                .pop_front()
                .unwrap_or_else(|| self.cwd.clone())
        }
        fn argv(&mut self, _: i32) -> Result<Vec<OsString>, InspectError> {
            self.argv.clone()
        }
    }
    fn working_directory(path: &Path) -> WorkingDirectory {
        let metadata = fs::metadata(path).unwrap();
        bind_working_directory(path, metadata_device(&metadata), metadata.ino()).unwrap()
    }
    fn fake(f: &Fixture, births: &[Result<Birth, InspectError>]) -> Fake {
        Fake {
            births: births.iter().copied().collect(),
            cwd: Ok(working_directory(&f.0)),
            cwd_changes: VecDeque::new(),
            argv: Ok(vec![os(b"resident"), os(b"unrelated")]),
        }
    }
    fn ps() -> Vec<u8> {
        let uid = unsafe { libc::getuid() };
        format!("2000000000 {uid} {uid} {uid} resident\n").into_bytes()
    }
    fn foreign_uid() -> u32 {
        let uid = unsafe { libc::getuid() };
        let euid = unsafe { libc::geteuid() };
        (0..=2)
            .find(|candidate| *candidate != uid && *candidate != euid)
            .unwrap()
    }
    fn foreign_ps(command: &str) -> Vec<u8> {
        let uid = foreign_uid();
        format!("2000000000 {uid} {uid} {uid} {command}\n").into_bytes()
    }

    #[test]
    fn cwd_path_must_match_native_vnode_identity_not_only_absolute_tail() {
        let f = Fixture::new();
        let original = working_directory(&f.0);
        let tail = f.0.join("unrelated-tail");
        fs::create_dir(&tail).unwrap();
        assert!(bind_working_directory(&tail, original.dev, original.ino).is_err());
        assert!(
            bind_working_directory(&f.0.join("missing-tail"), original.dev, original.ino).is_err()
        );
        assert!(
            bind_working_directory(Path::new("relative-tail"), original.dev, original.ino).is_err()
        );
        assert!(bind_working_directory(&f.0, original.dev.wrapping_add(1), original.ino).is_err());
        assert!(bind_working_directory(&f.0, original.dev, 0).is_err());
        assert_eq!(
            bind_working_directory(&f.0, original.dev, original.ino).unwrap(),
            original
        );
    }

    #[test]
    fn same_pid_cwd_churn_retries_once_and_repeated_change_is_unknown() {
        let f = Fixture::new();
        let second = f.0.join("second");
        fs::create_dir(&second).unwrap();
        let first = working_directory(&f.0);
        let second = working_directory(&second);
        let births = [Ok(birth(1)); 4];
        let mut once = fake(&f, &births);
        once.cwd_changes = [
            Ok(first.clone()),
            Ok(second.clone()),
            Ok(second.clone()),
            Ok(second.clone()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            inspect(&f.provenance(), &ps(), &mut once)
                .unwrap()
                .retried_pids,
            1
        );
        let mut twice = fake(&f, &births);
        twice.cwd_changes = [Ok(first.clone()), Ok(second.clone()), Ok(second), Ok(first)]
            .into_iter()
            .collect();
        assert_eq!(
            disposition(inspect(&f.provenance(), &ps(), &mut twice)),
            GuardDisposition::InspectionUnavailable
        );
    }

    #[test]
    fn pid_reuse_retries_one_snapshot_and_repeated_churn_retains() {
        let f = Fixture::new();
        let mut once = fake(
            &f,
            &[Ok(birth(1)), Ok(birth(2)), Ok(birth(2)), Ok(birth(2))],
        );
        let observed = inspect(&f.provenance(), &ps(), &mut once).unwrap();
        assert_eq!(observed.retried_pids, 1);
        assert_eq!(observed.inspected_current_user, 1);
        let mut twice = fake(
            &f,
            &[Ok(birth(1)), Ok(birth(2)), Ok(birth(2)), Ok(birth(3))],
        );
        assert_eq!(
            disposition(inspect(&f.provenance(), &ps(), &mut twice)),
            GuardDisposition::InspectionUnavailable
        );
    }

    #[test]
    fn relevant_permission_missing_cwd_and_unknown_birth_retain() {
        let f = Fixture::new();
        let mut denied = fake(&f, &[Ok(birth(1)), Ok(birth(1))]);
        denied.argv = Err(InspectError::Unavailable);
        assert_eq!(
            disposition(inspect(&f.provenance(), &ps(), &mut denied)),
            GuardDisposition::InspectionUnavailable
        );
        let mut no_cwd = fake(&f, &[Ok(birth(1)), Ok(birth(1))]);
        no_cwd.cwd = Err(InspectError::Unavailable);
        assert_eq!(
            disposition(inspect(&f.provenance(), &ps(), &mut no_cwd)),
            GuardDisposition::InspectionUnavailable
        );
        let mut unknown = fake(&f, &[Err(InspectError::Unavailable)]);
        assert_eq!(
            disposition(inspect(&f.provenance(), &ps(), &mut unknown)),
            GuardDisposition::InspectionUnavailable
        );
    }

    #[test]
    fn verified_disappearance_and_foreign_uid_boundary_are_counted() {
        let f = Fixture::new();
        let mut gone = fake(&f, &[Ok(birth(1)), Err(InspectError::Gone)]);
        gone.argv = Err(InspectError::Unavailable);
        assert_eq!(
            inspect(&f.provenance(), &ps(), &mut gone)
                .unwrap()
                .disappeared_pids,
            1
        );
        let mut other = fake(
            &f,
            &[Ok(Birth {
                current_user: false,
                ..birth(1)
            })],
        );
        let observed = inspect(&f.provenance(), &foreign_ps("resident"), &mut other).unwrap();
        assert_eq!(observed.outside_user_provenance, 1);
        assert_eq!(observed.inspected_current_user, 0);
        assert!(observed.coverage_boundary.contains("foreign"));
        let mut other_hint = fake(
            &f,
            &[Ok(Birth {
                current_user: false,
                ..birth(1)
            })],
        );
        let hint = foreign_ps(&format!("consumer {}", f.0.display()));
        assert_eq!(
            disposition(inspect(&f.provenance(), &hint, &mut other_hint)),
            GuardDisposition::InspectionUnavailable
        );
    }

    #[test]
    fn structured_uid_boundary_requires_all_three_foreign_and_no_hint() {
        let f = Fixture::new();
        let uid = unsafe { libc::getuid() };
        let foreign = foreign_uid();
        for current_slot in 0..3 {
            let mut uids = [foreign; 3];
            uids[current_slot] = uid;
            let ps = format!("2000000000 {} {} {} resident\n", uids[0], uids[1], uids[2]);
            let mut denied = fake(&f, &[Err(InspectError::Unavailable)]);
            assert_eq!(
                disposition(inspect(&f.provenance(), ps.as_bytes(), &mut denied)),
                GuardDisposition::InspectionUnavailable
            );
        }
        let mut ownership_changed = fake(
            &f,
            &[Ok(Birth {
                current_user: false,
                ..birth(1)
            })],
        );
        let failure = inspect(&f.provenance(), &ps(), &mut ownership_changed).unwrap_err();
        assert_eq!(failure.disposition, GuardDisposition::InspectionUnavailable);
        assert!(failure.message.contains("ownership_changed_since_ps"));
        // No native call is available: a successfully parsed foreign snapshot,
        // rather than a permission error, is the sole boundary evidence.
        let mut foreign_only = fake(&f, &[]);
        let observed = inspect(
            &f.provenance(),
            &foreign_ps("foreign-resident"),
            &mut foreign_only,
        )
        .unwrap();
        assert_eq!(observed.outside_user_provenance, 1);
        assert_eq!(observed.inspected_current_user, 0);
        for malformed in [
            "2000000000 1 2 command",
            "2000000000 1 2 nope command",
            "2000000000 1 2 -3 command",
            "2000000000 1 2 4294967296 command",
            "2000000000 1 2 3",
            "2000000000 +1 2 3 command",
        ] {
            assert!(enumerate(malformed.as_bytes(), &f.provenance()).is_err());
        }
    }

    #[test]
    fn malformed_enumeration_and_unvalidated_owner_are_unknown() {
        let f = Fixture::new();
        for bytes in [
            b"".as_slice(),
            b"bad command\n",
            b"123\n",
            b"12 1 1 1 a\n12 1 1 1 b\n",
            b"-1 cmd\n",
        ] {
            assert!(enumerate(bytes, &f.provenance()).is_err());
        }
        fs::remove_file(f.0.join("Cargo.toml")).unwrap();
        assert!(Provenance::load(&f.0.join("target")).is_err());
    }

    #[test]
    fn go_owner_is_validated_and_ownerless_global_cache_stays_unknown() {
        let f = Fixture::new();
        fs::write(f.0.join("go.mod"), b"module ownedfixture\n").unwrap();
        let cache = f.0.join("nested/go-build");
        fs::create_dir_all(cache.join("pkg")).unwrap();
        let provenance = Provenance::load(&cache).unwrap();
        assert!(!provenance.cargo);
        assert_eq!(provenance.owner, f.0);
        let mut observation = Observation::default();
        let failure = evaluate(
            &provenance,
            &f.0,
            &[os(b"consumer"), os(b"nested/go-build/pkg/input.a")],
            &mut observation,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(failure.disposition, GuardDisposition::ObservedActive);
        fs::remove_file(f.0.join("go.mod")).unwrap();
        // An isolated owned parent has no Go manifest in its bounded ancestry.
        // Typical host ancestors carry none; prove the test precondition rather
        // than assuming an unrelated /tmp or root manifest cannot exist.
        let parent_has_manifest =
            f.0.parent()
                .unwrap()
                .ancestors()
                .take(15)
                .any(|p| p.join("go.mod").exists() || p.join("go.work").exists());
        assert!(
            !parent_has_manifest,
            "owned fixture requires no unrelated ancestor Go manifest"
        );
        assert!(Provenance::load(&cache).is_err());
        symlink("missing-module", f.0.join("go.mod")).unwrap();
        assert!(Provenance::load(&cache).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_unopened_helper_relative_operand_and_unrelated_control() {
        use std::process::{Child, Command, Stdio};
        struct OwnedChild(Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let f = Fixture::new();
        for (arg, expected_active) in [
            ("target/debug/deps/input.o", true),
            ("unrelated.txt", false),
        ] {
            let mut child = OwnedChild(
                Command::new("/usr/bin/python3")
                    .args(["-c", "import sys; sys.stdin.read()", arg])
                    .current_dir(&f.0)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let pid = child.0.id() as i32;
            let mut native = Native::new().unwrap();
            let started = Instant::now();
            loop {
                if let Ok(argv) = native.argv(pid) {
                    if argv.last() == Some(&OsString::from(arg)) {
                        break;
                    }
                }
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "owned helper did not enter expected argv"
                );
                assert!(child.0.try_wait().unwrap().is_none(), "owned helper exited");
                std::thread::sleep(Duration::from_millis(10));
            }
            let uid = unsafe { libc::getuid() };
            let ps = format!("{pid} {uid} {uid} {uid} helper\n");
            let result = inspect(&f.provenance(), ps.as_bytes(), &mut native);
            if expected_active {
                assert_eq!(disposition(result), GuardDisposition::ObservedActive);
            } else {
                assert_eq!(result.unwrap().inspected_current_user, 1);
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "helper must remain live during observation"
            );
            assert_eq!(
                fs::read(f.0.join("target/debug/deps/input.o")).unwrap(),
                b"owned fixture"
            );
        }
    }
}
