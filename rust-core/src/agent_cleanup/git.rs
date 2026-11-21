//! Read-only index provenance, including genuine .git-file worktrees.
use super::*;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

#[derive(Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ObservationState {
    #[default]
    NotReached,
    RepositoryInspected,
    NoMarkerInBoundedAncestry,
    Unavailable,
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Observation {
    pub(super) state: ObservationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_present: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    marker_identity: Option<(u64, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_identity: Option<(u64, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scoped_tracked_names: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scoped_tracked_identities: Option<usize>,
}
impl Observation {
    pub(super) fn unavailable() -> Self {
        Self {
            state: ObservationState::Unavailable,
            ..Self::default()
        }
    }
}

pub(super) struct Tracked {
    scope: PathBuf,
    paths: HashSet<PathBuf>,
    identities: HashSet<(u64, u64)>,
    anchor: Option<(PathBuf, Option<Identity>, PathBuf, Option<Identity>)>,
}
fn signature(path: &Path) -> Result<Option<Identity>, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_symlink() => Ok(Some(Identity::metadata(&m))),
        Ok(_) => Err("Git metadata symlink has unknown provenance; retained".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Git metadata unreadable; retained: {e}")),
    }
}
fn validate_program(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("CUBLET_GIT_PROGRAM must be an absolute trusted executable".into());
    }
    let metadata = fs::metadata(path)
        .map_err(|e| format!("Trusted Git program unavailable; retained: {e}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("CUBLET_GIT_PROGRAM is not an executable file".into());
    }
    Ok(())
}

fn command(scope: &Path, args: &[&OsStr]) -> Result<Vec<u8>, String> {
    let program = std::env::var_os("CUBLET_GIT_PROGRAM")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"));
    validate_program(&program)?;
    let mut command = Command::new(program);
    command
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.quotepath=false",
            "-C",
        ])
        .arg(scope)
        .args(args);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_PREFIX",
    ] {
        command.env_remove(key);
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    let output = bounded_output(&mut command, Duration::from_secs(5))
        .map_err(|e| format!("Git inspection incomplete; retained: {e}"))?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(format!(
            "Git inspection failed; retained: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    Ok(output.stdout)
}
fn path_output(mut bytes: Vec<u8>) -> Result<PathBuf, String> {
    if bytes.pop() != Some(b'\n') {
        return Err("Git path response incomplete; retained".into());
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}
impl Tracked {
    // Only projects observations already made by load; no additional I/O.
    pub(super) fn observation(&self) -> Observation {
        match &self.anchor {
            Some((_, marker, _, index)) => Observation {
                state: ObservationState::RepositoryInspected,
                index_present: Some(index.is_some()),
                marker_identity: marker.as_ref().map(|m| (m.dev, m.ino)),
                index_identity: index.as_ref().map(|m| (m.dev, m.ino)),
                scoped_tracked_names: Some(self.paths.len()),
                scoped_tracked_identities: Some(self.identities.len()),
            },
            None => Observation {
                state: ObservationState::NoMarkerInBoundedAncestry,
                ..Observation::default()
            },
        }
    }
    pub(super) fn load(scope: &Path) -> Result<Self, String> {
        let mut marker = None;
        for (depth, parent) in scope.ancestors().enumerate() {
            if depth > 64 {
                return Err("Git ancestry budget exceeded; retained".into());
            }
            let path = parent.join(".git");
            if let Some(identity) = signature(&path)? {
                marker = Some((path, identity));
                break;
            }
        }
        let Some((marker, marker_identity)) = marker else {
            return Ok(Self {
                scope: scope.into(),
                paths: HashSet::new(),
                identities: HashSet::new(),
                anchor: None,
            });
        };
        let root = path_output(command(
            scope,
            &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
        )?)?;
        if !root.is_absolute() {
            return Err("Git root is not absolute; retained".into());
        }
        let (_, root_file) = safe_root(&root)?;
        let root_identity = Identity::metadata(&root_file.metadata().map_err(|e| e.to_string())?);
        let (_, scope_file) = safe_root(scope)?;
        let mut ancestor = scope_file.try_clone().map_err(|e| e.to_string())?;
        let mut chain = Vec::new();
        let mut found_root = false;
        for _ in 0..64 {
            let identity = Identity::metadata(&ancestor.metadata().map_err(|e| e.to_string())?);
            if identity.same_object(&root_identity) {
                found_root = true;
                break;
            }
            chain.push((identity.dev, identity.ino));
            let parent = open_child(&ancestor, OsStr::new(".."))?;
            if identity.same_object(&Identity::metadata(
                &parent.metadata().map_err(|e| e.to_string())?,
            )) {
                break;
            }
            ancestor = parent;
        }
        if !found_root {
            return Err("Git root does not physically contain selected scope; retained".into());
        }
        chain.reverse();
        let index = path_output(command(
            scope,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--path-format=absolute"),
                OsStr::new("--git-path"),
                OsStr::new("index"),
            ],
        )?)?;
        let before = signature(&index)?;
        // Enumerate from the repository ROOT without a literal scoped pathspec.
        // Git's name matching is not APFS case/Unicode alias matching.
        let bytes = command(
            &root,
            &[
                OsStr::new("ls-files"),
                OsStr::new("--cached"),
                OsStr::new("--full-name"),
                OsStr::new("-z"),
            ],
        )?;
        if !bytes.is_empty() && bytes.last() != Some(&0) {
            return Err("Git tracked list truncated; retained".into());
        }
        let mut paths = HashSet::new();
        let mut identities = HashSet::new();
        let mut prefixes = HashMap::new();
        for (number, part) in bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .enumerate()
        {
            if number >= MAX_ENTRIES {
                return Err("Repository tracked-index budget exceeded; candidate retained".into());
            }
            let relative = PathBuf::from(OsString::from_vec(part.to_vec()));
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|p| !matches!(p, Component::Normal(_)))
            {
                return Err("Invalid Git tracked path; retained".into());
            }
            let components: Vec<_> = relative.components().map(|p| p.as_os_str()).collect();
            if components.len() <= chain.len() {
                continue;
            }
            let prefix: PathBuf = components.iter().take(chain.len()).collect();
            let inside = if let Some(inside) = prefixes.get(&prefix) {
                *inside
            } else {
                let inside =
                    physical_prefix_matches(&root_file, &components[..chain.len()], &chain)?;
                prefixes.insert(prefix, inside);
                inside
            };
            if !inside {
                continue;
            }
            let scoped: PathBuf = components.iter().skip(chain.len()).collect();
            paths.insert(scoped.clone());
            let mut directory = scope_file.try_clone().map_err(|e| e.to_string())?;
            let mut present = true;
            for component in components
                .iter()
                .skip(chain.len())
                .take(components.len() - chain.len() - 1)
            {
                match directory_child(&directory, component)? {
                    Some(child) => directory = child,
                    None => {
                        present = false;
                        break;
                    }
                }
            }
            if !present {
                continue;
            }
            if let Some(identity) = tracked_identity(&directory, components.last().unwrap())? {
                identities.insert(identity);
            }
        }
        if before != signature(&index)? || Some(marker_identity.clone()) != signature(&marker)? {
            return Err("Git index/provenance changed during inspection; retained".into());
        }
        Ok(Self {
            scope: scope.into(),
            paths,
            identities,
            anchor: Some((marker, Some(marker_identity), index, before)),
        })
    }
    pub(super) fn contains(&self, path: &Path, identity: &Identity) -> bool {
        self.paths.contains(path) || self.identities.contains(&(identity.dev, identity.ino))
    }
    pub(super) fn revalidate(&mut self) -> Result<bool, String> {
        let reload = match &self.anchor {
            None => true,
            Some((marker, identity, index, index_identity)) => {
                signature(marker)? != *identity || signature(index)? != *index_identity
            }
        };
        if reload {
            *self = Self::load(&self.scope)?;
        }
        Ok(reload)
    }
}

fn directory_child(parent: &File, name: &OsStr) -> Result<Option<File>, String> {
    let name = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if fd >= 0 {
        return Ok(Some(unsafe { File::from_raw_fd(fd) }));
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
    ) {
        Ok(None)
    } else {
        Err(format!(
            "Git tracked ancestry unreadable; retained: {error}"
        ))
    }
}
fn physical_prefix_matches(
    root: &File,
    names: &[&OsStr],
    expected: &[(u64, u64)],
) -> Result<bool, String> {
    let mut current = root.try_clone().map_err(|e| e.to_string())?;
    for (name, expected) in names.iter().zip(expected) {
        let Some(child) = directory_child(&current, name)? else {
            return Ok(false);
        };
        let metadata = child.metadata().map_err(|e| e.to_string())?;
        if (metadata.dev(), metadata.ino()) != *expected {
            return Ok(false);
        }
        current = child;
    }
    Ok(true)
}
fn tracked_identity(parent: &File, name: &OsStr) -> Result<Option<(u64, u64)>, String> {
    let name = cstr(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        let stat = unsafe { stat.assume_init() };
        return Ok(Some((stat.st_dev as u64, stat.st_ino)));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(format!(
            "Git tracked identity unreadable; retained: {error}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_distinguish_bounded_no_marker_absent_index_and_actual_reload() {
        let path = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("cublet-git-observations-{}", id().unwrap()));
        fs::create_dir(&path).unwrap();
        let result = std::panic::catch_unwind(|| {
            let mut absent = Tracked::load(&path).unwrap();
            assert_eq!(
                json!(absent.observation())["state"],
                "no_marker_in_bounded_ancestry"
            );
            assert!(json!(absent.observation()).get("indexPresent").is_none());
            assert!(absent.revalidate().unwrap());
            assert_eq!(
                json!(absent.observation())["state"],
                "no_marker_in_bounded_ancestry"
            );
            let run = |args: &[&str]| {
                let output = Command::new("/usr/bin/git")
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "-c",
                        "core.fsmonitor=false",
                        "-C",
                    ])
                    .arg(&path)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            run(&["init", "--quiet", "--template="]);
            let mut tracked = Tracked::load(&path).unwrap();
            let observation = json!(tracked.observation());
            assert_eq!(observation["state"], "repository_inspected");
            assert_eq!(observation["indexPresent"], false);
            assert!(observation.get("head").is_none());
            assert!(!tracked.revalidate().unwrap());
            fs::write(path.join("input.o"), b"owned tracked input").unwrap();
            run(&["add", "input.o"]);
            assert!(tracked.revalidate().unwrap());
            assert_eq!(json!(tracked.observation())["indexPresent"], true);
            assert_eq!(json!(tracked.observation())["scopedTrackedNames"], 1);
            assert!(!tracked.revalidate().unwrap());
        });
        fs::remove_dir_all(&path).unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
    #[test]
    fn operator_git_program_rejects_relative_and_nonexecutables() {
        assert!(validate_program(Path::new("git")).is_err());
        assert!(validate_program(Path::new("/usr/bin/git")).is_ok());
        let path = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("cublet-git-program-{}", id().unwrap()));
        fs::write(&path, b"owned nonexecutable").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_program(&path).is_err());
        fs::remove_file(path).unwrap();
    }
}
