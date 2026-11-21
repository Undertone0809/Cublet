use crate::cleanup_disposition::Mode;
use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct Stamp {
    path: String,
    fingerprint: String,
}

pub(crate) fn seal(stamps: Vec<Stamp>) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        macos::seal(&stamps)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = stamps;
        Err("Application cleanup is available on macOS".into())
    }
}

pub(crate) fn dispose(mode: Mode, stamps: Vec<Stamp>, seal: Option<String>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::dispose(mode, &stamps, seal.as_deref())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (mode, stamps, seal);
        Err("Application cleanup is available on macOS".into())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{Mode, Stamp};
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    const CHANGED: &str = "This item changed. Review it again before proceeding.";
    const MAX_TREE_ENTRIES: usize = 200_000;

    fn nonnegative(value: i32) -> io::Result<i32> {
        if value < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }

    fn open_directory(parent: &File, name: &CStr) -> io::Result<File> {
        let fd = nonnegative(unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
            )
        })?;
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn stat_at(parent: &File, name: &CStr) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::uninit();
        nonnegative(unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        Ok(unsafe { stat.assume_init() })
    }

    fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::uninit();
        nonnegative(unsafe { libc::fstat(fd, stat.as_mut_ptr()) })?;
        Ok(unsafe { stat.assume_init() })
    }

    fn identity(stat: &libc::stat) -> (i32, u64, i128) {
        (
            stat.st_dev,
            stat.st_ino,
            nanos(stat.st_birthtime, stat.st_birthtime_nsec),
        )
    }

    fn nanos(seconds: i64, nanos: i64) -> i128 {
        i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
    }

    fn signature(stat: &libc::stat) -> String {
        let (device, inode, birth) = identity(stat);
        format!("{device}:{inode}:{birth}")
    }

    fn revision(stat: &libc::stat) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            signature(stat),
            stat.st_mode,
            stat.st_size,
            nanos(stat.st_mtime, stat.st_mtime_nsec),
            nanos(stat.st_ctime, stat.st_ctime_nsec)
        )
    }

    fn file_name(part: &OsStr) -> io::Result<CString> {
        CString::new(part.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
    }

    struct Target {
        parent: File,
        name: CString,
        stat: libc::stat,
    }

    fn check(stamp: &Stamp) -> Result<Target, String> {
        let path = Path::new(&stamp.path);
        if stamp.path.len() > 4096
            || !path.is_absolute()
            || stamp.path.chars().any(char::is_control)
        {
            return Err("Invalid application target".into());
        }
        let parts = path
            .components()
            .skip(1)
            .map(|part| match part {
                Component::Normal(part) => file_name(part),
                _ => Err(io::ErrorKind::InvalidInput.into()),
            })
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        if parts.len() < 2 || parts.len() > 128 {
            return Err("Invalid application target depth".into());
        }
        let (name, ancestors) = parts.split_last().expect("checked nonempty");
        let mut parent = File::open("/").map_err(|error| error.to_string())?;
        let mut signature_parts = Vec::new();
        for ancestor in ancestors {
            parent = open_directory(&parent, ancestor).map_err(|_| CHANGED)?;
            signature_parts.push(signature(
                &stat_fd(parent.as_raw_fd()).map_err(|_| CHANGED)?,
            ));
        }
        let stat = stat_at(&parent, name).map_err(|_| CHANGED)?;
        signature_parts.push(revision(&stat));
        let kind = stat.st_mode & libc::S_IFMT;
        if signature_parts.join("/") != stamp.fingerprint
            || ![libc::S_IFDIR, libc::S_IFREG].contains(&kind)
            || stat.st_dev != stat_fd(parent.as_raw_fd()).map_err(|_| CHANGED)?.st_dev
        {
            return Err(CHANGED.into());
        }
        Ok(Target {
            parent,
            name: name.clone(),
            stat,
        })
    }

    struct Directory(*mut libc::DIR);

    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }

    fn names(directory: &File, remaining: &mut usize) -> io::Result<Vec<CString>> {
        let fd =
            nonnegative(unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        let reader = Directory(raw);
        let mut entries = Vec::new();
        loop {
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(reader.0) };
            if entry.is_null() {
                let errno = unsafe { *libc::__error() };
                if errno != 0 {
                    return Err(io::Error::from_raw_os_error(errno));
                }
                return Ok(entries);
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("Permanent deletion item limit reached"))?;
            entries.push(name.to_owned());
        }
    }

    struct Node {
        name: CString,
        stat: libc::stat,
        children: Option<Vec<Node>>,
    }

    fn inventory(
        parent: &File,
        name: CString,
        device: i32,
        remaining: &mut usize,
        depth: usize,
    ) -> io::Result<Node> {
        let stat = stat_at(parent, &name)?;
        if device != stat.st_dev || depth > 128 {
            return Err(io::Error::other("Protected nested volume or depth limit"));
        }
        let children = if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let directory = open_directory(parent, &name)?;
            if identity(&stat_fd(directory.as_raw_fd())?) != identity(&stat) {
                return Err(io::Error::other(CHANGED));
            }
            Some(
                {
                    let mut children = names(&directory, remaining)?;
                    children.sort_unstable();
                    children
                }
                .into_iter()
                .map(|child| inventory(&directory, child, device, remaining, depth + 1))
                .collect::<io::Result<Vec<_>>>()?,
            )
        } else {
            None
        };
        Ok(Node {
            name,
            stat,
            children,
        })
    }

    fn purge(parent: &File, node: Node) -> io::Result<()> {
        let before = stat_at(parent, &node.name)?;
        let changed = if node.stat.st_nlink > 1 && node.children.is_none() {
            // Removing a sibling hard link can update ctime during this same operation.
            identity(&before) != identity(&node.stat)
                || before.st_mode != node.stat.st_mode
                || before.st_size != node.stat.st_size
                || before.st_mtime != node.stat.st_mtime
                || before.st_mtime_nsec != node.stat.st_mtime_nsec
        } else {
            revision(&before) != revision(&node.stat)
        };
        if changed {
            return Err(io::Error::other(CHANGED));
        }
        let flags = if let Some(children) = node.children {
            let directory = open_directory(parent, &node.name)?;
            if identity(&stat_fd(directory.as_raw_fd())?) != identity(&node.stat) {
                return Err(io::Error::other(CHANGED));
            }
            for child in children {
                purge(&directory, child)?;
            }
            libc::AT_REMOVEDIR
        } else {
            0
        };
        if identity(&stat_at(parent, &node.name)?) != identity(&node.stat) {
            return Err(io::Error::other(CHANGED));
        }
        nonnegative(unsafe { libc::unlinkat(parent.as_raw_fd(), node.name.as_ptr(), flags) })?;
        Ok(())
    }

    fn rename(from: &File, name: &CStr, to: &File, dest: &CStr) -> io::Result<()> {
        nonnegative(unsafe {
            libc::renameatx_np(
                from.as_raw_fd(),
                name.as_ptr(),
                to.as_raw_fd(),
                dest.as_ptr(),
                libc::RENAME_EXCL,
            )
        })?;
        Ok(())
    }

    extern "C" {
        fn CC_SHA256(data: *const std::ffi::c_void, len: u32, digest: *mut u8) -> *mut u8;
    }

    fn tree_seal(node: &Node) -> String {
        fn bytes(node: &Node, root: bool, output: &mut Vec<u8>) {
            let name = node.name.as_bytes();
            output.extend((name.len() as u32).to_le_bytes());
            output.extend(name);
            let signature = if root {
                // Renaming the approved object into its private sibling changes ctime.
                format!(
                    "{}:{}:{}:{}:{}",
                    signature(&node.stat),
                    node.stat.st_mode,
                    node.stat.st_size,
                    node.stat.st_mtime,
                    node.stat.st_mtime_nsec
                )
            } else {
                revision(&node.stat)
            };
            output.extend((signature.len() as u32).to_le_bytes());
            output.extend(signature.as_bytes());
            let children = node.children.as_deref().unwrap_or_default();
            output.extend((children.len() as u32).to_le_bytes());
            for child in children {
                bytes(child, false, output);
            }
        }
        let mut input = Vec::new();
        bytes(node, true, &mut input);
        let mut digest = [0; 32];
        // Inventory count, maximum name length, and fixed-size metadata bound input.
        unsafe {
            CC_SHA256(
                input.as_ptr().cast(),
                input.len() as u32,
                digest.as_mut_ptr(),
            )
        };
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn permanent(target: &Target, path: &Path, seal: &str) -> Result<(), String> {
        let staging = Stage::claim(target, path)?;
        let mut remaining = MAX_TREE_ENTRIES;
        let entry = inventory(
            &staging.dir,
            target.name.clone(),
            target.stat.st_dev,
            &mut remaining,
            0,
        )
        .and_then(|entry| {
            if !same_claim(&entry.stat, &target.stat) || tree_seal(&entry) != seal {
                Err(io::Error::other(CHANGED))
            } else {
                Ok(entry)
            }
        });
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Err(staging.rollback(&error.to_string())),
        };
        purge(&staging.dir, entry).map_err(|error| {
            format!(
                "Deletion stopped. Remaining files are in {}: {error}",
                staging.path.display()
            )
        })
    }

    fn same_claim(new: &libc::stat, old: &libc::stat) -> bool {
        identity(new) == identity(old)
            && new.st_mode == old.st_mode
            && new.st_mtime == old.st_mtime
            && new.st_mtime_nsec == old.st_mtime_nsec
            && new.st_size == old.st_size
    }

    struct Stage<'a> {
        target: &'a Target,
        dir: File,
        name: CString,
        path: std::path::PathBuf,
    }

    impl<'a> Stage<'a> {
        fn claim(target: &'a Target, path: &Path) -> Result<Self, String> {
            let nonce = crate::cleanup_disposition::nonce().map_err(|error| error.to_string())?;
            let name = CString::new(format!(".cublet-uninstall-{nonce}")).expect("ASCII");
            let path = path.with_file_name(name.to_string_lossy().as_ref());
            let parent_fd = target.parent.as_raw_fd();
            if revision(&stat_at(&target.parent, &target.name).map_err(|_| CHANGED)?)
                != revision(&target.stat)
            {
                return Err(CHANGED.into());
            }
            nonnegative(unsafe { libc::mkdirat(parent_fd, name.as_ptr(), 0o700) })
                .map_err(|error| error.to_string())?;
            let dir = match open_directory(&target.parent, &name) {
                Ok(dir) => dir,
                Err(error) => {
                    unsafe { libc::unlinkat(parent_fd, name.as_ptr(), libc::AT_REMOVEDIR) };
                    return Err(error.to_string());
                }
            };
            let staging = Self {
                target,
                dir,
                name,
                path,
            };
            rename(&target.parent, &target.name, &staging.dir, &target.name)
                .map_err(|error| format!("Could not secure item for removal: {error}"))?;
            if !stat_at(&staging.dir, &target.name)
                .is_ok_and(|entry| same_claim(&entry, &target.stat))
            {
                return Err(staging.rollback(CHANGED));
            }
            Ok(staging)
        }

        fn rollback(&self, reason: &str) -> String {
            let target = self.target;
            match rename(&self.dir, &target.name, &target.parent, &target.name) {
                Ok(()) => format!("Original item was restored: {reason}"),
                Err(_) => format!("Item retained in {}: {reason}", self.path.display()),
            }
        }
    }

    impl Drop for Stage<'_> {
        fn drop(&mut self) {
            // Keeps a nonempty staging folder intact for recovery.
            if let (Ok(path_entry), Ok(open_dir)) = (
                stat_at(&self.target.parent, &self.name),
                stat_fd(self.dir.as_raw_fd()),
            ) {
                if identity(&path_entry) == identity(&open_dir) {
                    unsafe {
                        libc::unlinkat(
                            self.target.parent.as_raw_fd(),
                            self.name.as_ptr(),
                            libc::AT_REMOVEDIR,
                        )
                    };
                }
            }
        }
    }

    #[link(name = "Foundation", kind = "framework")]
    extern "C" {}

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const libc::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const libc::c_char) -> *const std::ffi::c_void;
        fn objc_msgSend();
    }

    fn trash(path: &str, directory: bool) -> Result<(), String> {
        use std::ffi::c_void;
        type Obj = *mut c_void;
        type Sel = *const c_void;
        let path = CString::new(path).map_err(|error| error.to_string())?;
        // These selectors' signatures follow Foundation's Objective-C headers.
        unsafe {
            let send: unsafe extern "C" fn(Obj, Sel) -> Obj =
                std::mem::transmute(objc_msgSend as *const ());
            let send_void: unsafe extern "C" fn(Obj, Sel) =
                std::mem::transmute(objc_msgSend as *const ());
            let url: unsafe extern "C" fn(Obj, Sel, *const libc::c_char, bool, Obj) -> Obj =
                std::mem::transmute(objc_msgSend as *const ());
            let move_item: unsafe extern "C" fn(Obj, Sel, Obj, *mut Obj, *mut Obj) -> bool =
                std::mem::transmute(objc_msgSend as *const ());
            let pool = send(
                objc_getClass(c"NSAutoreleasePool".as_ptr()),
                sel_registerName(c"new".as_ptr()),
            );
            let file = url(
                objc_getClass(c"NSURL".as_ptr()),
                sel_registerName(
                    c"fileURLWithFileSystemRepresentation:isDirectory:relativeToURL:".as_ptr(),
                ),
                path.as_ptr(),
                directory,
                std::ptr::null_mut(),
            );
            let manager = send(
                objc_getClass(c"NSFileManager".as_ptr()),
                sel_registerName(c"defaultManager".as_ptr()),
            );
            let mut error = std::ptr::null_mut();
            let result = !file.is_null()
                && move_item(
                    manager,
                    sel_registerName(c"trashItemAtURL:resultingItemURL:error:".as_ptr()),
                    file,
                    std::ptr::null_mut(),
                    &mut error,
                );
            send_void(pool, sel_registerName(c"drain".as_ptr()));
            if result {
                Ok(())
            } else {
                Err("macOS could not move this item to Trash. Check access and try again.".into())
            }
        }
    }

    fn checked_target(stamps: &[Stamp]) -> Result<Target, String> {
        // Bundle, plist, executable, up to 32 helper+plist pairs, and ShipIt.
        if stamps.is_empty() || stamps.len() > 68 {
            return Err("Invalid application removal verification".into());
        }
        let first = &stamps[0];
        let path = Path::new(&first.path);
        for stamp in &stamps[1..] {
            if stamp.path == first.path || !Path::new(&stamp.path).starts_with(path) {
                return Err("Invalid application removal verification".into());
            }
            check(stamp)?;
        }
        check(first)
    }

    pub(super) fn seal(stamps: &[Stamp]) -> Result<String, String> {
        let target = checked_target(stamps)?;
        let mut remaining = MAX_TREE_ENTRIES;
        let entry = inventory(
            &target.parent,
            target.name,
            target.stat.st_dev,
            &mut remaining,
            0,
        )
        .map_err(|error| error.to_string())?;
        if revision(&entry.stat) != revision(&target.stat) {
            return Err(CHANGED.into());
        }
        Ok(tree_seal(&entry))
    }

    pub(super) fn dispose(mode: Mode, stamps: &[Stamp], seal: Option<&str>) -> Result<(), String> {
        let target = checked_target(stamps)?;
        let first = &stamps[0];
        match mode {
            Mode::Permanent => permanent(
                &target,
                Path::new(&first.path),
                seal.filter(|value| {
                    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
                })
                .ok_or("Missing permanent deletion verification. Review the selection again.")?,
            ),
            Mode::Trash => {
                let staging = Stage::claim(&target, Path::new(&first.path))?;
                // Cocoa takes a path. Give it the claimed entry in a private
                // directory, never reopen the caller-visible original name.
                let directory = stat_fd(staging.dir.as_raw_fd())
                    .map_err(|error| staging.rollback(&error.to_string()))?;
                let absolute = open_directory_at_path(&staging.path)
                    .map_err(|error| staging.rollback(&error.to_string()))?;
                if identity(&directory) != identity(&absolute)
                    || !stat_at(&staging.dir, &target.name)
                        .is_ok_and(|item| same_claim(&item, &target.stat))
                {
                    return Err(staging.rollback(CHANGED));
                }
                let path = staging.path.join(OsStr::from_bytes(target.name.as_bytes()));
                trash(
                    path.to_str().ok_or("Invalid Trash path")?,
                    target.stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
                )
                .map_err(|error| staging.rollback(&error))
            }
        }
    }

    fn open_directory_at_path(path: &Path) -> io::Result<libc::stat> {
        let mut directory = File::open("/")?;
        for component in path.components().skip(1) {
            directory = open_directory(&directory, &file_name(component.as_os_str())?)?;
        }
        stat_fd(directory.as_raw_fd())
    }
}
