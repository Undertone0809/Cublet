use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

fn parameter<T: serde::de::DeserializeOwned>(params: &Value, name: &str) -> Result<T, String> {
    params
        .get(name)
        .cloned()
        .ok_or_else(|| format!("Missing RPC parameter: {name}"))
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| format!("Invalid RPC parameter {name}: {error}"))
        })
}

fn optional_parameter<T: serde::de::DeserializeOwned>(
    params: &Value,
    name: &str,
) -> Result<Option<T>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| format!("Invalid RPC parameter {name}: {error}")),
    }
}

fn encode<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| format!("Failed to encode RPC result: {error}"))
}

pub fn dispatch(method: &str, params: Value) -> Result<Value, String> {
    match method {
        "get_agent_activity" => encode(crate::agent_activity::get_agent_activity()?),
        "get_agent_scan_history" => encode(crate::macdisk::history::agent_history(
            optional_parameter::<String>(&params, "scopeId")?.as_deref(),
        )?),
        "get_cleanup_disposition" => encode(crate::cleanup_disposition::get()?),
        "set_cleanup_disposition" => encode(crate::cleanup_disposition::set(parameter(
            &params, "mode",
        )?)?),
        "seal_application_item" => encode(crate::application_disposal::seal(parameter(
            &params, "stamps",
        )?)?),
        "dispose_application_item" => {
            crate::application_disposal::dispose(
                parameter(&params, "mode")?,
                parameter(&params, "stamps")?,
                optional_parameter(&params, "seal")?,
            )?;
            Ok(Value::Null)
        }
        "disk_overview" => encode(crate::macdisk::disk_overview()?),
        "get_disk_cache_settings" => encode(crate::macdisk::get_disk_cache_settings()?),
        "set_disk_cache_limit" => encode(crate::macdisk::set_disk_cache_limit(parameter(
            &params, "limitMiB",
        )?)?),
        "start_disk_scan" => {
            let path = optional_parameter(&params, "path")?;
            let refresh = optional_parameter(&params, "refresh")?.unwrap_or(false);
            let allow_protected = optional_parameter(&params, "allowProtected")?.unwrap_or(false);
            encode(crate::macdisk::start_disk_scan_with_mode(
                path,
                refresh,
                allow_protected,
                optional_parameter(&params, "mode")?,
            )?)
        }
        "get_cached_disk_scan" => encode(crate::macdisk::get_cached_disk_scan(
            optional_parameter(&params, "path")?,
            optional_parameter(&params, "allowProtected")?.unwrap_or(false),
            optional_parameter(&params, "retainScanId")?,
        )?),
        "get_disk_scan" => encode(crate::macdisk::get_disk_scan(parameter(
            &params, "scanId",
        )?)?),
        "list_disk_scan_history" => encode(crate::macdisk::list_disk_scan_history(parameter(
            &params, "scanId",
        )?)?),
        "get_disk_scan_comparison" => {
            encode(crate::macdisk::get_disk_scan_comparison_with_baseline(
                parameter(&params, "scanId")?,
                optional_parameter(&params, "path")?,
                optional_parameter(&params, "baselineId")?,
            )?)
        }
        "get_disk_scan_view" => encode(crate::macdisk::get_disk_scan_view(
            parameter(&params, "scanId")?,
            parameter(&params, "path")?,
        )?),
        "inspect_disk_directory" => encode(crate::macdisk::inspect_disk_directory(
            parameter(&params, "scanId")?,
            parameter(&params, "path")?,
        )?),
        "list_disk_directory" => encode(crate::macdisk::list_disk_directory(
            parameter(&params, "scanId")?,
            parameter(&params, "path")?,
            optional_parameter(&params, "afterPath")?,
            optional_parameter(&params, "limit")?,
        )?),
        "cancel_disk_scan" => encode(crate::macdisk::cancel_disk_scan(parameter(
            &params, "scanId",
        )?)?),
        "scan_storage_fast" => encode(crate::scanner::scan_storage_fast()?),
        "restore_live_cleanup_operation" => encode(crate::scanner::restore_live_cleanup_operation(
            parameter(&params, "operationId")?,
        )?),
        "list_live_cleanup_operations" => encode(crate::scanner::list_live_cleanup_operations()?),
        "purge_live_cleanup_operation" => encode(crate::scanner::purge_live_cleanup_operation(
            parameter(&params, "operationId")?,
            parameter(&params, "confirmation")?,
        )?),
        "scan_storage" => encode(crate::scanner::scan_storage()?),
        "list_worktrees" => encode(crate::worktrees::list_worktrees()?),
        "remove_worktree" => {
            let worktree = parameter(&params, "worktree")?;
            let confirmed = parameter(&params, "confirmed")?;
            crate::worktrees::remove_worktree(worktree, confirmed)?;
            encode(())
        }
        "remove_worktrees" => {
            let worktrees = parameter(&params, "worktrees")?;
            let confirmed = parameter(&params, "confirmed")?;
            encode(crate::worktrees::remove_worktrees(worktrees, confirmed)?)
        }
        "get_codex_status" => encode(crate::scanner::get_codex_status()?),
        "quit_codex" => encode(crate::scanner::quit_codex()?),
        "create_cleanup_plan" => {
            let thread_ids = parameter(&params, "threadIds")?;
            encode(crate::scanner::create_cleanup_plan(thread_ids)?)
        }
        "execute_cleanup_plan" => {
            let plan = parameter(&params, "plan")?;
            encode(crate::scanner::execute_cleanup_plan(plan)?)
        }
        "restore_cleanup_operation" => {
            let operation_id = parameter(&params, "operationId")?;
            encode(crate::scanner::restore_cleanup_operation(operation_id)?)
        }
        "create_compaction_plan" => encode(crate::scanner::create_compaction_plan()?),
        "execute_compaction_plan" => {
            let plan = parameter(&params, "plan")?;
            encode(crate::scanner::execute_compaction_plan(plan)?)
        }
        "restore_compaction_operation" => {
            let operation_id = parameter(&params, "operationId")?;
            encode(crate::scanner::restore_compaction_operation(operation_id)?)
        }
        "list_cleanup_operations" => encode(crate::scanner::list_cleanup_operations()?),
        "purge_recovery" => {
            let operation_id = parameter(&params, "operationId")?;
            let confirmation = parameter(&params, "confirmation")?;
            encode(crate::scanner::purge_recovery(operation_id, confirmation)?)
        }
        "create_managed_cleanup_plan" => {
            let category_id = parameter(&params, "categoryId")?;
            let item_ids = optional_parameter(&params, "itemIds")?;
            encode(crate::scanner::create_managed_cleanup_plan(
                category_id,
                item_ids,
            )?)
        }
        "execute_managed_cleanup_plan" => {
            let plan = parameter(&params, "plan")?;
            encode(crate::scanner::execute_managed_cleanup_plan(plan)?)
        }
        "restore_managed_cleanup_operation" => {
            let operation_id = parameter(&params, "operationId")?;
            encode(crate::scanner::restore_managed_cleanup_operation(
                operation_id,
            )?)
        }
        _ => Err(format!("Unknown RPC method: {method}")),
    }
}

fn response(id: Value, result: Result<Value, String>) -> Value {
    match result {
        Ok(result) => json!({ "id": id, "ok": true, "result": result }),
        Err(error) => json!({ "id": id, "ok": false, "error": error }),
    }
}

pub fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("Failed to read RPC request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<RpcRequest>(&line);
        let (id, result) = match parsed {
            Ok(request) => {
                let id = request.id.clone();
                let result = dispatch(&request.method, request.params);
                (id, result)
            }
            Err(error) => (Value::Null, Err(format!("Invalid RPC request: {error}"))),
        };
        serde_json::to_writer(&mut stdout, &response(id, result))
            .map_err(|error| format!("Failed to write RPC response: {error}"))?;
        stdout
            .write_all(b"\n")
            .map_err(|error| format!("Failed to flush RPC response: {error}"))?;
        stdout
            .flush()
            .map_err(|error| format!("Failed to flush RPC response: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::dispatch;
    use serde_json::json;

    #[test]
    fn unknown_method_returns_a_structured_error() {
        let error =
            dispatch("does_not_exist", json!({})).expect_err("unknown methods must fail closed");
        assert!(error.contains("Unknown RPC method"));
    }

    #[test]
    fn missing_parameters_are_rejected_before_core_execution() {
        let error =
            dispatch("create_cleanup_plan", json!({})).expect_err("thread IDs are required");
        assert!(error.contains("Missing RPC parameter: threadIds"));
    }
}
