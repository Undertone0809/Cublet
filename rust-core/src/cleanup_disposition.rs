use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Permanent,
    Trash,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Disposition {
    pub(crate) mode: Mode,
    pub(crate) revision: String,
}

fn file_path() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Some(root) = std::env::var_os("MSM_DISK_CACHE_ROOT") {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err("The debug settings root must be absolute".into());
        }
        return Ok(root.join("cublet-cleanup-disposition-v1.json"));
    }
    dirs::data_dir()
        .map(|root| root.join("mac-storage-manager/cublet-cleanup-disposition-v1.json"))
        .ok_or_else(|| "Could not locate Cublet settings".into())
}

fn read(path: &Path) -> Result<Disposition, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Disposition {
                mode: Mode::Permanent,
                revision: "default".into(),
            });
        }
        Err(error) => return Err(format!("Could not read cleanup settings: {error}")),
    };
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Invalid cleanup settings file".into());
    }
    let mut bytes = Vec::new();
    file.take(1025)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 1024 {
        return Err("Cleanup settings file exceeds its size limit".into());
    }
    let policy: Disposition = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid cleanup settings. Choose a cleanup mode again in Settings.")?;
    if policy.revision.len() != 32 || !policy.revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid cleanup settings revision".into());
    }
    Ok(policy)
}

pub(crate) fn nonce() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn get() -> Result<Disposition, String> {
    read(&file_path()?)
}

pub(crate) fn set(mode: Mode) -> Result<Disposition, String> {
    write(&file_path()?, mode).map_err(|error| format!("Could not save cleanup settings: {error}"))
}

fn write(path: &Path, mode: Mode) -> io::Result<Disposition> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    let revision = nonce()?;
    let temporary = parent.join(format!(".cleanup-settings-{revision}"));
    let mut file = OpenOptions::new()
        .write(true)
        .mode(0o600)
        .create_new(true)
        .open(&temporary)?;
    let policy = Disposition { mode, revision };
    let result = (|| {
        serde_json::to_writer(&mut file, &policy)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.map(|()| policy)
}

pub(crate) fn require_space_reclaim_mode() -> Result<(), String> {
    if get()?.mode == Mode::Trash {
        return Err("Cleanup is set to Move to Trash. Moving these files would not satisfy a free-space goal. No files were changed.".into());
    }
    Ok(())
}
