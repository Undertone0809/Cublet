use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn is_app_executable(name: &str) -> bool {
    Path::new(name).is_absolute()
        && ((name.contains("/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/")
            && Path::new(name)
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("Codex (")))
            || name.ends_with("/Codex.app/Contents/MacOS/Codex")
            || (name.contains("/Codex.app/Contents/Frameworks/")
                && Path::new(name)
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("Codex Helper"))))
}

pub fn is_codex_executable(name: &str) -> bool {
    is_app_executable(name)
        || Path::new(name)
            .file_name()
            .is_some_and(|n| n == "codex" || n == "codex.exe")
            && (Path::new(name).is_absolute() || name == "codex" || name == "codex.exe")
}

/// Trusted operator launch configuration, never a model-provided command.
pub(crate) fn ps_command() -> Result<Command, String> {
    ps_command_for(std::env::var_os("CUBLET_PS_PROGRAM"))
}

fn ps_command_for(configured: Option<OsString>) -> Result<Command, String> {
    let program = configured
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/ps"));
    if !program.is_absolute() {
        return Err(
            "CUBLET_PS_PROGRAM must be an absolute trusted executable; inspection incomplete"
                .into(),
        );
    }
    let metadata = std::fs::metadata(&program)
        .map_err(|e| format!("Trusted ps program unavailable; inspection incomplete: {e}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("CUBLET_PS_PROGRAM is not an executable file; inspection incomplete".into());
    }
    Ok(Command::new(program))
}

// No shell, no command arguments or prompts. Bound all native subprocess waits.
pub fn bounded_output(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    bounded_output_with_limit(command, timeout, 16 * 1024 * 1024)
}

fn bounded_output_with_limit(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
) -> Result<Output, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let group = child.id();
    let stdout = child.stdout.take().ok_or("Missing stdout")?;
    let stderr = child.stderr.take().ok_or("Missing stderr")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut b = Vec::new();
        let result = stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut b)
            .map_err(|e| e.to_string())
            .and_then(|_| {
                if b.len() > limit {
                    Err("Native helper output exceeds 16 MiB budget; inspection incomplete".into())
                } else {
                    Ok(b)
                }
            });
        let _ = tx.send((true, result));
    });
    std::thread::spawn(move || {
        use std::io::Read;
        let mut b = Vec::new();
        let result = stderr
            .take(limit as u64 + 1)
            .read_to_end(&mut b)
            .map_err(|e| e.to_string())
            .and_then(|_| {
                if b.len() > limit {
                    Err("Native helper output exceeds 16 MiB budget; inspection incomplete".into())
                } else {
                    Ok(b)
                }
            });
        let _ = tx2.send((false, result));
    });
    let started = Instant::now();
    let mut expired = || {
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // This is only the dedicated process group we just spawned, never a user app.
            unsafe {
                kill(-(group as i32), 9);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        "Operation timed out; helper termination requested. Rescan before retrying".to_string()
    };
    // Collect pipes before waiting: reader descendants are included in the same deadline.
    let mut out = Vec::new();
    let mut err = Vec::new();
    for _ in 0..2 {
        let remaining = timeout.saturating_sub(started.elapsed());
        let (is_stdout, data) = rx.recv_timeout(remaining).map_err(|_| expired())?;
        let data = data.inspect_err(|_| {
            expired();
        })?;
        if is_stdout {
            out = data;
        } else {
            err = data;
        }
    }
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() >= timeout {
            #[cfg(unix)]
            {
                unsafe extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                unsafe {
                    kill(-(group as i32), 9);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                "Operation timed out; helper termination requested. Rescan before retrying".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

pub fn counts() -> Result<(usize, usize), String> {
    let output = bounded_output(
        ps_command()?.args(["-axo", "comm="]),
        Duration::from_secs(3),
    )?;
    if !output.status.success() {
        return Err("Cannot inspect Codex processes; cleanup remains blocked".into());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
    let mut app = 0;
    let mut cli = 0;
    for name in text.lines().map(str::trim) {
        if is_app_executable(name) {
            app += 1;
        } else if is_codex_executable(name) {
            cli += 1;
        }
    }
    Ok((app, cli))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_ps_program_validates_paths_without_environment_changes() {
        assert!(ps_command_for(Some("ps".into())).is_err());
        let path = std::env::temp_dir().join(format!(
            "cublet-ps-program-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(ps_command_for(Some(path.clone().into_os_string())).is_err());
        std::fs::write(&path, b"not executable").unwrap();
        let rejected = ps_command_for(Some(path.clone().into_os_string())).is_err();
        std::fs::remove_file(&path).unwrap();
        assert!(rejected);
        assert!(ps_command_for(Some(std::env::temp_dir().into_os_string())).is_err());
        assert_eq!(ps_command_for(None).unwrap().get_program(), "/bin/ps");
    }

    #[test]
    fn configured_ps_helper_executes_directly_with_original_arguments() {
        // A distinct real executable proves the configured launch is used,
        // without mutating global environment shared with other native tests.
        for fields in ["comm=", "pid=,command="] {
            let mut command = ps_command_for(Some("/bin/echo".into())).unwrap();
            let output =
                bounded_output(command.args(["-axo", fields]), Duration::from_secs(3)).unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, format!("-axo {fields}\n").as_bytes());
        }
    }

    #[test]
    fn unbounded_helper_output_is_stopped_before_inspection() {
        let error = bounded_output_with_limit(
            &mut Command::new("/usr/bin/yes"),
            Duration::from_secs(2),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("inspection incomplete"));
    }
    #[test]
    fn bounded_wait_includes_inherited_pipes() {
        let started = Instant::now();
        let result = bounded_output(
            Command::new("/bin/sh").args(["-c", "sleep 3 & exit 0"]),
            Duration::from_millis(100),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn executable_identity_not_prompt_substrings() {
        assert!(is_codex_executable(
            "/Applications/Codex.app/Contents/MacOS/Codex"
        ));
        assert!(is_codex_executable("/opt/homebrew/bin/codex"));
        assert!(is_codex_executable("/Applications/Codex.app/Contents/Frameworks/Codex Helper.app/Contents/MacOS/Codex Helper"));
        assert!(is_app_executable("/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Versions/157.0/Helpers/Codex (Aperitif Renderer).app/Contents/MacOS/Codex (Aperitif Renderer)"));
        assert!(!is_app_executable(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
        ));
        assert!(!is_app_executable(
            "/Applications/Mac 磁盘管家.app/Contents/MacOS/Mac 磁盘管家"
        ));
        assert!(!is_codex_executable("/bin/zsh"));
        assert!(!is_codex_executable("/tmp/codex-storage-manager"));
        assert!(!is_codex_executable("/tmp/codex-research/python"));
        assert!(!is_codex_executable(
            "python prompt about Codex.app/Contents/MacOS/Codex"
        ));
    }
}
