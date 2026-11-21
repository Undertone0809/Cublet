//! The host owns this stdio connection. Only its bundled worker is replaced.
use super::{
    error, read_request, send_response, valid_envelope, APP_VERSION, MAX_RESPONSE, WORKER_STATUS,
};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const CHECK_INTERVAL: Duration = Duration::from_secs(1);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn bundled_executable() -> Option<PathBuf> {
    APP_VERSION?;
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    let resources = executable.parent()?.parent()?;
    if resources.ends_with("Contents/Resources")
        && executable.parent()?.ends_with("bin")
        && matches!(
            executable.file_name()?.to_str()?,
            "cublet-cleaner" | "codex-core"
        )
    {
        Some(executable)
    } else {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
}

impl FileIdentity {
    fn at(path: &Path) -> Option<Self> {
        let file = fs::symlink_metadata(path).ok()?;
        if !file.is_file() {
            return None;
        }
        Some(Self {
            device: file.dev(),
            inode: file.ino(),
            size: file.len(),
            modified: (file.mtime(), file.mtime_nsec()),
        })
    }
}

fn verify_bundle(executable: &Path) -> Result<(), String> {
    // Local debug fixtures exercise the native handoff with owned bundles.
    if cfg!(debug_assertions) {
        return Ok(());
    }
    let bundle = executable
        .ancestors()
        .nth(4)
        .ok_or("Missing Cublet bundle")?;
    let signing_requirement = "=anchor apple generic and certificate leaf[subject.OU] = QRP7F2GTGJ";
    let bundle_requirement = format!(
        "{signing_requirement} and identifier \"com.zeeland.codex-storage-manager.electron\""
    );
    for (path, requirement) in [
        (bundle, bundle_requirement.as_str()),
        (executable, signing_requirement),
    ] {
        let result = Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "-R", requirement])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        if !result.success() {
            return Err("Cublet MCP replacement signature could not be verified".into());
        }
    }
    Ok(())
}

struct Worker {
    process: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Value>,
}

impl Worker {
    fn launch(path: &Path, args: &[String]) -> Result<Self, String> {
        let mut process = Command::new(path)
            .arg("--mcp-worker")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let input = process.stdin.take().expect("configured pipe");
        let mut output = BufReader::new(process.stdout.take().expect("configured pipe"));
        let (send, replies) = mpsc::sync_channel(1);
        std::thread::spawn(move || loop {
            let mut line = Vec::new();
            if output
                .by_ref()
                .take((MAX_RESPONSE + 1) as u64)
                .read_until(b'\n', &mut line)
                .is_err()
                || line.last() != Some(&b'\n')
            {
                break;
            }
            let Ok(value) = serde_json::from_slice(&line) else {
                break;
            };
            if send.send(value).is_err() {
                break;
            }
        });
        let mut worker = Self {
            process,
            input: Some(input),
            replies,
        };
        worker.status(Some(STARTUP_TIMEOUT))?;
        Ok(worker)
    }

    fn exchange(&mut self, request: &Value, timeout: Option<Duration>) -> Result<Value, String> {
        let input = self.input.as_mut().expect("running MCP worker");
        send_response(input, request).map_err(|e| e.to_string())?;
        if valid_envelope(request) && request.get("id").is_none() {
            return Ok(Value::Null);
        }
        let reply = match timeout {
            Some(timeout) => self.replies.recv_timeout(timeout).ok(),
            None => self.replies.recv().ok(),
        }
        .ok_or("Cublet MCP worker did not respond")?;
        if reply["jsonrpc"] != "2.0" || reply["id"] != request["id"] {
            return Err("Unexpected Cublet MCP worker response".into());
        }
        Ok(reply)
    }

    fn status(&mut self, timeout: Option<Duration>) -> Result<bool, String> {
        let reply = self.exchange(
            &json!({
                "jsonrpc":"2.0", "id":"cublet-worker-status", "method":WORKER_STATUS
            }),
            timeout,
        )?;
        reply["result"]["busy"]
            .as_bool()
            .ok_or_else(|| "Incompatible Cublet MCP worker".into())
    }

    fn retire(mut self) {
        // EOF lets the quiescent worker release its session and app companion.
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.process.try_wait().is_ok_and(|status| status.is_none())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[derive(Default)]
struct Connection {
    negotiated: Option<Value>,
    ready: bool,
}

impl Connection {
    fn record(&mut self, request: &Value, reply: &Value) {
        if request["method"] == "initialize" && reply["result"].is_object() {
            self.negotiated = Some(reply["result"].clone());
        } else if request["method"] == "notifications/initialized"
            && valid_envelope(request)
            && request.get("id").is_none()
            && self.negotiated.is_some()
        {
            self.ready = true;
        }
    }

    fn restore(&self, worker: &mut Worker) -> Result<(), String> {
        if let Some(previous) = &self.negotiated {
            // The supervisor is the new worker's client. Keep the startup
            // frame bounded independently of the host's initialize payload.
            let request = json!({
                "jsonrpc":"2.0", "id":"cublet-worker-initialize", "method":"initialize",
                "params":{"protocolVersion":previous["protocolVersion"], "capabilities":{},
                    "clientInfo":{"name":"Cublet MCP supervisor","version":super::server_version()}}
            });
            let next = worker.exchange(&request, Some(STARTUP_TIMEOUT))?;
            if next["result"]["protocolVersion"] != previous["protocolVersion"]
                || next["result"]["capabilities"] != previous["capabilities"]
                || next["result"]["serverInfo"]["name"] != previous["serverInfo"]["name"]
            {
                return Err("Cublet MCP worker changed the negotiated protocol".into());
            }
        }
        if self.ready {
            worker.exchange(
                &json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
                None,
            )?;
        }
        Ok(())
    }
}

pub(super) fn run(executable: PathBuf, args: &[String]) -> Result<(), String> {
    let mut identity = FileIdentity::at(&executable).ok_or("Bundled MCP executable is missing")?;
    let mut worker = Worker::launch(&executable, args)?;
    let mut connection = Connection::default();
    let mut failure = None;
    let (send, frames) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let frame = read_request(&mut input);
            let done = !matches!(&frame, Ok(Some(_)));
            if send.send(frame).is_err() || done {
                break;
            }
        }
    });
    let mut output = io::BufWriter::new(io::stdout().lock());
    loop {
        let frame = frames.recv_timeout(CHECK_INTERVAL);
        if matches!(&frame, Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected)) {
            break;
        }
        if let Some(next) = FileIdentity::at(&executable).filter(|next| *next != identity) {
            let backed_off = failure.is_some_and(|(failed, at): (FileIdentity, Instant)| {
                failed == next && at.elapsed() < RETRY_INTERVAL
            });
            if !backed_off && !worker.status(None)? {
                let candidate = (|| -> Result<Worker, String> {
                    verify_bundle(&executable)?;
                    if FileIdentity::at(&executable) != Some(next) {
                        return Err("Cublet bundle changed during signature verification".into());
                    }
                    let mut candidate = Worker::launch(&executable, args)?;
                    connection.restore(&mut candidate)?;
                    if FileIdentity::at(&executable) != Some(next) {
                        return Err("Cublet bundle changed again during MCP startup".into());
                    }
                    Ok(candidate)
                })();
                match candidate {
                    Ok(candidate) => {
                        std::mem::replace(&mut worker, candidate).retire();
                        identity = next;
                        failure = None;
                        if connection.ready {
                            send_response(
                                &mut output,
                                &json!({
                                    "jsonrpc":"2.0", "method":"notifications/tools/list_changed"
                                }),
                            )
                            .map_err(|e| e.to_string())?;
                        }
                    }
                    Err(e) => {
                        if failure.is_none_or(|(failed, _)| failed != next) {
                            eprintln!("Cublet MCP will retry switching workers: {e}");
                        }
                        failure = Some((next, Instant::now()));
                    }
                }
            }
        }
        let response = match frame {
            Err(RecvTimeoutError::Timeout) => continue,
            Ok(Err(e)) => return Err(e.to_string()),
            Ok(Ok(Some(Err(response)))) => response,
            Ok(Ok(Some(Ok(request)))) => {
                if request["method"] == WORKER_STATUS {
                    if request.get("id").is_none() {
                        continue;
                    }
                    error(request["id"].clone(), -32601, "Method not found")
                } else {
                    let response = worker.exchange(&request, None)?;
                    connection.record(&request, &response);
                    if response.is_null() {
                        continue;
                    }
                    response
                }
            }
            Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => break,
        };
        send_response(&mut output, &response).map_err(|e| e.to_string())?;
    }
    Ok(())
}
