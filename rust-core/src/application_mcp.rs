use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const RESPONSE_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, PartialEq)]
pub(crate) enum CallError {
    Transport(String),
    Tool(String),
}

impl CallError {
    pub(crate) fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    pub(crate) fn message(self) -> String {
        match self {
            Self::Transport(message) | Self::Tool(message) => message,
        }
    }
}

fn exchange(
    input: &mut impl Write,
    output: &mut impl BufRead,
    method: &str,
    params: Value,
) -> Result<Value, CallError> {
    let transport = |error: std::io::Error| {
        CallError::Transport(format!("Application service connection failed: {error}. Review the current application state before retrying."))
    };
    let request = serde_json::to_vec(&json!({ "method": method, "params": params }))
        .map_err(|error| CallError::Transport(error.to_string()))?;
    input.write_all(&request).map_err(transport)?;
    input.write_all(b"\n").map_err(transport)?;
    input.flush().map_err(transport)?;
    let mut bytes = Vec::new();
    let read = output
        .by_ref()
        .take(RESPONSE_LIMIT)
        .read_until(b'\n', &mut bytes)
        .map_err(transport)?;
    if read == 0 || bytes.last() != Some(&b'\n') {
        return Err(CallError::Transport("Application service response unavailable or oversized. The connection was closed; review the current application state and preview again before retrying.".into()));
    }
    let response: Value = serde_json::from_slice(&bytes).map_err(|_| {
        CallError::Transport(
            "Invalid application service response; preview again before retrying.".into(),
        )
    })?;
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) if response.get("result").is_some() => Ok(response["result"].clone()),
        Some(false) if response["error"].is_string() => {
            Err(CallError::Tool(response["error"].as_str().unwrap().into()))
        }
        _ => Err(CallError::Transport(
            "Malformed application service response; preview again before retrying.".into(),
        )),
    }
}

pub(crate) struct ApplicationSession {
    process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl ApplicationSession {
    pub(crate) fn launch() -> Result<Self, String> {
        if !cfg!(target_os = "macos") {
            return Err("Applications are available on macOS".into());
        }
        let core = std::env::current_exe().map_err(|error| error.to_string())?;
        let (electron, entry) =
            companion(&core).ok_or("Application tools require Cublet's bundled executable")?;
        let mut process = Command::new(electron)
            .arg(entry)
            .arg(core)
            .env("ELECTRON_RUN_AS_NODE", "1")
            .env_remove("NODE_OPTIONS")
            .env_remove("NODE_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not start Cublet application service: {error}"))?;
        let input = process.stdin.take().expect("configured pipe");
        let output = BufReader::new(process.stdout.take().expect("configured pipe"));
        Ok(Self {
            process,
            input,
            output,
        })
    }

    pub(crate) fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        exchange(&mut self.input, &mut self.output, method, params)
    }
}

fn companion(core: &std::path::Path) -> Option<(PathBuf, PathBuf)> {
    if let Some(contents) = core
        .parent()
        .and_then(|bin| bin.parent())
        .and_then(|resources| resources.parent())
    {
        let archive = contents.join("Resources/app.asar");
        let executable = contents.join("MacOS/Cublet");
        if contents.file_name().is_some_and(|name| name == "Contents")
            && archive.is_file()
            && executable.is_file()
        {
            return Some((
                executable,
                archive.join("electron/application-service-stdio.cjs"),
            ));
        }
    }
    #[cfg(debug_assertions)]
    {
        let project = core.parent()?.parent()?.parent()?.parent()?;
        let executable =
            project.join("node_modules/electron/dist/Electron.app/Contents/MacOS/Electron");
        let entry = project.join("electron/application-service-stdio.cjs");
        if executable.is_file() && entry.is_file() {
            return Some((executable, entry));
        }
    }
    None
}

impl Drop for ApplicationSession {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn tool_errors_preserve_stream_alignment_and_following_request() {
        let mut output = Cursor::new(b"{\"ok\":false,\"error\":\"Plan expired\"}\n{\"ok\":true,\"result\":{\"planId\":\"fresh\"}}\n".to_vec());
        let mut input = Vec::new();
        let error =
            exchange(&mut input, &mut output, "execute_app_uninstall", json!({})).unwrap_err();
        assert!(!error.is_transport());
        assert_eq!(error.message(), "Plan expired");
        assert_eq!(
            exchange(&mut input, &mut output, "preview_app_uninstall", json!({})).unwrap()
                ["planId"],
            "fresh"
        );
    }

    #[test]
    fn truncated_oversized_and_malformed_responses_are_transport_faults() {
        for bytes in [
            Vec::new(),
            b"{\"ok\":true}".to_vec(),
            b"garbage\n".to_vec(),
            b"{\"result\":1}\n".to_vec(),
            b"{\"ok\":false}\n".to_vec(),
            vec![b'x'; RESPONSE_LIMIT as usize + 1],
        ] {
            assert!(exchange(
                &mut Vec::new(),
                &mut Cursor::new(bytes),
                "search_applications",
                json!({})
            )
            .unwrap_err()
            .is_transport());
        }
    }
}
