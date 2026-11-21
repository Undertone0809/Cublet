//! Local, newline-delimited MCP. Expensive work belongs to bounded job workers.
use crate::{agent_cleanup::AgentCleanup, agent_scan::AgentScanSession};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod reload;

const VERSION: &str = "2025-11-25";
const WORKER_STATUS: &str = "cublet/worker-status";
// Packaging sets this from package.json. Standalone developer builds keep their crate version.
const APP_VERSION: Option<&str> = option_env!("CUBLET_PACKAGED_VERSION");
pub fn server_version() -> &'static str {
    APP_VERSION.unwrap_or(env!("CARGO_PKG_VERSION"))
}

const MAX_REQUEST: usize = 256 * 1024;
const MAX_RESPONSE: usize = 1024 * 1024;

struct Server {
    cleanup_profile: bool,
    initialized: bool,
    ready: bool,
    shared_cache: bool,
    cleanup_enabled: bool,
    scans: Arc<AgentScanSession>,
    cleanup: AgentCleanup,
    refreshed: HashMap<String, Value>,
    executing: Vec<String>,
    goal_verifications: HashMap<String, Arc<Mutex<Value>>>,
    applications: Option<crate::application_mcp::ApplicationSession>,
    cleanup_executions: HashMap<String, String>,
    cleanup_goals: HashMap<String, String>,
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    destructive: bool,
) -> Value {
    json!({"name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":!destructive,"destructiveHint":destructive,"openWorldHint":false}})
}
fn tools(cleanup: bool) -> Vec<Value> {
    let job = json!({"type":"string","minLength":1,"maxLength":4096});
    let path = json!({"type":"string","minLength":1,"maxLength":4096});
    let offset = json!({"type":"integer","minimum":0,"maximum":1000000,"default":0});
    let limit = json!({"type":"integer","minimum":1,"maximum":200,"default":50});
    let source = json!({"type":"string","enum":["baseline","current"]});
    let mut result = vec![
        tool("storage_status", "Read filesystem free space and session capacity. Directory allocation is an estimate, not reclaimable physical bytes.", json!({}), &[], false),
        tool("scan", "Return a job immediately. Restore retained same-policy baseline in background, then audit the requested scope. Repeated calls reuse an active job. Start with the smallest useful scope; refresh=false also reuses a completed job. Fresh audits still walk all scoped metadata. Poll every 0.5–2 seconds, then back off.", json!({"path":path,"allowProtected":{"type":"boolean","default":false},"refresh":{"type":"boolean","default":true},"mode":{"type":"string","enum":["auto","full"],"default":"auto"}}), &[], false),
        tool("scan_poll", "Read bounded job progress and largest measured rows without waiting for the scan. The baseline is historical; current partial rows are observations. Never infer absence from incomplete coverage.", json!({"jobId":job,"offset":offset,"limit":limit}), &["jobId"], false),
        tool("scan_view", "Browse a measured directory from this job's baseline or current generation. Does not initiate a recursive scan. Cached layer restoration may read its compact index.", json!({"jobId":job,"path":path,"source":source,"offset":offset,"limit":limit}), &["jobId","path"], false),
        tool("scan_list", "Page live immediate children for candidate discovery without a recursive walk. File allocation can be hard-linked. Directory summaries carry source freshness. Names and sizes never authorize cleanup.", json!({"jobId":job,"path":path,"source":source,"afterPath":path,"limit":limit}), &["jobId","path"], false),
        tool("scan_growth", "Compare retained complete same-policy measurements. Growth is endpoint allocation, not disk free-space recovery. Historical queries do not start live scans.", json!({"jobId":job,"path":path,"baselineId":job,"offset":offset,"limit":limit}), &["jobId"], false),
        tool("scan_history", "List this scope's retained complete measurement identities and real times. Partial scans never become completed history.", json!({"jobId":job,"offset":offset,"limit":limit}), &["jobId"], false),
        tool("scan_cancel", "Request cancellation of this session's audit while preserving its usable historical baseline.", json!({"jobId":job}), &["jobId"], false),
        tool("cleanup_preview", "Plan explicit rebuildable Rust target or Go build-cache contents asynchronously. This is a metadata-only preview. Explain rebuilding cost and obtain user authorization for the exact scope before execute. Poll cleanup_status. Huge/inaccessible/in-use trees return a specific limitation. Sources, module downloads and application data are outside these providers.", json!({"kind":{"type":"string","enum":["rust_target","go_build_cache"]},"path":path}), &["kind","path"], false),
        tool("cleanup_status", "Read cleanup preview or durable execution receipt. Reports allocated estimates separately from signed observed volume delta. Concurrent writes can outweigh deletion. Successful/partial execution schedules a fresh scoped scan while retaining history.", json!({"jobId":job}), &["jobId"], false),
        tool("cleanup_cancel", "Request cooperative cancellation and preserve any partial execution receipt. Read cleanup_status for the final result.", json!({"jobId":job}), &["jobId"], false),
    ];
    if cleanup {
        result.push(tool("cleanup_execute", "Permanently delete only the server-held short-lived plan after user authorization for that cache scope. --allow-cleanup enables this capability; it does not supply user intent. Returns immediately, then poll cleanup_status. Retrying the same plan returns its existing operation instead of deleting again. Cancellation/deadline can leave a partial receipt.", json!({"planId":job}), &["planId"], true));
    }
    if cfg!(target_os = "macos") {
        result.extend([
            tool("search_applications", "Find independently installed application bundles and their copies. This is read-only. Use exact paths to tell copies apart.", json!({"query":{"type":"string","maxLength":256},"refresh":{"type":"boolean"},"offset": offset,"limit":limit}), &[], false),
            tool("get_cleanup_disposition", "Read the effective Cublet cleanup mode shared with desktop Settings.", json!({}), &[], false),
            tool("preview_app_uninstall", "Review these exact standalone bundles and related files. Read-only. After a partial operation, pass its retryPlanId to review previously selected failed items again.", json!({"paths":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"string","maxLength":4096}},"retryPlanId":{"type":"string","minLength":1,"maxLength":64}}), &["paths"], false),
        ]);
        if cleanup {
            result.push(tool("execute_app_uninstall", "Apply the selected item IDs from a current preview only after user authorization for those bundles and any selected related data. Confirm the action mode from that plan. Cublet keeps shared or blocked items. Report per-item failures and actual effects.", json!({
                "planId":job,
                "selectedIds":{"type":"array","minItems":1,"maxItems":2048,"items":{"type":"string","maxLength":64}},
                "mode":{"type":"string","enum":["permanent","trash"]},
                "confirmed":{"type":"boolean"}
            }), &["planId","selectedIds","mode","confirmed"], true));
        }
    }
    result
}

const CLEANUP_CLIENT_FLOW: &str = "For an authorized space task, scope may contain multiple/nested repositories and need not itself be Git/Cargo. Call reclaim_space directly with the desired total available bytes, including requested working margin. Native code owns exact checks and verification for its deletion footprint. Poll only for poll_cleanup_status. At report_result relay observed space, actual effects and retained notices; routine parent-Git preflight, group audits or child-plan reconstruction are not prerequisites. Broader independent audit remains valid when explicitly requested by the user or a critical review action requires it. protectionEvidence reports actual phase-specific Git provenance and link/source exclusion events; metadata rechecks are not fresh full-index reads. executionEvidence is guard enforcement, not whole-tree hashing or concurrent source immutability. effects samples at most4 actual children; its durable locator covers all children. Deleted allocation differs from observed signed volume change. verification is optional history refresh. Space is a point-in-time observation; future writes can consume it.";

fn cleanup_tools(enabled: bool) -> Vec<Value> {
    let operation = json!({"type":"string","minLength":1,"maxLength":128});
    let wait = json!({"type":"integer","minimum":0,"maximum":2000,"default":2000});
    let mut result=vec![tool("cleanup_status","Poll native work; at report_result relay actual space/effects/retentions. Native verification covers its deletion footprint. Broader independent audit is valid when explicitly requested by the user or critical review requires it. cancel retains partial effects/recovery; optional history is separate.",json!({"operationId":operation,"waitMs":wait,"cancel":{"type":"boolean","default":false}}),&["operationId"],false)];
    if enabled {
        result.insert(0,tool("reclaim_space","Run native guarded space recovery directly in the authorized scope. Native code discovers providers, checks/executes exact artifacts and verifies actual space; active/unknown inputs are retained. Rebuilds regenerate outputs. Poll for poll_cleanup_status; relay report_result with effects/retentions. Broader independent audit is valid when explicitly requested by the user or critical review requires it.",json!({"scope":{"type":"string","minLength":1,"maxLength":4096,"description":"Authorized directory, possibly a container of multiple/nested repositories; it need not be Git/Cargo. Native discovery needs no parent-Git preflight."},"targetAvailableBytes":{"type":"integer","minimum":1,"description":"Desired total available bytes on the scope volume, including requested working margin for known ongoing writes. Stops at the observed target; this point-in-time readback does not guarantee future headroom."},"providers":{"type":"array","items":{"type":"string","enum":["rust_target","go_build_cache"]},"minItems":1,"maxItems":2,"default":["rust_target"]},"waitMs":wait}),&["scope","targetAvailableBytes"],true));
    }
    result
}

fn validate_arguments(definition: &Value, args: &Value) -> Result<(), String> {
    let object = args.as_object().ok_or("Tool arguments must be an object")?;
    let schema = &definition["inputSchema"];
    for key in schema["required"].as_array().into_iter().flatten() {
        if !object.contains_key(key.as_str().unwrap_or_default()) {
            return Err(format!("Missing argument {key}"));
        }
    }
    for (key, value) in object {
        let rule = schema["properties"]
            .get(key)
            .ok_or_else(|| format!("Unknown argument {key}"))?;
        let valid = match rule["type"].as_str() {
            Some("string") => value.as_str().is_some_and(|s| {
                s.len() >= rule["minLength"].as_u64().unwrap_or(0) as usize
                    && s.len() <= rule["maxLength"].as_u64().unwrap_or(4096) as usize
            }),
            Some("boolean") => value.is_boolean(),
            Some("array") => value.as_array().is_some_and(|items| {
                items.len() >= rule["minItems"].as_u64().unwrap_or(0) as usize
                    && items.len() <= rule["maxItems"].as_u64().unwrap_or(16) as usize
                    && items.iter().all(|v| {
                        let item = &rule["items"];
                        if let Some(choices) = item["enum"].as_array() {
                            choices.contains(v)
                        } else {
                            item["type"] == "string"
                                && v.as_str().is_some_and(|s| {
                                    !s.is_empty()
                                        && s.len()
                                            <= item["maxLength"].as_u64().unwrap_or(4096) as usize
                                })
                        }
                    })
            }),
            Some("integer") => value.as_u64().is_some_and(|n| {
                n >= rule["minimum"].as_u64().unwrap_or(0)
                    && n <= rule["maximum"].as_u64().unwrap_or(u64::MAX)
            }),
            _ => false,
        };
        if !valid
            || rule["enum"]
                .as_array()
                .is_some_and(|choices| !choices.contains(value))
        {
            return Err(format!("Invalid argument {key}"));
        }
    }
    Ok(())
}
fn string(args: &Value, key: &str) -> Option<String> {
    args[key].as_str().map(str::to_owned)
}
fn page(args: &Value) -> (usize, usize) {
    (
        args["offset"].as_u64().unwrap_or(0) as usize,
        args["limit"].as_u64().unwrap_or(50) as usize,
    )
}

fn active_cleanup(value: &Value) -> bool {
    !matches!(
        value["state"].as_str(),
        Some("ready" | "completed" | "partial" | "cancelled" | "failed" | "interrupted")
    )
}

fn goal_replay_key(args: &Value) -> String {
    let mut providers: Vec<&str> = args["providers"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_else(|| vec!["rust_target"]);
    providers.sort_unstable();
    providers.dedup();
    json!([args["scope"], args["targetAvailableBytes"], providers]).to_string()
}
impl Server {
    fn call(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let definitions = if self.cleanup_profile {
            cleanup_tools(self.cleanup_enabled)
        } else {
            tools(self.cleanup_enabled)
        };
        let definition = definitions
            .iter()
            .find(|tool| tool["name"] == name)
            .ok_or("Unknown or disabled tool")?;
        validate_arguments(definition, &args)?;
        if self.cleanup_profile {
            let value = match name {
                "reclaim_space" => {
                    let key = goal_replay_key(&args);
                    self.cleanup_goals.retain(|_, id| {
                        self.cleanup
                            .status(id)
                            .is_ok_and(|value| active_cleanup(&value))
                    });
                    if let Some(id) = self.cleanup_goals.get(&key) {
                        // This only observes already-started work. Its confirmed
                        // disposition remains fixed even after Settings changes.
                        self.cleanup
                            .goal_status(id, args["waitMs"].as_u64().unwrap_or(2000), false)
                    } else {
                        crate::cleanup_disposition::require_space_reclaim_mode()?;
                        let scans = self.scans.clone();
                        let scope = args["scope"].as_str().unwrap().to_owned();
                        let value = self.cleanup.reclaim_with_before(&args, move || {
                            scans.settle_before_cleanup(&scope)?;
                            crate::macdisk::agent_cleanup_audit_lease()
                        })?;
                        if let Some(id) = value["operationId"].as_str() {
                            self.cleanup_goals.insert(key, id.to_owned());
                        }
                        Ok(value)
                    }
                }
                "cleanup_status" => self.cleanup.goal_status(
                    args["operationId"].as_str().unwrap(),
                    args["waitMs"].as_u64().unwrap_or(2000),
                    args["cancel"].as_bool().unwrap_or(false),
                ),
                _ => Err("Unknown compact tool".into()),
            }?;
            return self
                .goal_verification(value, args["cancel"].as_bool().unwrap_or(false))
                .map(history_purpose);
        }
        let job = args["jobId"].as_str().unwrap_or_default();
        let (offset, limit) = page(&args);
        match name {
            "storage_status" => {
                let mut status = self.scans.storage_status()?;
                status["cacheMode"] = json!(if self.shared_cache {
                    "shared"
                } else {
                    "isolated"
                });
                status["cleanupEnabled"] = json!(self.cleanup_enabled);
                Ok(status)
            }
            "get_cleanup_disposition" => serde_json::to_value(crate::cleanup_disposition::get()?)
                .map_err(|error| error.to_string()),
            "search_applications" | "preview_app_uninstall" | "execute_app_uninstall" => {
                if name == "execute_app_uninstall" && args["confirmed"] != true {
                    return Err("Confirm the exact items and cleanup disposition".into());
                }
                if self.applications.is_none() {
                    self.applications = Some(crate::application_mcp::ApplicationSession::launch()?);
                }
                let result = self
                    .applications
                    .as_mut()
                    .expect("session exists")
                    .call(name, args);
                if result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.is_transport())
                {
                    // Drop kills/reaps the companion and discards untrusted
                    // buffered tails. Business errors keep current plans alive.
                    self.applications = None;
                }
                result.map_err(|error| error.message())
            }
            "scan" => {
                self.executing.retain(|id| {
                    self.cleanup
                        .status(id)
                        .is_ok_and(|v| matches!(v["state"].as_str(), Some("running" | "measuring")))
                });
                if !self.executing.is_empty() {
                    return Err(
                        "Cleanup is active; poll cleanup_status before starting another audit"
                            .into(),
                    );
                }
                self.scans.scan(
                    string(&args, "path"),
                    args["allowProtected"].as_bool().unwrap_or(false),
                    args["refresh"].as_bool().unwrap_or(true),
                    string(&args, "mode"),
                )
            }
            "scan_poll" => self.scans.poll(job, offset, limit),
            "scan_view" => self.scans.view(
                job,
                string(&args, "path").unwrap(),
                string(&args, "source"),
                offset,
                limit,
            ),
            "scan_list" => self.scans.list(
                job,
                string(&args, "path").unwrap(),
                string(&args, "source"),
                string(&args, "afterPath"),
                limit,
            ),
            "scan_growth" => self.scans.growth(
                job,
                string(&args, "path"),
                string(&args, "baselineId"),
                offset,
                limit,
            ),
            "scan_history" => self.scans.history(job, offset, limit),
            "scan_cancel" => self.scans.cancel(job),
            "cleanup_preview" => self.cleanup.preview(&args),
            "cleanup_status" => {
                let mut result = self.cleanup.status(job)?;
                self.refresh_cleanup(&mut result, job);
                Ok(result)
            }
            "cleanup_execute" => {
                let id = args["planId"].as_str().unwrap();
                if let Some(existing) = self.cleanup_executions.get(id) {
                    // A repeated plan ID can only read this execution receipt;
                    // it cannot create new work under a different disposition.
                    return self.cleanup.status(existing);
                }
                crate::cleanup_disposition::require_space_reclaim_mode()?;
                let preview = self.cleanup.status(id)?;
                let scans = self.scans.clone();
                let path = preview["path"]
                    .as_str()
                    .ok_or("Cleanup scope unavailable")?
                    .to_owned();
                let value = self.cleanup.execute_with_before(id, move || {
                    scans.settle_before_cleanup(&path)?;
                    crate::macdisk::agent_cleanup_audit_lease()
                })?;
                if let Some(id) = value["jobId"].as_str() {
                    self.cleanup_executions
                        .retain(|_, job| self.cleanup.status(job).is_ok());
                    self.cleanup_executions
                        .insert(args["planId"].as_str().unwrap().to_owned(), id.to_owned());
                    if !self.executing.iter().any(|existing| existing == id) {
                        self.executing.push(id.to_owned());
                    }
                }
                Ok(value)
            }
            "cleanup_cancel" => self.cleanup.cancel(job),
            _ => Err("Unknown tool".into()),
        }
    }
    fn goal_verification(&mut self, mut value: Value, cancel: bool) -> Result<Value, String> {
        let Some(operation) = value["operationId"].as_str().map(str::to_owned) else {
            return Ok(value);
        };
        if !matches!(
            value["state"].as_str(),
            Some("completed" | "partial" | "failed" | "cancelled" | "interrupted")
        ) || value["deletedFiles"].as_u64().unwrap_or(0) == 0
        {
            return Ok(value);
        }
        if cancel || value["state"] == "cancelled" {
            if let Some(state) = self.goal_verifications.get(&operation) {
                if let Ok(mut tracker) = state.lock() {
                    tracker["cancelRequested"] = json!(true);
                    if let Some(id) = tracker["scanJobId"].as_str() {
                        let _ = self.scans.cancel(id);
                    }
                    tracker["state"] = json!("cancelled");
                    tracker["fresh"] = json!(false);
                    value["verification"] = tracker.clone();
                }
            } else {
                value["verification"] = json!({"verificationId":format!("verify-{operation}"),"state":"cancelled","fresh":false,"reason":"Cancellation preserved historical baseline; no background audit scheduled"});
            }
            return Ok(value);
        }
        let retry = self
            .goal_verifications
            .get(&operation)
            .and_then(|s| s.lock().ok())
            .is_some_and(|s| {
                s["state"] == "deferred"
                    && s["retryWhenHeadroomAvailable"] == true
                    && verification_headroom_safe(&value)
            });
        if retry {
            self.goal_verifications.remove(&operation);
        }
        if !self.goal_verifications.contains_key(&operation) {
            if self.goal_verifications.len() >= 32 {
                let expired = self
                    .goal_verifications
                    .keys()
                    .find(|id| self.cleanup.status(id).is_err())
                    .cloned();
                let deferred = || {
                    self.goal_verifications
                        .iter()
                        .find(|(_, state)| state.lock().is_ok_and(|s| s["state"] == "deferred"))
                        .map(|(id, _)| id.clone())
                };
                if let Some(key) = expired.or_else(deferred) {
                    self.goal_verifications.remove(&key);
                } else {
                    value["verification"] = json!({"verificationId":format!("verify-{operation}"),"state":"deferred","fresh":false,"reason":"Verification capacity in use; historical baseline retained"});
                    return Ok(value);
                }
            }
            let state = Arc::new(Mutex::new(
                json!({"verificationId":format!("verify-{operation}"),"state":"pending","fresh":false}),
            ));
            self.goal_verifications
                .insert(operation.clone(), state.clone());
            let scans = self.scans.clone();
            let cleanup = self.cleanup.clone();
            let operation = operation.clone();
            let scope = value["scope"].as_str().map(str::to_owned).or_else(|| {
                self.cleanup
                    .status(&operation)
                    .ok()
                    .and_then(|v| v["scope"].as_str().map(str::to_owned))
            });
            let Some(scope) = scope else {
                value["verification"] = json!({"verificationId":format!("verify-{operation}"),"state":"deferred","fresh":false,"reason":"Goal scope evidence unavailable for audit; returned space observation retained"});
                return Ok(value);
            };
            std::thread::spawn(move || {
                run_goal_verification(scans, cleanup, operation, scope, state)
            });
        }
        if let Some(state) = self.goal_verifications.get(&operation) {
            value["verification"] = state
                .lock()
                .map_err(|_| "Verification state unavailable")?
                .clone();
        }
        Ok(value)
    }
    fn refresh_cleanup(&mut self, result: &mut Value, job: &str) {
        if let Some(verification) = self.refreshed.get(job) {
            result["verificationScan"] = verification.clone();
            return;
        }
        if result["operation"] == "execute"
            && (result["deletedFiles"].as_u64().unwrap_or(0) > 0
                || result["recoveryFiles"].as_u64().unwrap_or(0) > 0)
            && matches!(
                result["state"].as_str(),
                Some("completed" | "partial" | "cancelled" | "failed" | "interrupted")
            )
        {
            if let Some(path) = result["path"].as_str() {
                match self.scans.refresh_after_cleanup(path) {
                    Ok(mut verification) if verification.get("jobId").is_none() => {
                        verification["retryOnNextStatus"] = json!(true);
                        result["verificationScan"] = verification;
                    }
                    Ok(verification) => {
                        if self.refreshed.len() >= 32 {
                            if let Some(key) = self.refreshed.keys().next().cloned() {
                                self.refreshed.remove(&key);
                            }
                        }
                        self.refreshed.insert(job.to_owned(), verification.clone());
                        result["verificationScan"] = verification;
                    }
                    Err(e) => {
                        result["verificationScan"] = json!({"error":e,"retryOnNextStatus":true})
                    }
                }
            }
        }
    }
    fn handle(&mut self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request["method"].as_str();
        if !valid_envelope(&request) {
            return Some(error(
                id.unwrap_or(Value::Null),
                -32600,
                "Invalid JSON-RPC request",
            ));
        }
        let method = method.unwrap();
        if id.is_none() {
            if method == "notifications/initialized" && self.initialized {
                self.ready = true;
            }
            return None;
        }
        let id = id.unwrap();
        let params = &request["params"];
        let result: Result<Value, String> = match method {
            "initialize" if !self.initialized => {
                if !params["protocolVersion"].is_string()
                    || !params["capabilities"].is_object()
                    || !params["clientInfo"]["name"].is_string()
                {
                    return Some(error(id, -32602, "Invalid initialize parameters"));
                }
                self.initialized = true;
                Ok(
                    json!({"protocolVersion":VERSION,"capabilities":{"tools":{"listChanged":true}},
                    "serverInfo":{"name":"cublet-cleaner","title":"Cublet Cleaner","version":server_version()},
                    "instructions":if self.cleanup_profile {CLEANUP_CLIENT_FLOW} else {"Start with storage_status and a scoped scan. scan returns immediately; poll with backoff, browse cached baseline while auditing. Cache is historical until verified. A fresh audit traverses scoped metadata. Cancel work when enough evidence exists. Select large rebuildable scopes, plan once and execute once with user authorization, then read signed df receipts. Do not repeatedly rescan unchanged scopes or claim directory allocation as recovered space."}}),
                )
            }
            "ping" => Ok(json!({})),
            _ if !self.ready => {
                return Some(error(
                    id,
                    -32002,
                    "Initialize and send notifications/initialized first",
                ))
            }
            "tools/list" => {
                if params.get("cursor").is_some() {
                    return Some(error(id, -32602, "Unknown tools cursor"));
                }
                Ok(
                    json!({"tools":if self.cleanup_profile {cleanup_tools(self.cleanup_enabled)} else {tools(self.cleanup_enabled)},
                           "_meta":{"cc.cublet/serverVersion":server_version()}}),
                )
            }
            "tools/call" => {
                let Some(name) = params["name"].as_str() else {
                    return Some(error(id, -32602, "Missing tool name"));
                };
                let called = self.call(name, params.get("arguments").cloned().unwrap_or(json!({})));
                Ok(match called {
                    Ok(value) if self.cleanup_profile => compact_goal_result(value),
                    Ok(value) => {
                        json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                    }
                    Err(message) => {
                        json!({"content":[{"type":"text","text":message}],"isError":true})
                    }
                })
            }
            _ => return Some(error(id, -32601, "Method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result.unwrap_or(Value::Null)}))
    }
}
fn valid_envelope(request: &Value) -> bool {
    request["jsonrpc"] == "2.0"
        && request["method"].is_string()
        && request
            .get("id")
            .is_none_or(|id| id.is_string() || id.is_i64() || id.is_u64())
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

// Reserve the largest supported cache publication envelope plus metadata room.
fn history_purpose(mut value: Value) -> Value {
    if value["verification"].is_object() {
        value["verification"]["purpose"] = json!("optional_history_refresh");
        value["verification"]["requiredForCleanupCompletion"] = json!(false);
        value["verification"]["freshAppliesTo"] = json!("history_generation");
    }
    value
}
fn compact_goal_result(value: Value) -> Value {
    let wrap = |value: &Value| json!({"content":[{"type":"text","text":value.to_string()}],"isError":false});
    let result = wrap(&value);
    if result.to_string().len() <= 8192 {
        return result;
    }
    // Share the native receipt budget so both paths preserve all critical fields.
    let mut small = AgentCleanup::budget_goal_receipt(&value);
    if value["verification"].is_object() {
        let v = &value["verification"];
        small["verification"] = json!({"verificationId":v["verificationId"].as_str().unwrap_or("").chars().take(64).collect::<String>(),"state":v["state"].as_str().unwrap_or("unknown").chars().take(32).collect::<String>(),"fresh":v["fresh"].as_bool(),"purpose":"optional_history_refresh","requiredForCleanupCompletion":false,"freshAppliesTo":"history_generation"});
    }
    wrap(&small)
}

// A tight goal is explicitly deferred rather than spent on a new broad audit.
const VERIFICATION_RESERVE: u64 = (512 + 16) * 1024 * 1024;
fn verification_headroom_safe(value: &Value) -> bool {
    value["durabilityIncomplete"] != true
        && value["recovery"]["files"].as_u64().unwrap_or(0) == 0
        && value["availableBytes"]
            .as_u64()
            .zip(value["targetAvailableBytes"].as_u64())
            .is_some_and(|(available, target)| {
                available.saturating_sub(target) >= VERIFICATION_RESERVE
            })
}
fn run_goal_verification(
    scans: Arc<AgentScanSession>,
    cleanup: AgentCleanup,
    operation: String,
    scope: String,
    state: Arc<Mutex<Value>>,
) {
    let update = |change: Value| {
        if let Ok(mut v) = state.lock() {
            if let Some(fields) = change.as_object() {
                for (key, value) in fields {
                    v[key] = value.clone();
                }
            }
        }
    };
    let result = (|| -> Result<(), String> {
        cleanup.verification_publication_barrier()?;
        if state.lock().is_ok_and(|v| v["cancelRequested"] == true) {
            return Ok(());
        }
        let before = cleanup.goal_status(&operation, 0, false)?;
        if !verification_headroom_safe(&before) {
            update(
                json!({"state":"deferred","fresh":false,"reason":"Preserving requested headroom; historical baseline retained. Background audit needs publication reserve.","requiredSurplusBytes":VERIFICATION_RESERVE,"retryWhenHeadroomAvailable":true}),
            );
            return Ok(());
        }
        let job = scans.refresh_after_cleanup(&scope)?;
        let Some(id) = job["jobId"].as_str().map(str::to_owned) else {
            update(
                json!({"state":"deferred","fresh":false,"reason":"Another scope is auditing; historical baseline retained","retryWhenHeadroomAvailable":true}),
            );
            return Ok(());
        };
        update(json!({"state":"running","scanJobId":id,"fresh":false}));
        if state.lock().is_ok_and(|v| v["cancelRequested"] == true) {
            let _ = scans.cancel(&id);
            update(json!({"state":"cancelled","fresh":false}));
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let progress = scans.poll(&id, 0, 1)?;
            match progress["status"].as_str() {
                Some("complete") => {
                    update(
                        json!({"state":"complete","fresh":progress["current"]["freshness"]=="verified","baselineScanId":progress["baseline"]["scanId"],"currentScanId":progress["current"]["scanId"]}),
                    );
                    return Ok(());
                }
                Some("failed" | "cancelled") => {
                    update(
                        json!({"state":progress["status"],"fresh":false,"reason":"Verification did not complete; historical baseline retained"}),
                    );
                    return Ok(());
                }
                _ => {}
            }
            let current = cleanup.goal_status(&operation, 0, false)?;
            if current["availableBytes"]
                .as_u64()
                .zip(current["targetAvailableBytes"].as_u64())
                .is_some_and(|(available, target)| {
                    available < target.saturating_add(16 * 1024 * 1024)
                })
                || Instant::now() >= deadline
            {
                let _ = scans.cancel(&id);
                update(
                    json!({"state":"deferred","fresh":false,"reason":"Verification stopped to preserve headroom/deadline; historical baseline retained","retryWhenHeadroomAvailable":true}),
                );
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })();
    if let Err(error) = result {
        update(
            json!({"state":"failed","fresh":false,"error":error.chars().take(180).collect::<String>()}),
        );
    }
}

fn cleanup_profile(args: &[String]) -> Result<bool, String> {
    let mut enabled = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--allow-cleanup" | "--shared-cache" => {}
            "--profile" if args.get(index + 1).map(String::as_str) == Some("cleanup") => {
                enabled = true;
                index += 1;
            }
            _ => return Err(
                "Usage: codex-core --mcp [--profile cleanup] [--allow-cleanup] [--shared-cache]"
                    .into(),
            ),
        }
        index += 1;
    }
    Ok(enabled)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let profile = cleanup_profile(args)?;
    #[cfg(target_os = "macos")]
    if let Some(executable) = reload::bundled_executable() {
        return reload::run(executable, args);
    }
    serve(args, profile, false)
}

pub fn run_worker(args: &[String]) -> Result<(), String> {
    serve(args, cleanup_profile(args)?, true)
}

fn serve(args: &[String], cleanup_profile: bool, worker: bool) -> Result<(), String> {
    let shared_cache = args.iter().any(|arg| arg == "--shared-cache");
    let cleanup_enabled = args.iter().any(|arg| arg == "--allow-cleanup");
    crate::macdisk::configure_agent_cache(shared_cache)?;
    let cleanup = AgentCleanup::new(AgentCleanup::default_journal_dir()?, cleanup_enabled)?;
    let mut server = Server {
        cleanup_profile,
        initialized: false,
        ready: false,
        shared_cache,
        cleanup_enabled,
        scans: Arc::new(AgentScanSession::new()),
        cleanup,
        refreshed: HashMap::new(),
        executing: Vec::new(),
        goal_verifications: HashMap::new(),
        applications: None,
        cleanup_executions: HashMap::new(),
        cleanup_goals: HashMap::new(),
    };
    let mut input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    while let Some(frame) = read_request(&mut input).map_err(|e| e.to_string())? {
        let response = match frame {
            Ok(request) if worker && request["method"] == WORKER_STATUS => Some(json!({
                "jsonrpc":"2.0", "id":request["id"],
                "result":{"busy":server.cleanup.has_background_work() || server.scans.has_background_work()}
            })),
            Ok(request) => server.handle(request),
            Err(response) => Some(response),
        };
        if let Some(response) = response {
            send_response(&mut output, &response).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn read_request(input: &mut impl BufRead) -> io::Result<Option<Result<Value, Value>>> {
    let mut bytes = Vec::new();
    let count = input
        .by_ref()
        .take((MAX_REQUEST + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_REQUEST {
        if bytes.last() != Some(&b'\n') {
            input.skip_until(b'\n')?;
        }
        return Ok(Some(Err(error(
            Value::Null,
            -32600,
            "Request exceeds 256 KiB",
        ))));
    }
    Ok(Some(
        serde_json::from_slice(&bytes).map_err(|_| error(Value::Null, -32700, "Invalid JSON")),
    ))
}

fn send_response(output: &mut impl Write, response: &Value) -> io::Result<()> {
    let encoded = serde_json::to_vec(response)?;
    let encoded = if encoded.len() > MAX_RESPONSE {
        serde_json::to_vec(&error(
            response["id"].clone(),
            -32603,
            "Response exceeds 1 MiB; request a smaller page",
        ))?
    } else {
        encoded
    };
    output.write_all(&encoded)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn goal_replay_identity_ignores_poll_wait_but_preserves_scope_target_and_providers() {
        let first = json!({"scope":"/fixture","targetAvailableBytes":1,"waitMs":0});
        assert_eq!(
            goal_replay_key(&first),
            goal_replay_key(
                &json!({"scope":"/fixture","targetAvailableBytes":1,"providers":["rust_target"],"waitMs":2000})
            )
        );
        for changed in [
            json!({"scope":"/other","targetAvailableBytes":1}),
            json!({"scope":"/fixture","targetAvailableBytes":2}),
            json!({"scope":"/fixture","targetAvailableBytes":1,"providers":["go_build_cache"]}),
        ] {
            assert_ne!(goal_replay_key(&first), goal_replay_key(&changed));
        }
        assert!(active_cleanup(&json!({"state":"running"})));
        for state in ["completed", "partial", "cancelled", "failed", "interrupted"] {
            assert!(!active_cleanup(&json!({"state":state})));
        }
    }

    #[test]
    fn application_tools_are_advanced_and_execution_is_capability_gated() {
        assert!(cleanup_tools(true)
            .iter()
            .all(|tool| tool["name"] != "search_applications"
                && tool["name"] != "execute_app_uninstall"));
        if cfg!(target_os = "macos") {
            assert!(tools(false)
                .iter()
                .any(|tool| tool["name"] == "search_applications"));
            assert!(tools(false)
                .iter()
                .all(|tool| tool["name"] != "execute_app_uninstall"));
            assert!(tools(true)
                .iter()
                .any(|tool| tool["name"] == "execute_app_uninstall"));
        }
    }
    #[test]
    fn schemas_reject_unknown_arguments_and_unbounded_pages() {
        let definitions = tools(false);
        let poll = definitions
            .iter()
            .find(|t| t["name"] == "scan_poll")
            .unwrap();
        assert!(validate_arguments(poll, &json!({"jobId":"one","limit":200})).is_ok());
        for args in [
            json!({"jobId":"one","limit":201}),
            json!({"jobId":"one","limit":0}),
            json!({"jobId":"one","offset":-1}),
            json!({"jobId":"one","command":"rm"}),
            json!({"jobId":null}),
        ] {
            assert!(validate_arguments(poll, &args).is_err());
        }
        assert!(definitions.iter().all(|t| t["name"] != "cleanup_execute"));
        assert!(tools(true).iter().any(|t| t["name"] == "cleanup_execute"));
    }
    #[test]
    fn compact_definitions_expose_only_goal_and_status_and_remain_bounded() {
        let definitions = cleanup_tools(true);
        assert_eq!(definitions.len(), 2);
        assert!(definitions.iter().any(|t| t["name"] == "reclaim_space"));
        assert!(definitions.iter().any(|t| t["name"] == "cleanup_status"));
        assert!(serde_json::to_vec(&definitions).unwrap().len() < 12 * 1024);
        let reclaim = &definitions[0];
        assert!(validate_arguments(reclaim,&json!({"scope":"/fixture","targetAvailableBytes":1,"providers":["rust_target"],"waitMs":2000})).is_ok());
        assert!(validate_arguments(
            reclaim,
            &json!({"scope":"/fixture","targetAvailableBytes":1,"providers":["unknown"]})
        )
        .is_err());
        assert!(validate_arguments(
            reclaim,
            &json!({"scope":"/fixture","targetAvailableBytes":1,"waitMs":2001})
        )
        .is_err());
        assert_eq!(cleanup_tools(false).len(), 1);
    }
    #[test]
    fn optional_history_freshness_never_relabels_completed_space_execution() {
        let completion =
            json!({"spaceGoalVerified":true,"cleanupSettled":true,"nextAction":"report_result"});
        for state in ["pending", "deferred", "running", "failed", "complete"] {
            let value = history_purpose(
                json!({"completion":completion,"verification":{"verificationId":"verify-owned","state":state,"fresh":false}}),
            );
            assert_eq!(value["completion"], completion);
            assert_eq!(value["verification"]["purpose"], "optional_history_refresh");
            assert_eq!(value["verification"]["requiredForCleanupCompletion"], false);
            assert_eq!(
                value["verification"]["freshAppliesTo"],
                "history_generation"
            );
        }
    }
    #[test]
    fn escaped_final_envelope_keeps_completion_and_critical_diagnostics() {
        let completion =
            json!({"spaceGoalVerified":true,"cleanupSettled":false,"nextAction":"review_recovery"});
        let error = "\"\\\n😀".repeat(45);
        let mut value = history_purpose(
            json!({"operationId":"0123456789abcdef0123456789abcdef","state":"partial","phase":"done","goalReached":true,"availableBytes":10,"targetAvailableBytes":1,"completion":completion,"recovery":{"files":2,"operationCount":1,"journalDirectory":"x".repeat(9000)},"durabilityIncomplete":true,"operationInterrupted":true,"cancelRequested":true,"deadlineExceeded":true,"actualSpaceError":"same scope unavailable","journalError":"x".repeat(9000),"errors":vec![error;8],"errorsCount":11,"errorsTruncated":true,"diagnostics":{"candidateRetentionEvents":4,"operationIssueEvents":7,"samples":[{"kind":"operation_issue","code":"execution_interrupted","errorIndex":0}],"detailOperationId":"0123456789abcdef0123456789abcdef"},"verification":{"verificationId":"verify-owned","state":"deferred","fresh":false,"reason":"x".repeat(9000)}}),
        );
        value["effects"] = json!({"available":true,"childCount":36,"settledChildCount":36,"actionCount":36,"deletedFiles":36,"deletedAllocatedBytes":36*4096,"coordinationFilesCreated":36,"recoveryFiles":2,"totalsComplete":false,"detailsDurable":false,"samples":[{"childOperationId":"a".repeat(32),"provider":"rust_target","artifactScopeDisplay":"😀\"\\\n".repeat(3000),"deletedFiles":1}],"truncated":true,"detail":{"operationId":"0123456789abcdef0123456789abcdef","receiptFile":"0123456789abcdef0123456789abcdef.json","field":"childDetails","allChildren":false}});
        let phase = json!({"gitInspected":35,"noMarker":0,"unavailable":1,"notReached":0,"retained":{"hardLinks":2,"symlinks":3,"sourceBoundaries":1,"otherRetentions":2,"gitTrackedEntries":0}});
        value["protectionEvidence"] = json!({"scope":"settled_dispatched_children","state":"incomplete","available":true,"incomplete":true,"detailsDurable":false,"childCount":36,"settledChildCount":36,"missingChildEvidence":0,"plan":phase,"execution":phase,"revalidation":{"attempts":3,"metadataBindingChecks":3,"successfulReloads":0,"failures":1},"samples":[{"childOperationId":"a".repeat(32),"executionGit":{"state":"repository_inspected","display":"😀\"\\\n".repeat(3000)}}],"sampleLimit":2,"truncated":true,"detail":{"operationId":"0123456789abcdef0123456789abcdef","field":"childDetails.protectionEvidence"}});
        let result = compact_goal_result(value);
        assert!(
            result.to_string().len() <= 8192,
            "{}",
            result.to_string().len()
        );
        let receipt: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(receipt["effects"]["actionCount"], 36);
        assert_eq!(receipt["effects"]["coordinationFilesCreated"], 36);
        assert_eq!(receipt["effects"]["detailsDurable"], false);
        assert_eq!(receipt["effects"]["truncated"], true);
        assert_eq!(receipt["effects"]["detail"]["field"], "childDetails");
        assert_eq!(
            receipt["protectionEvidence"]["execution"]["gitInspected"],
            35
        );
        assert_eq!(
            receipt["protectionEvidence"]["execution"]["retained"]["otherRetentions"],
            2
        );
        assert_eq!(receipt["protectionEvidence"]["incomplete"], true);
        assert_eq!(receipt["protectionEvidence"]["detailsDurable"], false);
        assert_eq!(receipt["protectionEvidence"]["truncated"], true);
        assert_eq!(
            receipt["protectionEvidence"]["detail"]["field"],
            "childDetails.protectionEvidence"
        );
        assert_eq!(receipt["completion"], completion);
        assert_eq!(receipt["recovery"]["files"], 2);
        assert_eq!(receipt["errorsCount"], 11);
        assert_eq!(receipt["diagnostics"]["operationIssueEvents"], 7);
        for key in [
            "durabilityIncomplete",
            "operationInterrupted",
            "cancelRequested",
            "deadlineExceeded",
        ] {
            assert_eq!(receipt[key], true, "{key}");
        }
        assert!(receipt.get("actualSpaceError").is_some());
        assert!(receipt.get("journalError").is_some());
        assert_eq!(
            receipt["verification"]["purpose"],
            "optional_history_refresh"
        );
        assert_eq!(
            receipt["verification"]["requiredForCleanupCompletion"],
            false
        );
    }

    #[test]
    fn background_verification_preserves_goal_and_requires_a_safe_publication_reserve() {
        let goal = 320 * 1024 * 1024u64;
        assert!(!verification_headroom_safe(
            &json!({"availableBytes":goal+16*1024*1024,"targetAvailableBytes":goal})
        ));
        assert!(verification_headroom_safe(
            &json!({"availableBytes":goal+VERIFICATION_RESERVE,"targetAvailableBytes":goal})
        ));
        assert!(!verification_headroom_safe(
            &json!({"availableBytes":goal+VERIFICATION_RESERVE,"targetAvailableBytes":goal,"durabilityIncomplete":true})
        ));
    }
}
