//! Goal orchestration reuses the exact-plan engine; history only orders discovery.
use super::*;
use std::collections::{HashSet, VecDeque};

const DISCOVERY_ENTRIES: u64 = 50_000;
const DISCOVERY_DIRECTORIES: u64 = 4096;
const DISCOVERY_DEPTH: usize = 12;
const MAX_CANDIDATES: usize = 64;
const CARGO_CHILD_PROBES: u64 = DISCOVERY_DIRECTORIES;

#[derive(Default)]
struct CargoChildProbes {
    attempted: u64,
    hits: u64,
    skipped: u64,
}
impl CargoChildProbes {
    fn record(&self, receipt: &mut Value) {
        receipt["coverage"]["cargoChildProbes"] = json!(self.attempted);
        receipt["coverage"]["cargoChildProbeLimit"] = json!(CARGO_CHILD_PROBES);
        receipt["coverage"]["cargoChildProbeHits"] = json!(self.hits);
        receipt["coverage"]["cargoChildProbesSkipped"] = json!(self.skipped);
        receipt["coverage"]["cargoChildProbeBudgetExhausted"] =
            json!(self.attempted >= CARGO_CHILD_PROBES);
        receipt["coverage"]["discoveryDepthLimit"] = json!(DISCOVERY_DEPTH);
        receipt["coverage"]["discoveryEntryLimit"] = json!(DISCOVERY_ENTRIES);
        receipt["coverage"]["discoveryDirectoryLimit"] = json!(DISCOVERY_DIRECTORIES);
        receipt["coverage"]["discoveryCandidateLimit"] = json!(MAX_CANDIDATES);
    }
    fn probe(
        &mut self,
        parent: &File,
        name: &OsStr,
        path: &Path,
        device: u64,
        cancel: &AtomicBool,
        started: Instant,
    ) -> Result<Option<PathBuf>, String> {
        check_budget(cancel, started, EXECUTE_SECONDS)?;
        if self.attempted >= CARGO_CHILD_PROBES {
            self.skipped += 1;
            return Ok(None);
        }
        self.attempted += 1;
        // Fixed metadata only. The child remains on the normal queue whether
        // this optional scheduling clue succeeds or fails.
        let candidate = (|| -> Option<PathBuf> {
            let child = open_child(parent, name).ok()?;
            let metadata = child.metadata().ok()?;
            if metadata.dev() != device || placeholder(&metadata) {
                return None;
            }
            let manifest = stat_child(&child, OsStr::new("Cargo.toml")).ok()?;
            if manifest.st_mode & libc::S_IFMT != libc::S_IFREG {
                return None;
            }
            let target = open_child(&child, OsStr::new("target")).ok()?;
            let metadata = target.metadata().ok()?;
            if metadata.dev() != device || placeholder(&metadata) {
                return None;
            }
            let path = path.join("target");
            validate_kind("rust_target", &path, &target).ok()?;
            Some(path)
        })();
        check_budget(cancel, started, EXECUTE_SECONDS)?;
        if candidate.is_some() {
            self.hits += 1;
        }
        Ok(candidate)
    }
}
fn discovery_depth(scope: &Path, path: &Path) -> Option<usize> {
    let relative = path.strip_prefix(scope).ok()?;
    relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then(|| relative.components().count())
}

impl AgentCleanup {
    /// The caller supplies the already-authorized scope. Capability is checked
    /// here; no historical record or supplied plan can grant mutation authority.
    pub fn reclaim_with_before<F, G>(&self, args: &Value, before: F) -> Result<Value, String>
    where
        F: FnOnce() -> Result<G, String> + Send + 'static,
        G: Send + 'static,
    {
        if !self.inner.allow_cleanup {
            return Err("Start with --allow-cleanup to execute an authorized goal".into());
        }
        let scope = PathBuf::from(
            args["scope"]
                .as_str()
                .ok_or("Explicit absolute scope is required")?,
        );
        let target = args["targetAvailableBytes"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("Positive targetAvailableBytes is required")?;
        let mut providers: Vec<String> = match args.get("providers") {
            None => vec!["rust_target".into()],
            Some(value) => value
                .as_array()
                .ok_or("providers must be an array")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("Invalid provider".to_string())
                })
                .collect::<Result<_, _>>()?,
        };
        providers.sort();
        providers.dedup();
        if providers.is_empty()
            || providers
                .iter()
                .any(|p| !matches!(p.as_str(), "rust_target" | "go_build_cache"))
        {
            return Err(
                "Only explicitly authorized rust_target/go_build_cache providers are supported"
                    .into(),
            );
        }
        let wait = args["waitMs"].as_u64().unwrap_or(2000);
        if wait > 2000 {
            return Err("waitMs must be at most2000".into());
        }
        // Cheap lexical checks on request thread. Native validation and volume
        // queries occur on the worker, so unavailable mounts cannot stall RPC.
        if !scope.is_absolute()
            || scope
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
        {
            return Err("Explicit absolute scope without traversal is required".into());
        }
        let key = format!("{}:{target}:{}", scope.display(), providers.join(","));
        let mut goals = self
            .inner
            .goals
            .lock()
            .map_err(|_| "Goal registry unavailable")?;
        if let Some(existing) = goals.get(&key) {
            if self.status(existing).is_ok_and(|v| !terminal(&v)) {
                let id = existing.clone();
                drop(goals);
                return self.goal_status(&id, wait, false);
            }
        }
        let operation = id()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let receipt = json!({"jobId":operation,"operationId":operation,"operation":"reclaim","state":"running","phase":"discovering","scope":scope,"path":scope,"targetAvailableBytes":target,"availableBytes":null,"goalReached":false,"deletedFiles":0,"deletedAllocatedBytes":0,"deletedLogicalBytes":0,"coverage":{"freshPlans":true,"discoveryComplete":false,"protectedCandidates":0,"unknownCandidates":0},"cacheEvidence":{"hit":false,"requestedScope":scope,"hintsUsed":0,"policy":"historical ordering only; exact fresh plans"},"childDetails":[],"errors":[],"errorsCount":0,"durabilityIncomplete":false,"operationInterrupted":false,"cancelRequested":false,"deadlineExceeded":false,"spaceReadbackVerified":false,"mutationSettled":false,"recovery":{"operationIds":[]},"startedAtMs":now_ms()});
        let mut receipt = receipt;
        CargoChildProbes::default().record(&mut receipt);
        self.insert(&operation, receipt, cancel.clone())?;
        goals.retain(|_, id| self.status(id).is_ok_and(|v| !terminal(&v)));
        goals.insert(key, operation.clone());
        drop(goals);
        let service = self.clone();
        let worker_id = operation.clone();
        std::thread::spawn(move || {
            service.run_goal(&worker_id, scope, target, providers, &cancel, before)
        });
        self.goal_status(&operation, wait, false)
    }
    pub fn goal_status(
        &self,
        operation: &str,
        wait_ms: u64,
        cancel: bool,
    ) -> Result<Value, String> {
        if wait_ms > 2000 {
            return Err("waitMs must be at most2000".into());
        }
        let mut value = if cancel {
            self.cancel(operation)?
        } else {
            self.status(operation)?
        };
        if value["operation"] != "reclaim" {
            return Err("operationId does not identify a goal operation".into());
        }
        // Detail references bind the server registry key, never a journal-supplied path.
        value["operationId"] = json!(operation);
        let start = Instant::now();
        while (!terminal(&value) || value["mutationSettled"] == false)
            && start.elapsed() < Duration::from_millis(wait_ms)
        {
            std::thread::sleep(Duration::from_millis(10));
            value = self.status(operation)?;
        }
        value["operationId"] = json!(operation);
        if terminal(&value) {
            refresh_terminal_space(&mut value);
        }
        completion_contract(&mut value);
        // Detailed file identities stay in server-side execution manifests.
        value
            .as_object_mut()
            .ok_or("Invalid goal receipt")?
            .remove("jobId");
        value.as_object_mut().unwrap().remove("path");
        value.as_object_mut().unwrap().remove("ownerSession");
        value.as_object_mut().unwrap().remove("childDetails");
        if encoded_receipt_size(&value) > 8064 {
            value = budget_receipt(&value);
        }
        Ok(value)
    }
    pub(crate) fn budget_goal_receipt(value: &Value) -> Value {
        budget_receipt(value)
    }
    fn publish_goal(&self, operation: &str, receipt: &mut Value) {
        completion_contract(receipt);
        self.publish(operation, receipt.clone());
        if let Ok(current) = self.status(operation) {
            if let Some(error) = current.get("journalError") {
                receipt["journalError"] = error.clone();
                receipt["durabilityIncomplete"] = json!(true);
            }
        }
    }
    fn run_goal<F, G>(
        &self,
        operation: &str,
        scope: PathBuf,
        target: u64,
        providers: Vec<String>,
        cancel: &AtomicBool,
        before: F,
    ) where
        F: FnOnce() -> Result<G, String>,
    {
        let started = Instant::now();
        let mut receipt = self.status(operation).unwrap();
        receipt["recovery"]["journalDirectory"] = json!(self.inner.journal);
        let mut candidate_ids = Vec::new();
        let mut errors = Errors::default();
        let mut protected = 0u64;
        let mut unknown = 0u64;
        let mut examined = 0u64;
        let mut directories = 0u64;
        let mut deleted = 0u64;
        let mut allocated = 0u64;
        let mut logical = 0u64;
        let mut max_batch = 0u64;
        let mut recovery_files = 0u64;
        let mut mutation = None;
        let mut coordination = None;
        let mut limit = None;
        let mut probes = CargoChildProbes::default();
        let result = (|| -> Result<(), String> {
            let (scope, handle) = safe_root(&scope)?;
            let metadata = handle.metadata().map_err(|e| e.to_string())?;
            let (available, total) = volume_available(&handle)?;
            if target > total {
                return Err("Requested goal exceeds this volume's total capacity".into());
            }
            receipt["availableBytes"] = json!(available);
            receipt["initialAvailableBytes"] = json!(available);
            receipt["volumeDevice"] = json!(metadata.dev());
            receipt["scopeInode"] = json!(metadata.ino());
            let goal = GoalLimit {
                scope: scope.clone(),
                identity: Identity::metadata(&metadata),
                target,
            };
            limit = Some(goal.clone());
            if available >= target {
                receipt["goalReached"] = json!(true);
                receipt["noOp"] = json!(true);
                return Ok(());
            }
            mutation = Some(self.execution_lock(cancel, started)?);
            coordination = Some(before()?);
            check_budget(cancel, started, EXECUTE_SECONDS)?;
            // Same volume and scope are revalidated after the queue/barrier wait.
            receipt["availableBytes"] = json!(goal.available()?);
            let mut queue = VecDeque::new();
            let mut seen = HashSet::new();
            let hints = historical_hints(&scope, &mut receipt);
            for path in hints.into_iter().chain(std::iter::once(scope.clone())) {
                if let Some(depth) = discovery_depth(&scope, &path) {
                    queue.push_back((path, depth));
                }
            }
            let mut candidates = 0usize;
            let mut stopped_at_goal = false;
            while let Some((path, depth)) = queue.pop_front() {
                check_budget(cancel, started, EXECUTE_SECONDS)?;
                let available = goal.available()?;
                receipt["availableBytes"] = json!(available);
                if available >= target {
                    receipt["goalReached"] = json!(true);
                    stopped_at_goal = true;
                    break;
                }
                if !seen.insert(path.clone()) {
                    continue;
                }
                if !path.starts_with(&scope)
                    || cloud_path(&path)
                    || service_path(&self.inner.journal, &path, &scope)
                {
                    unknown += 1;
                    continue;
                }
                if depth > DISCOVERY_DEPTH {
                    return Err("Bounded discovery incomplete; unvisited paths retained".into());
                }
                let (_, dir) = match safe_root(&path) {
                    Ok(v) => v,
                    Err(e) => {
                        unknown += 1;
                        push_scoped_issue(
                            &mut errors,
                            IssueKind::CandidateRetention,
                            "candidate_unavailable",
                            &e,
                            &path,
                        );
                        continue;
                    }
                };
                if dir.metadata().map_err(|e| e.to_string())?.dev() != goal.identity.dev {
                    unknown += 1;
                    continue;
                }
                let provider = providers
                    .iter()
                    .find(|kind| validate_kind(kind, &path, &dir).is_ok());
                if let Some(kind) = provider {
                    candidates += 1;
                    if candidates > MAX_CANDIDATES {
                        return Err("Candidate budget reached; remaining scopes retained".into());
                    }
                    receipt["phase"] = json!("cleaning");
                    self.publish_goal(operation, &mut receipt);
                    let plan_id = id()?;
                    let plan =
                        match self.build_plan_context(&plan_id, kind, &path, cancel, Some(started))
                        {
                            Ok(p) => p,
                            Err(e) => {
                                unknown += 1;
                                push_scoped_issue(
                                    &mut errors,
                                    IssueKind::CandidateRetention,
                                    "candidate_plan_unavailable",
                                    &e,
                                    &path,
                                );
                                continue;
                            }
                        };
                    if plan.entries.is_empty() {
                        receipt["coverage"]["gitTrackedFiles"] = json!(
                            receipt["coverage"]["gitTrackedFiles"].as_u64().unwrap_or(0)
                                + plan.git_tracked_files
                        );
                        unknown += plan.skipped;
                        continue;
                    }
                    let child = id()?;
                    self.insert(&child,json!({"jobId":child,"planId":plan.id,"parentOperationId":operation,"operation":"execute","state":"running","path":plan.root,"startedAtMs":now_ms()}),Arc::new(AtomicBool::new(false)))?;
                    candidate_ids.push(child.clone());
                    start_child_detail(&mut receipt, &child, kind, &plan.root);
                    // The existing root receipt retains every dispatched child,
                    // independently of the compact last16 recovery ID sample.
                    self.publish_goal(operation, &mut receipt);
                    self.run_execution_context(
                        &child,
                        &plan,
                        cancel,
                        || Ok(()),
                        ExecutionContext {
                            started: Some(started),
                            locks_held: true,
                            authorization_scope: Some(scope.clone()),
                            parent_goal: Some(operation.to_owned()),
                            goal: Some(goal.clone()),
                        },
                    );
                    let outcome = self.status(&child)?;
                    settle_child_detail(&mut receipt, &child, &outcome);
                    if outcome["coordinationDetailsComplete"] == false {
                        push_scoped_issue(
                            &mut errors,
                            IssueKind::Operation,
                            "coordination_effect_evidence_incomplete",
                            outcome["coordinationEvidenceError"]
                                .as_str()
                                .unwrap_or("Created coordination identity evidence is incomplete"),
                            &plan.root,
                        );
                    }
                    receipt["coverage"]["coordinationLockFiles"] = json!(
                        receipt["coverage"]["coordinationLockFiles"]
                            .as_u64()
                            .unwrap_or(0)
                            + outcome["coordinationLockFiles"].as_u64().unwrap_or(0)
                    );
                    deleted += outcome["deletedFiles"].as_u64().unwrap_or(0);
                    allocated += outcome["deletedAllocatedBytes"].as_u64().unwrap_or(0);
                    logical += outcome["deletedLogicalBytes"].as_u64().unwrap_or(0);
                    recovery_files += outcome["recoveryFiles"].as_u64().unwrap_or(0);
                    receipt["largestDeletedFileAllocatedBytes"] = json!(receipt
                        ["largestDeletedFileAllocatedBytes"]
                        .as_u64()
                        .unwrap_or(0)
                        .max(
                            outcome["largestDeletedFileAllocatedBytes"]
                                .as_u64()
                                .unwrap_or(0)
                        ));
                    max_batch =
                        max_batch.max(outcome["maxBatchAllocatedBytes"].as_u64().unwrap_or(0));
                    let tracked_files = outcome["gitTrackedFiles"].as_u64().unwrap_or(0);
                    receipt["coverage"]["gitTrackedFiles"] = json!(
                        receipt["coverage"]["gitTrackedFiles"].as_u64().unwrap_or(0)
                            + tracked_files
                    );
                    unknown += outcome["retainedEntries"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(tracked_files.saturating_sub(plan.git_tracked_files));
                    if let Some(error) = outcome["journalError"].as_str() {
                        receipt["durabilityIncomplete"] = json!(true);
                        push_scoped_issue(
                            &mut errors,
                            IssueKind::Operation,
                            "execution_journal_incomplete",
                            &format!("Execution {child} journal incomplete: {error}"),
                            &plan.root,
                        );
                    }
                    if let Some(error) = outcome["error"].as_str() {
                        if outcome["failureDisposition"] == "observed_active" {
                            protected += 1;
                        } else {
                            unknown += 1;
                        }
                        let (kind, code) = child_issue(&outcome);
                        push_scoped_issue(&mut errors, kind, code, error, &plan.root);
                    }
                    update_goal(
                        &mut receipt,
                        deleted,
                        allocated,
                        logical,
                        protected,
                        unknown,
                        examined,
                        directories,
                        &candidate_ids,
                        &errors,
                    );
                    self.publish_goal(operation, &mut receipt);
                    if recovery_files > 0 {
                        return Err(
                            "Recovery files retained; review durable evidence before more mutation"
                                .into(),
                        );
                    }
                    continue;
                }
                directories += 1;
                if directories > DISCOVERY_DIRECTORIES {
                    return Err("Bounded discovery incomplete; unvisited paths retained".into());
                }
                // Fresh project provenance promotes only its conventional target,
                // before unrelated broad directory queues can consume the budget.
                let rust_enabled = providers.iter().any(|p| p == "rust_target");
                let mut direct_target = None;
                if rust_enabled && depth < DISCOVERY_DEPTH {
                    let candidate = path.join("target");
                    if let Ok((_, target)) = safe_root(&candidate) {
                        if target
                            .metadata()
                            .is_ok_and(|metadata| metadata.dev() == goal.identity.dev)
                            && !service_path(&self.inner.journal, &candidate, &scope)
                            && validate_kind("rust_target", &candidate, &target).is_ok()
                        {
                            direct_target = Some(candidate);
                        }
                    }
                }
                let mut children = Vec::new();
                let mut priority_children = Vec::new();
                for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
                    check_budget(cancel, started, EXECUTE_SECONDS)?;
                    examined += 1;
                    if examined > DISCOVERY_ENTRIES {
                        return Err(
                            "Discovery entry budget reached; remaining paths retained".into()
                        );
                    }
                    let entry = match entry {
                        Ok(v) => v,
                        Err(_) => {
                            unknown += 1;
                            continue;
                        }
                    };
                    let name = entry.file_name();
                    if name.to_str().is_none()
                        || matches!(name.to_str(), Some(".git" | ".svn" | ".hg"))
                    {
                        unknown += 1;
                        continue;
                    }
                    let file_type = match entry.file_type() {
                        Ok(v) => v,
                        Err(_) => {
                            unknown += 1;
                            continue;
                        }
                    };
                    if file_type.is_symlink() {
                        unknown += 1;
                    } else if file_type.is_dir() {
                        let child = entry.path();
                        if rust_enabled
                            && depth + 2 <= DISCOVERY_DEPTH
                            && !cloud_path(&child)
                            && !service_path(&self.inner.journal, &child, &scope)
                            && !service_path(&self.inner.journal, &child.join("target"), &scope)
                        {
                            if let Some(target) = probes.probe(
                                &dir,
                                &name,
                                &child,
                                goal.identity.dev,
                                cancel,
                                started,
                            )? {
                                priority_children.push(target);
                            }
                        }
                        children.push(child);
                    }
                }
                children.sort();
                for child in children {
                    queue.push_back((child, depth + 1));
                }
                priority_children.sort();
                for target in priority_children.into_iter().rev() {
                    queue.push_front((target, depth + 2));
                }
                if let Some(target) = direct_target {
                    queue.push_front((target, depth + 1));
                }
                probes.record(&mut receipt);
            }
            receipt["coverage"]["discoveryComplete"] = json!(queue.is_empty() && !stopped_at_goal);
            receipt["coverage"]["stoppedAtGoal"] = json!(stopped_at_goal);
            let available = goal.available()?;
            receipt["availableBytes"] = json!(available);
            receipt["goalReached"] = json!(available >= target);
            Ok(())
        })();
        probes.record(&mut receipt);
        receipt["phase"] = json!("verifying");
        self.publish_goal(operation, &mut receipt);
        if let Some(goal) = limit {
            match goal.available() {
                Ok(available) => {
                    receipt["availableBytes"] = json!(available);
                    receipt["goalReached"] = json!(available >= target);
                }
                Err(e) => {
                    push_issue(
                        &mut errors,
                        IssueKind::Operation,
                        "space_measurement_failed",
                        &e,
                    );
                    receipt["goalReached"] = json!(false);
                }
            }
        }
        update_goal(
            &mut receipt,
            deleted,
            allocated,
            logical,
            protected,
            unknown,
            examined,
            directories,
            &candidate_ids,
            &errors,
        );
        receipt["recovery"]["journalDirectory"] = json!(self.inner.journal);
        receipt["recovery"]["files"] = json!(recovery_files);
        receipt["maxBatchAllocatedBytes"] = json!(max_batch);
        receipt["batchAllocatedLimitBytes"] = json!(BATCH_ALLOCATED_BYTES);
        receipt["batchFileLimit"] = json!(BATCH);
        receipt["observedHeadroomAboveTargetBytes"] = json!(receipt["availableBytes"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(target));
        receipt["observedAvailableBytesDelta"] = match (
            receipt["initialAvailableBytes"].as_u64(),
            receipt["availableBytes"].as_u64(),
        ) {
            (Some(a), Some(b)) => json!(b as i128 - a as i128),
            _ => Value::Null,
        };
        receipt["measurementNote"]=json!("Same-volume statvfs headroom; signed net change includes concurrent writes. Deleted allocation is separate. Stop is checked before exceeding256 planned files or16MiB allocated per batch; one atomic file may exceed the byte limit and its size is reported. Observed excess includes concurrent changes.");
        let cancelled = result.as_ref().is_err_and(|e| e == "cancelled");
        receipt["cancelRequested"] = json!(cancel.load(Ordering::Relaxed));
        receipt["deadlineExceeded"] =
            json!(started.elapsed() >= Duration::from_secs(EXECUTE_SECONDS));
        receipt["operationInterrupted"] = json!(
            result.is_err()
                || receipt["cancelRequested"] == true
                || receipt["deadlineExceeded"] == true
        );
        if let Err(error) = result {
            push_issue(
                &mut errors,
                IssueKind::Operation,
                "operation_interrupted",
                &error,
            );
            record_errors(&mut receipt, &errors);
        }
        receipt["state"] = json!(if cancelled {
            "cancelled"
        } else if receipt["goalReached"] == true && recovery_files == 0 {
            "completed"
        } else if deleted > 0 {
            "partial"
        } else {
            "failed"
        });
        receipt["phase"] = json!("done");
        receipt["completedAtMs"] = json!(now_ms());
        receipt["elapsedMs"] = json!(started.elapsed().as_millis());
        receipt["spaceSampleTiming"]=json!("before final durable publication; returned terminal receipts resample after publication");
        self.publish_goal(operation, &mut receipt);
        refresh_terminal_space(&mut receipt);
        drop(coordination);
        drop(mutation);
        receipt["mutationSettled"] = json!(true);
        receipt["cancelRequested"] = json!(cancel.load(Ordering::Relaxed));
        receipt["deadlineExceeded"] =
            json!(started.elapsed() >= Duration::from_secs(EXECUTE_SECONDS));
        if receipt["cancelRequested"] == true || receipt["deadlineExceeded"] == true {
            receipt["operationInterrupted"] = json!(true);
        }
        completion_contract(&mut receipt);
        if let Ok(mut registry) = self.inner.registry.lock() {
            if let Some(job) = registry.jobs.get_mut(operation) {
                job.value = receipt;
            }
        }
    }
}
#[derive(Clone, Copy)]
enum IssueKind {
    CandidateRetention,
    Operation,
}
impl IssueKind {
    fn name(self) -> &'static str {
        match self {
            Self::CandidateRetention => "candidate_retention",
            Self::Operation => "operation_issue",
        }
    }
}
#[derive(Default)]
struct Errors {
    samples: Vec<String>,
    kinds: Vec<Value>,
    total: u64,
    retained: u64,
    operation: u64,
    scoped: u64,
    scopes: Vec<Value>,
}
fn push_issue(errors: &mut Errors, kind: IssueKind, code: &'static str, error: &str) {
    errors.total += 1;
    match kind {
        IssueKind::CandidateRetention => errors.retained += 1,
        IssueKind::Operation => errors.operation += 1,
    }
    if errors.samples.len() < 8 {
        errors
            .kinds
            .push(json!({"kind":kind.name(),"code":code,"errorIndex":errors.samples.len()}));
        errors.samples.push(error.chars().take(180).collect());
    }
}
fn child_issue(outcome: &Value) -> (IssueKind, &'static str) {
    if outcome["deletedFiles"].as_u64() == Some(0) && outcome["recoveryFiles"].as_u64() == Some(0) {
        match outcome["failureDisposition"].as_str() {
            Some("observed_active") => return (IssueKind::CandidateRetention, "candidate_active"),
            Some("inspection_unavailable") => {
                return (
                    IssueKind::CandidateRetention,
                    "candidate_inspection_unavailable",
                )
            }
            _ => {}
        }
    }
    (IssueKind::Operation, "execution_interrupted")
}
fn scope_display(path: &Path, limit: usize) -> (String, bool) {
    let chars: Vec<_> = path.to_string_lossy().chars().collect();
    if chars.len() <= limit {
        return (chars.into_iter().collect(), false);
    }
    let prefix = limit / 3;
    let suffix = limit.saturating_sub(prefix + 1);
    (
        chars[..prefix]
            .iter()
            .chain(std::iter::once(&'…'))
            .chain(chars[chars.len() - suffix..].iter())
            .collect(),
        true,
    )
}
fn push_scoped_issue(
    errors: &mut Errors,
    kind: IssueKind,
    code: &'static str,
    error: &str,
    path: &Path,
) {
    let error_index = errors.total;
    push_issue(errors, kind, code, error);
    errors.scoped += 1;
    if error_index < 8 && errors.scopes.len() < 4 {
        let (display, truncated) = scope_display(path, 240);
        errors.scopes.push(
            json!({"errorIndex":error_index,"scopeDisplay":display,"scopeTruncated":truncated}),
        );
    }
}
fn record_errors(v: &mut Value, errors: &Errors) {
    v["errors"] = json!(errors.samples);
    v["errorsCount"] = json!(errors.total);
    v["errorsTruncated"] = json!(errors.total > errors.samples.len() as u64);
    v["diagnostics"] = json!({"candidateRetentionEvents":errors.retained,"operationIssueEvents":errors.operation,"samples":errors.kinds,"truncated":errors.total > errors.kinds.len() as u64,"detailOperationId":v["operationId"],"candidateScopes":{"count":errors.scoped,"samples":errors.scopes,"truncated":errors.scoped > errors.scopes.len() as u64,"sampleLimit":4}});
}

fn start_child_detail(receipt: &mut Value, child: &str, provider: &str, scope: &Path) {
    let parent = receipt["operationId"].clone();
    let rows = receipt["childDetails"]
        .as_array_mut()
        .expect("native child ledger");
    if rows.iter().any(|row| row["childOperationId"] == child) {
        return;
    }
    rows.push(json!({"childOperationId":child,"parentOperationId":parent,"provider":provider,"artifactScope":scope,"state":"running","settled":false}));
}
fn settle_child_detail(receipt: &mut Value, child: &str, outcome: &Value) {
    if let Some(row) = receipt["childDetails"]
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|row| row["childOperationId"] == child))
    {
        row["state"] = outcome["state"].clone();
        row["settled"] = json!(terminal(outcome));
        for field in [
            "deletedFiles",
            "deletedLogicalBytes",
            "deletedAllocatedBytes",
            "recoveryFiles",
            "coordinationLocksCreated",
            "coordinationLocksHeld",
        ] {
            row[field] = json!(outcome[field].as_u64().unwrap_or(0));
        }
        let bindings = outcome
            .get("coordinationCreatedBindings")
            .cloned()
            .unwrap_or(json!([]));
        let binding_count = bindings.as_array().map(Vec::len).unwrap_or(0) as u64;
        let binding_identities = bindings
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["identityRecorded"] == true));
        row["coordinationDetailsComplete"] = json!(
            outcome["coordinationDetailsComplete"] != false
                && binding_count == outcome["coordinationLocksCreated"].as_u64().unwrap_or(0)
                && binding_identities
        );
        row["coordinationCreatedBindings"] = bindings;
        if let Some(evidence) = outcome.get("protectionEvidence") {
            row["protectionEvidence"] = evidence.clone();
        }
        for field in [
            "processReferenceCoverage",
            "incomingReferenceCoverage",
            "successfulReferenceChecks",
            "referenceInspectionAttempts",
        ] {
            if let Some(evidence) = outcome.get(field) {
                row[field] = evidence.clone();
            }
        }
        if outcome["coordinationJournalPath"].is_string() {
            row["coordinationReceiptFile"] = json!(format!("{child}.coordination.json"));
        }
        if outcome.get("journalError").is_some() {
            row["journalError"] = json!(true);
        }
        if let Some(error) = outcome["error"].as_str() {
            row["error"] = json!(error.chars().take(180).collect::<String>());
            row["errorTruncated"] = json!(error.chars().count() > 180);
        }
        if let Some(disposition) = outcome.get("failureDisposition") {
            row["failureDisposition"] = disposition.clone();
        }
    }
}
fn protection_phase_summary() -> Value {
    json!({"gitInspected":0,"noMarker":0,"unavailable":0,"notReached":0,"retained":{"hardLinks":0,"symlinks":0,"symlinkReferents":0,"sourceBoundaries":0,"otherRetentions":0,"gitTrackedEntries":0}})
}
fn add_count(value: &mut Value, key: &str, amount: u64) {
    value[key] = json!(value[key].as_u64().unwrap_or(0).saturating_add(amount));
}
// Contribution to the actual text-content MCP envelope, including double
// escaping; not the length of the inner JSON object alone.
fn protection_wire_bytes(value: &Value) -> usize {
    encoded_receipt_size(&json!({"protectionEvidence":value})) - encoded_receipt_size(&json!({}))
}
fn compact_protection(mut value: Value, omit_samples: bool) -> Value {
    if omit_samples {
        // Known scalar fields only: neither private metadata nor unknown detail
        // can defeat the final receipt's reserved wire budget.
        let mut minimal = json!({"scope":"settled_dispatched_children","state":value["state"].as_str().filter(|s|matches!(*s,"not_reached"|"in_progress"|"incomplete"|"recorded")).unwrap_or("incomplete"),"samples":[],"sampleLimit":2,"truncated":value["truncated"]==true || value["settledChildCount"].as_u64().unwrap_or(0)>0,"detail":{"operationId":value["detail"]["operationId"].as_str().filter(|id|valid_id(id)).unwrap_or(""),"field":"childDetails.protectionEvidence"}});
        for key in ["available", "incomplete", "detailsDurable"] {
            minimal[key] = json!(value[key].as_bool());
        }
        for key in ["childCount", "settledChildCount", "missingChildEvidence"] {
            minimal[key] = json!(value[key].as_u64());
        }
        for phase in ["plan", "execution"] {
            for key in ["gitInspected", "noMarker", "unavailable", "notReached"] {
                minimal[phase][key] = json!(value[phase][key].as_u64());
            }
            for key in [
                "hardLinks",
                "symlinks",
                "symlinkReferents",
                "sourceBoundaries",
                "otherRetentions",
                "gitTrackedEntries",
            ] {
                minimal[phase]["retained"][key] = json!(value[phase]["retained"][key].as_u64());
            }
        }
        for key in [
            "attempts",
            "metadataBindingChecks",
            "successfulReloads",
            "failures",
        ] {
            minimal["revalidation"][key] = json!(value["revalidation"][key].as_u64());
        }
        minimal["references"]["scope"] = json!("current_user_argv_and_validated_roots");
        for key in ["freshChecks", "unavailableChildren", "unknownChildren"] {
            minimal["references"][key] = json!(value["references"][key].as_u64());
        }
        return minimal;
    }
    if omit_samples || protection_wire_bytes(&value) > 1536 {
        while value["samples"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
            && (omit_samples || protection_wire_bytes(&value) > 1536)
        {
            value["samples"].as_array_mut().unwrap().pop();
        }
    }
    value["truncated"] = json!(
        value["settledChildCount"].as_u64().unwrap_or(0)
            > value["samples"].as_array().map(Vec::len).unwrap_or(0) as u64
    );
    value
}
fn protection_summary(v: &Value) -> Value {
    let rows = v["childDetails"]
        .as_array()
        .filter(|rows| rows.len() <= MAX_CANDIDATES);
    let children = rows.map(Vec::len).unwrap_or(0);
    let mut result = json!({"scope":"settled_dispatched_children","state":"not_reached","available":false,"incomplete":rows.is_none(),"childCount":children,"settledChildCount":0,"missingChildEvidence":0,"plan":protection_phase_summary(),"execution":protection_phase_summary(),"revalidation":{"attempts":0,"metadataBindingChecks":0,"successfulReloads":0,"failures":0},"references":{"freshChecks":0,"unavailableChildren":0,"unknownChildren":0,"scope":"current_user_argv_and_validated_roots"},"samples":[],"sampleLimit":2,"truncated":false,"detail":{"operationId":v["operationId"],"field":"childDetails.protectionEvidence"}});
    let mut seen = HashSet::new();
    let mut settled = 0;
    let mut observed = 0;
    for row in rows.into_iter().flatten() {
        if !seen.insert(row["childOperationId"].as_str().unwrap_or("")) {
            continue;
        }
        if row["settled"] != true {
            continue;
        }
        settled += 1;
        let incoming_state = row["incomingReferenceCoverage"]["state"].as_str();
        let process_state = row["processReferenceCoverage"]["state"].as_str();
        let coverage_known = matches!(
            incoming_state,
            Some("not_reached" | "scoped_closure_inspected" | "inspection_unavailable")
        ) && matches!(
            process_state,
            Some(
                "not_reached"
                    | "scoped_process_observation"
                    | "observed_active"
                    | "inspection_unavailable"
            )
        ) && (incoming_state != Some("scoped_closure_inspected")
            || row["incomingReferenceCoverage"]["complete"] == true)
            && (incoming_state != Some("inspection_unavailable")
                || row["incomingReferenceCoverage"]["complete"] == false);
        if let Some(attempts) = row["successfulReferenceChecks"]
            .as_u64()
            .filter(|_| coverage_known)
        {
            add_count(&mut result["references"], "freshChecks", attempts);
            if row["incomingReferenceCoverage"]["state"] == "inspection_unavailable"
                || row["processReferenceCoverage"]["state"] == "inspection_unavailable"
                || row["failureDisposition"] == "inspection_unavailable"
            {
                add_count(&mut result["references"], "unavailableChildren", 1);
                result["incomplete"] = json!(true);
            }
        } else {
            add_count(&mut result["references"], "unknownChildren", 1);
            result["incomplete"] = json!(true);
        }
        let evidence = &row["protectionEvidence"];
        if !evidence.is_object() {
            add_count(&mut result, "missingChildEvidence", 1);
            continue;
        }
        observed += 1;
        for phase in ["plan", "execution"] {
            let state = match evidence[phase]["git"]["state"].as_str() {
                Some("repository_inspected") => "gitInspected",
                Some("no_marker_in_bounded_ancestry") => "noMarker",
                Some("unavailable") => "unavailable",
                Some("not_reached") => "notReached",
                _ => {
                    result["incomplete"] = json!(true);
                    "unavailable"
                }
            };
            add_count(&mut result[phase], state, 1);
            for key in [
                "hardLinks",
                "symlinks",
                "symlinkReferents",
                "sourceBoundaries",
                "otherRetentions",
                "gitTrackedEntries",
            ] {
                if let Some(count) = evidence[phase]["retained"][key].as_u64() {
                    add_count(&mut result[phase]["retained"], key, count);
                } else {
                    result["incomplete"] = json!(true);
                }
            }
        }
        for key in [
            "attempts",
            "metadataBindingChecks",
            "successfulReloads",
            "failures",
        ] {
            if let Some(count) = evidence["revalidation"][key].as_u64() {
                add_count(&mut result["revalidation"], key, count);
            } else {
                result["incomplete"] = json!(true);
            }
        }
        if result["samples"].as_array().unwrap().len() < 2 {
            // Observation-time Git bindings, never repository content hashes.
            result["samples"].as_array_mut().unwrap().push(json!({"childOperationId":row["childOperationId"],"executionGit":evidence["execution"]["git"]}));
        }
    }
    let children = seen.len();
    result["childCount"] = json!(children);
    result["settledChildCount"] = json!(settled);
    result["available"] = json!(observed > 0);
    result["incomplete"] = json!(
        result["incomplete"] == true
            || settled != children
            || observed != settled
            || !terminal(v)
            || v["mutationSettled"] == false
    );
    result["detailsDurable"] = json!(
        result["incomplete"] != true
            && v["effects"]["detailsDurable"] == true
            && v["durabilityIncomplete"] != true
            && v.get("journalError").is_none()
            && rows
                .into_iter()
                .flatten()
                .all(|row| row.get("journalError").is_none())
    );
    result["state"] = json!(if !terminal(v) || v["mutationSettled"] == false {
        "in_progress"
    } else if children == 0 {
        "not_reached"
    } else if result["incomplete"] == true {
        "incomplete"
    } else {
        "recorded"
    });
    compact_protection(result, false)
}
fn effect_summary(v: &Value) -> Value {
    let Some(rows) = v["childDetails"]
        .as_array()
        .filter(|rows| rows.len() <= MAX_CANDIDATES)
    else {
        return json!({"available":false,"totalsComplete":false,"detailsDurable":false,"samples":[],"truncated":true,"sampleLimit":4,"detail":{"operationId":v["operationId"],"receiptFile":format!("{}.json",v["operationId"].as_str().unwrap_or("")),"field":"childDetails"}});
    };
    let mut settled = 0u64;
    let mut actions = 0u64;
    let mut deleted = 0u64;
    let mut allocated = 0u64;
    let mut logical = 0u64;
    let mut created = 0u64;
    let mut recovery = 0u64;
    let mut complete = true;
    let mut samples = Vec::new();
    for row in rows {
        if row["settled"] != true {
            complete = false;
            continue;
        }
        settled = settled.saturating_add(1);
        let files = row["deletedFiles"].as_u64().unwrap_or(0);
        let allocation = row["deletedAllocatedBytes"].as_u64().unwrap_or(0);
        let coordinates = row["coordinationLocksCreated"].as_u64().unwrap_or(0);
        let recover = row["recoveryFiles"].as_u64().unwrap_or(0);
        deleted = deleted.saturating_add(files);
        allocated = allocated.saturating_add(allocation);
        logical = logical.saturating_add(row["deletedLogicalBytes"].as_u64().unwrap_or(0));
        created = created.saturating_add(coordinates);
        recovery = recovery.saturating_add(recover);
        complete &= row["coordinationDetailsComplete"] != false && row["journalError"] != true;
        if files > 0 || coordinates > 0 || recover > 0 {
            actions += 1;
            if samples.len() < 4 {
                let (display, truncated) =
                    scope_display(Path::new(row["artifactScope"].as_str().unwrap_or("")), 240);
                let mut sample = json!({"childOperationId":row["childOperationId"],"provider":row["provider"],"artifactScopeDisplay":display,"artifactScopeTruncated":truncated,"deletedFiles":files,"deletedAllocatedBytes":allocation,"coordinationFilesCreated":coordinates,"recoveryFiles":recover,"coordinationDetailsComplete":row["coordinationDetailsComplete"],"cargoLocksHeld":row["coordinationLocksHeld"]});
                if let Some(file) = row.get("coordinationReceiptFile") {
                    sample["coordinationReceiptFile"] = file.clone();
                }
                samples.push(sample);
            }
        }
    }
    complete &= v["deletedFiles"].as_u64() == Some(deleted)
        && v["deletedLogicalBytes"].as_u64() == Some(logical)
        && v["deletedAllocatedBytes"].as_u64() == Some(allocated)
        && v["recovery"]["files"].as_u64() == Some(recovery);
    let durable = complete && v["durabilityIncomplete"] != true && v.get("journalError").is_none();
    json!({"available":true,"childCount":rows.len(),"settledChildCount":settled,"actionCount":actions,"deletedFiles":deleted,"deletedLogicalBytes":logical,"deletedAllocatedBytes":allocated,"coordinationFilesCreated":created,"recoveryFiles":recovery,"totalsComplete":complete,"detailsDurable":durable,"samples":samples,"sampleLimit":4,"truncated":actions>4,"detail":{"operationId":v["operationId"],"receiptFile":format!("{}.json",v["operationId"].as_str().unwrap_or("")),"field":"childDetails","allChildren":durable}})
}
fn execution_evidence(v: &Value) -> Value {
    let deleted = v["deletedFiles"].as_u64().unwrap_or(0);
    let state = if !terminal(v) || v["mutationSettled"] == false {
        "in_progress"
    } else if matches!(
        v["completion"]["nextAction"].as_str(),
        Some(
            "review_partial_result"
                | "review_space_measurement"
                | "review_recovery"
                | "review_durability"
        )
    ) {
        "review_required"
    } else if v["completion"]["nextAction"] != "report_result" {
        "stopped_incomplete"
    } else if v["noOp"] == true {
        "no_op"
    } else if deleted > 0 {
        "guarded_deletions"
    } else if v["effects"]["coordinationFilesCreated"]
        .as_u64()
        .unwrap_or(0)
        > 0
    {
        "coordination_only"
    } else {
        "no_artifact_effects"
    };
    let checks = if deleted > 0 {
        vec![
            "provider_policy_and_source_boundaries",
            "ordinary_single_link_files",
            "descriptor_identity_and_staging",
            "current_process_inspection",
            "git_index_exclusions",
            "applicable_cargo_coordination",
        ]
    } else {
        Vec::new()
    };
    json!({"state":state,"scope":if deleted>0 {"deleted_artifacts"} else {"no_artifacts_deleted"},"guardedDeletedFiles":deleted,"guardChecks":checks,"method":if deleted>0 {"native preventive exclusions and candidate metadata revalidation"} else if state=="in_progress" {"native_work_in_progress"} else {"not_applicable_no_deleted_artifacts"},"protectionBasis":"native exclusions and guard enforcement for recorded unlinks","wholeTreeContentHashAudit":false,"concurrentSourceImmutabilityVerified":false})
}

// This contract describes the requested space operation, not a whole-tree
// integrity audit or a promise that future concurrent writes cannot consume it.
fn completion_contract(v: &mut Value) {
    v["effects"] = effect_summary(v);
    v["protectionEvidence"] = protection_summary(v);
    let done = terminal(v);
    let space = done
        && v["spaceReadbackVerified"] == true
        && v["goalReached"] == true
        && v.get("actualSpaceError").is_none();
    let recovery = v["recovery"]["files"].as_u64();
    let settled = done && v["mutationSettled"] == true && recovery == Some(0);
    let action = if !done {
        "poll_cleanup_status"
    } else if recovery.is_some_and(|files| files > 0) {
        "review_recovery"
    } else if v["durabilityIncomplete"] == true || v.get("journalError").is_some() {
        "review_durability"
    } else if v["cancelRequested"] == true || v["state"] == "cancelled" {
        "report_cancelled"
    } else if v["spaceReadbackVerified"] != true || v.get("actualSpaceError").is_some() {
        "review_space_measurement"
    } else if v["operationInterrupted"] != false
        || v["deadlineExceeded"] == true
        || v["diagnostics"]["operationIssueEvents"]
            .as_u64()
            .unwrap_or(0)
            > 0
        || matches!(v["state"].as_str(), Some("failed" | "interrupted"))
    {
        "review_partial_result"
    } else if !space {
        "report_unmet_goal"
    } else if !settled || v["effects"]["detailsDurable"] != true {
        "review_partial_result"
    } else {
        "report_result"
    };
    v["completion"] =
        json!({"spaceGoalVerified":space,"cleanupSettled":settled,"nextAction":action});
    v["executionEvidence"] = execution_evidence(v);
}

#[allow(clippy::too_many_arguments)]
fn update_goal(
    v: &mut Value,
    files: u64,
    allocated: u64,
    logical: u64,
    protected: u64,
    unknown: u64,
    entries: u64,
    dirs: u64,
    operations: &[String],
    errors: &Errors,
) {
    v["deletedFiles"] = json!(files);
    v["deletedAllocatedBytes"] = json!(allocated);
    v["deletedLogicalBytes"] = json!(logical);
    v["coverage"]["protectedCandidates"] = json!(protected);
    v["coverage"]["unknownCandidates"] = json!(unknown);
    v["coverage"]["directoryEntriesExamined"] = json!(entries);
    v["coverage"]["directoriesExamined"] = json!(dirs);
    v["recovery"]["operationIds"] = json!(operations.iter().rev().take(16).collect::<Vec<_>>());
    v["recovery"]["operationCount"] = json!(operations.len());
    record_errors(v, errors);
}
fn service_path(journal: &Path, path: &Path, scope: &Path) -> bool {
    path.starts_with(journal)
        || ["MSM_DISK_CACHE_ROOT", "TMPDIR"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .any(|root| root.starts_with(scope) && root != scope && path.starts_with(root))
}
fn historical_hints(scope: &Path, receipt: &mut Value) -> Vec<PathBuf> {
    for ancestor in scope.ancestors().take(8) {
        if ancestor.parent().is_none() {
            break;
        }
        let Ok(Some(snapshot)) = crate::macdisk::get_cached_disk_scan(
            Some(ancestor.to_string_lossy().into_owned()),
            false,
            None,
        ) else {
            continue;
        };
        if snapshot.status != "complete" || !snapshot.cache_hit {
            continue;
        }
        receipt["cacheEvidence"]["hit"] = json!(true);
        receipt["cacheEvidence"]["sourceScanId"] = json!(snapshot.scan_id);
        receipt["cacheEvidence"]["sourceRoots"] = json!(snapshot.roots);
        receipt["cacheEvidence"]["indexPolicyVersion"] = json!(snapshot.index_policy_version);
        receipt["cacheEvidence"]["sourceStartedAtMs"] = json!(snapshot.started_at_ms);
        receipt["cacheEvidence"]["sourceUpdatedAtMs"] = json!(snapshot.updated_at_ms);
        let mut rows = snapshot.entries;
        rows.sort_by_key(|row| std::cmp::Reverse(row.allocated_bytes));
        let hints: Vec<_> = rows
            .into_iter()
            .filter(|row| row.kind == "directory")
            .map(|row| PathBuf::from(row.path))
            .filter(|p| {
                p != scope
                    && discovery_depth(scope, p).is_some_and(|depth| depth <= DISCOVERY_DEPTH)
            })
            .take(64)
            .collect();
        receipt["cacheEvidence"]["hintsUsed"] = json!(hints.len());
        return hints;
    }
    Vec::new()
}

fn refresh_terminal_space(value: &mut Value) {
    if value["durabilityIncomplete"] == true && value["state"] == "completed" {
        value["state"] = json!("partial");
    }
    let result = (|| -> Result<u64, String> {
        let (_, file) = safe_root(Path::new(
            value["scope"].as_str().ok_or("Scope unavailable")?,
        ))?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if Some(metadata.dev()) != value["volumeDevice"].as_u64()
            || Some(metadata.ino()) != value["scopeInode"].as_u64()
        {
            return Err("Authorized scope/volume changed".into());
        }
        volume_available(&file).map(|(available, _)| available)
    })();
    match result {
        Ok(available) => {
            value["spaceReadbackVerified"] = json!(true);
            value["availableBytes"] = json!(available);
            let target = value["targetAvailableBytes"].as_u64().unwrap_or(u64::MAX);
            value["goalReached"] = json!(available >= target);
            value["observedHeadroomAboveTargetBytes"] = json!(available.saturating_sub(target));
            if let Some(initial) = value["initialAvailableBytes"].as_u64() {
                value["observedAvailableBytesDelta"] = json!(available as i128 - initial as i128);
            }
            if available < target && value["state"] == "completed" {
                value["state"] = json!("partial");
            }
            value["spaceSampleTiming"] =
                json!("after durable receipt publication; no subsequent state write");
        }
        Err(error) => {
            value["spaceReadbackVerified"] = json!(false);
            value["goalReached"] = json!(false);
            value["availableBytes"] = Value::Null;
            value["actualSpaceError"] = json!(error.chars().take(180).collect::<String>());
            if value["state"] == "completed" {
                value["state"] = json!("partial");
            }
        }
    }
}
fn encoded_receipt_size(value: &Value) -> usize {
    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
        .to_string()
        .len()
}
fn minimal_effects(v: &Value) -> Value {
    let id = v["operationId"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(32)
        .collect::<String>();
    let omitted =
        v["effects"]["actionCount"].as_u64().unwrap_or(1) > 0 || v["effects"]["truncated"] == true;
    let mut result = json!({"samples":[],"sampleLimit":4,"truncated":omitted,"sampleDetailsOmitted":omitted,"detail":{"operationId":id,"receiptFile":format!("{id}.json"),"field":"childDetails","allChildren":v["effects"]["detailsDurable"].as_bool()}});
    for field in ["available", "totalsComplete", "detailsDurable"] {
        result[field] = json!(v["effects"][field].as_bool());
    }
    for field in [
        "childCount",
        "settledChildCount",
        "actionCount",
        "deletedFiles",
        "deletedLogicalBytes",
        "deletedAllocatedBytes",
        "coordinationFilesCreated",
        "recoveryFiles",
    ] {
        result[field] = json!(v["effects"][field].as_u64());
    }
    result
}
fn minimal_receipt(v: &Value) -> Value {
    let id = v["operationId"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(32)
        .collect::<String>();
    let mut result = json!({"operationId":id,"state":v["state"].as_str().unwrap_or("unknown").chars().take(32).collect::<String>(),"phase":v["phase"].as_str().unwrap_or("unknown").chars().take(32).collect::<String>(),"receiptTruncated":true,"detailedEvidenceRetainedServerSide":true,"errors":[],"errorsTruncated":v["errorsTruncated"]==true || v["errorsCount"].as_u64().unwrap_or(1)>0,"errorSampleDetailsTruncated":v["errors"].as_array().is_some_and(|items|!items.is_empty()),"recovery":{"files":v["recovery"]["files"].as_u64(),"operationCount":v["recovery"]["operationCount"].as_u64()},"diagnostics":{"candidateRetentionEvents":v["diagnostics"]["candidateRetentionEvents"].as_u64(),"operationIssueEvents":v["diagnostics"]["operationIssueEvents"].as_u64(),"samples":[],"truncated":true,"detailOperationId":id,"candidateScopes":{"count":v["diagnostics"]["candidateScopes"]["count"].as_u64(),"samples":[],"truncated":true,"sampleLimit":4}}});
    for key in [
        "goalReached",
        "durabilityIncomplete",
        "operationInterrupted",
        "cancelRequested",
        "deadlineExceeded",
        "spaceReadbackVerified",
        "mutationSettled",
    ] {
        result[key] = json!(v[key].as_bool());
    }
    for key in [
        "availableBytes",
        "targetAvailableBytes",
        "deletedFiles",
        "deletedAllocatedBytes",
        "deletedLogicalBytes",
        "errorsCount",
        "maxBatchAllocatedBytes",
        "largestDeletedFileAllocatedBytes",
        "batchAllocatedLimitBytes",
        "batchFileLimit",
    ] {
        result[key] = json!(v[key].as_u64());
    }
    for key in [
        "observedAvailableBytesDelta",
        "observedHeadroomAboveTargetBytes",
    ] {
        result[key] = v[key]
            .as_number()
            .cloned()
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    for key in [
        "discoveryComplete",
        "freshPlans",
        "stoppedAtGoal",
        "cargoChildProbeBudgetExhausted",
    ] {
        result["coverage"][key] = json!(v["coverage"][key].as_bool());
    }
    for key in [
        "protectedCandidates",
        "unknownCandidates",
        "directoriesExamined",
        "directoryEntriesExamined",
        "gitTrackedFiles",
        "coordinationLockFiles",
        "cargoChildProbes",
        "cargoChildProbeLimit",
        "cargoChildProbeHits",
        "cargoChildProbesSkipped",
        "discoveryDepthLimit",
        "discoveryEntryLimit",
        "discoveryDirectoryLimit",
        "discoveryCandidateLimit",
    ] {
        result["coverage"][key] = json!(v["coverage"][key].as_u64());
    }
    let action = v["completion"]["nextAction"]
        .as_str()
        .filter(|action| {
            matches!(
                *action,
                "poll_cleanup_status"
                    | "report_result"
                    | "report_unmet_goal"
                    | "review_partial_result"
                    | "review_space_measurement"
                    | "review_recovery"
                    | "review_durability"
                    | "report_cancelled"
            )
        })
        .unwrap_or("review_partial_result");
    result["completion"] = json!({"spaceGoalVerified":v["completion"]["spaceGoalVerified"].as_bool(),"cleanupSettled":v["completion"]["cleanupSettled"].as_bool(),"nextAction":action});
    result["effects"] = minimal_effects(v);
    result["protectionEvidence"] = compact_protection(v["protectionEvidence"].clone(), true);
    result["executionEvidence"] = json!({"state":v["executionEvidence"]["state"].as_str().unwrap_or("review_required").chars().take(32).collect::<String>(),"scope":if v["deletedFiles"].as_u64().unwrap_or(0)>0 {"deleted_artifacts"} else {"no_artifacts_deleted"},"guardedDeletedFiles":v["deletedFiles"].as_u64(),"wholeTreeContentHashAudit":false,"concurrentSourceImmutabilityVerified":false});
    if v.get("actualSpaceError").is_some() {
        result["actualSpaceError"] =
            json!("Scope/space readback failed; detailed error retained server-side");
    }
    if v.get("journalError").is_some() {
        result["journalError"] =
            json!("Receipt publication failed; detailed error retained server-side");
    }
    result
}
fn budget_receipt(v: &Value) -> Value {
    let mut result = json!({"operationId":v["operationId"],"state":v["state"],"phase":v["phase"],"goalReached":v["goalReached"],"targetAvailableBytes":v["targetAvailableBytes"],"availableBytes":v["availableBytes"],"deletedFiles":v["deletedFiles"],"deletedAllocatedBytes":v["deletedAllocatedBytes"],"deletedLogicalBytes":v["deletedLogicalBytes"],"observedAvailableBytesDelta":v["observedAvailableBytesDelta"],"maxBatchAllocatedBytes":v["maxBatchAllocatedBytes"],"largestDeletedFileAllocatedBytes":v["largestDeletedFileAllocatedBytes"],"batchAllocatedLimitBytes":v["batchAllocatedLimitBytes"],"batchFileLimit":v["batchFileLimit"],"observedHeadroomAboveTargetBytes":v["observedHeadroomAboveTargetBytes"],"coverage":v["coverage"],"receiptTruncated":true,"detailedEvidenceRetainedServerSide":true,"durabilityIncomplete":v["durabilityIncomplete"],"errorsTruncated":v["errorsTruncated"],"scopeDisplay":v["scope"].as_str().unwrap_or("").chars().take(120).collect::<String>(),"errorsCount":v["errorsCount"],"errors":v["errors"].as_array().map(|e|e.iter().take(2).filter_map(Value::as_str).map(|e|e.chars().take(120).collect::<String>()).collect::<Vec<_>>()).unwrap_or_default(),"recovery":{"files":v["recovery"]["files"],"operationCount":v["recovery"]["operationCount"]},"cacheEvidence":{"hit":v["cacheEvidence"]["hit"],"sourceScanId":v["cacheEvidence"]["sourceScanId"].as_str().map(|s|s.chars().take(128).collect::<String>()),"hintsUsed":v["cacheEvidence"]["hintsUsed"]}});
    for field in [
        "completion",
        "executionEvidence",
        "effects",
        "protectionEvidence",
        "operationInterrupted",
        "cancelRequested",
        "deadlineExceeded",
        "spaceReadbackVerified",
        "mutationSettled",
        "diagnostics",
    ] {
        if let Some(value) = v.get(field) {
            result[field] = value.clone();
        }
    }
    // Typed samples refer to the original bounded error sample indexes.
    // Retain those samples and their original truncation/count semantics.
    result["errors"] = v["errors"].clone();
    if v.get("actualSpaceError").is_some() {
        result["actualSpaceError"] = v["actualSpaceError"].clone();
    }
    if v.get("journalError").is_some() {
        result["journalError"] =
            json!("Receipt publication failed; server retains operation state");
    }
    if encoded_receipt_size(&result) > 7400 {
        if let Some(rows) = result["effects"]["samples"].as_array_mut() {
            for row in rows {
                if let Some(path) = row["artifactScopeDisplay"].as_str() {
                    let (display, truncated) = scope_display(Path::new(path), 80);
                    row["artifactScopeDisplay"] = json!(display);
                    if truncated {
                        row["artifactScopeTruncated"] = json!(true);
                    }
                }
            }
        }
        if let Some(rows) = result["diagnostics"]["candidateScopes"]["samples"].as_array_mut() {
            for row in rows {
                if let Some(display) = row["scopeDisplay"].as_str() {
                    let (display, truncated) = scope_display(Path::new(display), 80);
                    row["scopeDisplay"] = json!(display);
                    if truncated {
                        row["scopeTruncated"] = json!(true);
                    }
                }
            }
        }
        result["errors"] = json!(v["errors"]
            .as_array()
            .map(|items| items
                .iter()
                .take(8)
                .filter_map(Value::as_str)
                .map(|e| e.chars().take(80).collect::<String>())
                .collect::<Vec<_>>())
            .unwrap_or_default());
        let shortened = v["errors"].as_array().is_some_and(|items| {
            items.len() > 8
                || items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|message| message.chars().count() > 80)
        });
        result["errorsTruncated"] = json!(v["errorsTruncated"] == true || shortened);
        result["errorSampleDetailsTruncated"] = json!(shortened);
    }
    if encoded_receipt_size(&result) > 7400 {
        result["diagnostics"]["samples"] = json!([]);
        result["diagnostics"]["truncated"] = json!(true);
        result["diagnostics"]["candidateScopes"]["samples"] = json!([]);
        result["diagnostics"]["candidateScopes"]["truncated"] = json!(true);
    }
    if encoded_receipt_size(&result) > 7400 {
        result["protectionEvidence"] = compact_protection(v["protectionEvidence"].clone(), true);
        result["effects"] = minimal_effects(v);
    }
    if encoded_receipt_size(&result) > 7400 {
        result = minimal_receipt(v);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Owned {
        root: PathBuf,
        target: PathBuf,
        service: AgentCleanup,
    }
    impl Owned {
        fn new(files: usize) -> Self {
            let root = fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("cublet-goal-test-{}", id().unwrap()));
            let project = root.join("worktrees/project");
            let target = project.join("target");
            fs::create_dir_all(target.join("debug/deps")).unwrap();
            fs::create_dir(project.join("src")).unwrap();
            fs::write(
                project.join("Cargo.toml"),
                b"[package]\nname='owned'\nversion='0.0.0'\n",
            )
            .unwrap();
            fs::write(project.join("src/lib.rs"), b"// preserve source").unwrap();
            fs::write(root.join("state.sqlite"), b"preserve ordinary data").unwrap();
            for i in 0..files {
                fs::write(target.join(format!("debug/deps/{i}.o")), [i as u8; 4096]).unwrap();
            }
            let service = AgentCleanup::new(root.join("service-journal"), true).unwrap();
            *service.inner.process_guard.lock().unwrap() = Some(Ok(()));
            Self {
                root,
                target,
                service,
            }
        }
        fn unattainable(&self) -> u64 {
            let (_, file) = safe_root(&self.root).unwrap();
            volume_available(&file).unwrap().1 - 1
        }
        fn wait(&self, id: &str) -> Value {
            let started = Instant::now();
            loop {
                let value = self.service.goal_status(id, 2000, false).unwrap();
                if terminal(&value) {
                    return value;
                }
                assert!(started.elapsed() < Duration::from_secs(12), "{value}");
            }
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn fresh_cargo_child_priority_reaches_gitfile_project_before_unrelated_breadth() {
        let f = Owned::new(0);
        let scope = f.root.join("worktrees");
        let base = scope.join("project");
        git_fixture(&base, &["init", "--quiet", "--template="]);
        git_fixture(&base, &["add", "Cargo.toml", "src/lib.rs"]);
        git_fixture(
            &base,
            &["commit", "--quiet", "-m", "owned discovery source"],
        );
        let deep = scope.join("nested/deeper/linked-project");
        fs::create_dir_all(deep.parent().unwrap()).unwrap();
        git_fixture(
            &base,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                deep.to_str().unwrap(),
            ],
        );
        assert!(deep.join(".git").is_file());
        let artifact = deep.join("target/debug/deps/owned.o");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, [7u8; 4096]).unwrap();
        let control = fs::read(deep.join("src/lib.rs")).unwrap();
        let control_inode = deep.join("src/lib.rs").metadata().unwrap().ino();
        // The same breadth can exhaust the production50k entry cutoff before
        // FIFO reaches this project's target; no reduced test-only budget.
        let broad = scope.join("z-unrelated");
        for bucket in 0..512 {
            let directory = broad.join(format!("group-{bucket:04}"));
            fs::create_dir_all(&directory).unwrap();
            for marker in 0..128 {
                fs::write(directory.join(format!("entry-{marker:04}")), []).unwrap();
            }
        }
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":scope,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 1, "{result}");
        assert!(!artifact.exists());
        assert_eq!(fs::read(deep.join("src/lib.rs")).unwrap(), control);
        assert_eq!(
            deep.join("src/lib.rs").metadata().unwrap().ino(),
            control_inode
        );
        assert_eq!(
            result["coverage"]["directoryEntriesExamined"],
            DISCOVERY_ENTRIES + 1
        );
        assert_eq!(result["coverage"]["discoveryComplete"], false);
        assert_eq!(result["operationInterrupted"], true);
        assert!(result["coverage"]["cargoChildProbeHits"].as_u64().unwrap() > 0);
        assert!(result["coverage"]["cargoChildProbes"].as_u64().unwrap() <= CARGO_CHILD_PROBES);
        assert!(broad.join("group-0511/entry-0127").exists());
    }
    #[test]
    fn cargo_child_probes_reject_symlinks_source_boundaries_and_foreign_device() {
        use std::os::unix::fs::symlink;
        let f = Owned::new(1);
        let parent_path = f.root.join("worktrees");
        let (_, parent) = safe_root(&parent_path).unwrap();
        let project = parent_path.join("project");
        let device = parent.metadata().unwrap().dev();
        let mut probes = CargoChildProbes::default();
        let cancel = AtomicBool::new(false);
        assert!(probes
            .probe(
                &parent,
                OsStr::new("project"),
                &project,
                device + 1,
                &cancel,
                Instant::now()
            )
            .unwrap()
            .is_none());
        let manifest = project.join("Cargo.toml");
        let saved = project.join("saved-manifest");
        fs::rename(&manifest, &saved).unwrap();
        symlink(&saved, &manifest).unwrap();
        assert!(probes
            .probe(
                &parent,
                OsStr::new("project"),
                &project,
                device,
                &cancel,
                Instant::now()
            )
            .unwrap()
            .is_none());
        fs::remove_file(&manifest).unwrap();
        fs::rename(&saved, &manifest).unwrap();
        let saved_target = project.join("saved-target");
        fs::rename(&f.target, &saved_target).unwrap();
        symlink(&saved_target, &f.target).unwrap();
        assert!(probes
            .probe(
                &parent,
                OsStr::new("project"),
                &project,
                device,
                &cancel,
                Instant::now()
            )
            .unwrap()
            .is_none());
        fs::remove_file(&f.target).unwrap();
        fs::rename(&saved_target, &f.target).unwrap();
        for marker in ["Cargo.toml", ".git"] {
            fs::write(f.target.join(marker), b"owned source marker").unwrap();
            assert!(probes
                .probe(
                    &parent,
                    OsStr::new("project"),
                    &project,
                    device,
                    &cancel,
                    Instant::now()
                )
                .unwrap()
                .is_none());
            fs::remove_file(f.target.join(marker)).unwrap();
        }
        let clue = probes
            .probe(
                &parent,
                OsStr::new("project"),
                &project,
                device,
                &cancel,
                Instant::now(),
            )
            .unwrap()
            .unwrap();
        // A scheduling clue cannot authorize the target after a source-boundary
        // change. Execution performs its independent fresh validation.
        fs::write(
            f.target.join(".git"),
            b"owned source boundary introduced after probe",
        )
        .unwrap();
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":clue,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0);
        assert!(f.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn cargo_child_probe_work_and_cancel_deadline_are_independently_bounded() {
        let f = Owned::new(1);
        let path = f.root.join("worktrees");
        let (_, parent) = safe_root(&path).unwrap();
        let device = parent.metadata().unwrap().dev();
        let mut probes = CargoChildProbes::default();
        let cancel = AtomicBool::new(false);
        for _ in 0..CARGO_CHILD_PROBES {
            assert!(probes
                .probe(
                    &parent,
                    OsStr::new("absent"),
                    &path.join("absent"),
                    device,
                    &cancel,
                    Instant::now()
                )
                .unwrap()
                .is_none());
        }
        assert!(probes
            .probe(
                &parent,
                OsStr::new("project"),
                &path.join("project"),
                device,
                &cancel,
                Instant::now()
            )
            .unwrap()
            .is_none());
        assert_eq!(probes.attempted, CARGO_CHILD_PROBES);
        assert_eq!(probes.skipped, 1);
        cancel.store(true, Ordering::Relaxed);
        assert!(probes
            .probe(
                &parent,
                OsStr::new("project"),
                &path.join("project"),
                device,
                &cancel,
                Instant::now()
            )
            .is_err());
        cancel.store(false, Ordering::Relaxed);
        assert!(probes
            .probe(
                &parent,
                OsStr::new("project"),
                &path.join("project"),
                device,
                &cancel,
                Instant::now() - Duration::from_secs(EXECUTE_SECONDS + 1)
            )
            .is_err());
        assert_eq!(probes.attempted, CARGO_CHILD_PROBES);
        // An exhausted optional clue is not a deletion prohibition. The normal
        // queued project's direct provider discovery still uses fresh guards.
        let response=f.service.reclaim_with_before(&json!({"scope":path.join("project"),"targetAvailableBytes":f.unattainable(),"waitMs":0}),||Ok(())).unwrap();
        assert_eq!(
            f.wait(response["operationId"].as_str().unwrap())["deletedFiles"],
            1
        );
    }
    #[test]
    fn discovery_depth_precedes_provider_execution_but_explicit_deep_scope_is_valid() {
        for depth in [DISCOVERY_DEPTH, DISCOVERY_DEPTH + 1] {
            let f = Owned::new(0);
            let scope = f.root.join("depth-scope");
            let mut project = scope.clone();
            for index in 0..depth - 1 {
                project.push(format!("level-{index}"));
            }
            let target = project.join("target");
            let artifact = target.join("debug/deps/owned.o");
            fs::create_dir_all(artifact.parent().unwrap()).unwrap();
            fs::write(
                project.join("Cargo.toml"),
                b"[package]\nname='owned'\nversion='0.0.0'\n",
            )
            .unwrap();
            fs::write(&artifact, [3u8; 4096]).unwrap();
            assert_eq!(discovery_depth(&scope, &target), Some(depth));
            assert_eq!(discovery_depth(&target, &target), Some(0));
            let response = f
                .service
                .reclaim_with_before(
                    &json!({"scope":scope,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                    || Ok(()),
                )
                .unwrap();
            let result = f.wait(response["operationId"].as_str().unwrap());
            assert_eq!(result["coverage"]["discoveryDepthLimit"], DISCOVERY_DEPTH);
            if depth == DISCOVERY_DEPTH {
                assert_eq!(result["deletedFiles"], 1, "{result}");
                assert!(!artifact.exists());
            } else {
                assert_eq!(result["deletedFiles"], 0, "{result}");
                assert!(artifact.exists());
                assert_eq!(result["coverage"]["cargoChildProbeHits"], 0);
                assert_eq!(result["coverage"]["discoveryComplete"], false);
                let explicit = f
                    .service
                    .reclaim_with_before(
                        &json!({"scope":target,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                        || Ok(()),
                    )
                    .unwrap();
                assert_eq!(
                    f.wait(explicit["operationId"].as_str().unwrap())["deletedFiles"],
                    1
                );
            }
        }
    }
    #[test]
    fn disabled_rust_provider_does_not_probe_or_delete_rust_targets() {
        let f = Owned::new(1);
        let response=f.service.reclaim_with_before(&json!({"scope":f.root,"providers":["go_build_cache"],"targetAvailableBytes":f.unattainable(),"waitMs":0}),||Ok(())).unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0);
        assert_eq!(result["coverage"]["cargoChildProbes"], 0);
        assert_eq!(result["coverage"]["cargoChildProbeHits"], 0);
        assert!(f.target.join("debug/deps/0.o").exists());
    }

    #[test]
    fn actual_many_gitfile_children_have_durable_lineage_and_poll_dedup_totals() {
        let f = Owned::new(0);
        let scope = f.root.join("worktrees");
        let base = scope.join("project");
        git_fixture(&base, &["init", "--quiet", "--template="]);
        git_fixture(&base, &["add", "Cargo.toml", "src/lib.rs"]);
        git_fixture(
            &base,
            &["commit", "--quiet", "-m", "owned effects controls"],
        );
        let mut expected_allocation = 0;
        let mut controls = Vec::new();
        for index in 0..36 {
            let project = scope.join(format!("nested/project-{index:02}"));
            fs::create_dir_all(project.parent().unwrap()).unwrap();
            git_fixture(
                &base,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    project.to_str().unwrap(),
                ],
            );
            let artifact = project.join("target/debug/deps/owned.o");
            fs::create_dir_all(artifact.parent().unwrap()).unwrap();
            fs::write(&artifact, [index as u8; 4096]).unwrap();
            expected_allocation += artifact.metadata().unwrap().blocks() * 512;
            controls.push((
                project.join("src/lib.rs"),
                fs::read(project.join("src/lib.rs")).unwrap(),
                project.join("src/lib.rs").metadata().unwrap().ino(),
            ));
        }
        let (entered, wait) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let args = json!({"scope":scope,"targetAvailableBytes":f.unattainable(),"waitMs":0});
        let first = f
            .service
            .reclaim_with_before(&args, move || {
                entered.send(()).unwrap();
                blocked
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = f
            .service
            .reclaim_with_before(&args, || {
                Err::<(), _>("dedup must not dispatch again".into())
            })
            .unwrap();
        assert_eq!(first["operationId"], second["operationId"]);
        assert_eq!(first["executionEvidence"]["state"], "in_progress");
        assert_eq!(first["protectionEvidence"]["state"], "in_progress");
        assert_eq!(first["protectionEvidence"]["incomplete"], true);
        release.send(()).unwrap();
        let op = first["operationId"].as_str().unwrap();
        let started = Instant::now();
        let result = loop {
            let value = f.service.goal_status(op, 2000, false).unwrap();
            if terminal(&value) && value["mutationSettled"] == true {
                break value;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "{value}");
        };
        assert_eq!(result["deletedFiles"], 36);
        assert_eq!(result["deletedAllocatedBytes"], expected_allocation);
        assert_eq!(result["effects"]["childCount"], 36);
        assert_eq!(result["effects"]["actionCount"], 36);
        assert_eq!(result["effects"]["deletedFiles"], 36);
        assert_eq!(result["effects"]["coordinationFilesCreated"], 36);
        assert_eq!(result["effects"]["samples"].as_array().unwrap().len(), 4);
        assert_eq!(result["effects"]["truncated"], true);
        assert_eq!(result["effects"]["detailsDurable"], true);
        assert_eq!(result["protectionEvidence"]["plan"]["gitInspected"], 36);
        assert_eq!(
            result["protectionEvidence"]["execution"]["gitInspected"],
            36
        );
        assert_eq!(result["protectionEvidence"]["childCount"], 36);
        assert_eq!(result["protectionEvidence"]["detailsDurable"], true);
        assert!(
            result["protectionEvidence"]["samples"]
                .as_array()
                .unwrap()
                .len()
                <= 2
        );
        assert_eq!(result["protectionEvidence"]["truncated"], true);
        assert!(protection_wire_bytes(&result["protectionEvidence"]) <= 1536);
        assert!(result.get("childDetails").is_none());
        for _ in 0..3 {
            assert_eq!(
                f.service.goal_status(op, 0, false).unwrap()["protectionEvidence"],
                result["protectionEvidence"]
            );
            assert_eq!(
                f.service.goal_status(op, 0, false).unwrap()["effects"],
                result["effects"]
            );
        }
        let durable: Value = serde_json::from_slice(
            &fs::read(f.service.inner.journal.join(format!("{op}.json"))).unwrap(),
        )
        .unwrap();
        let rows = durable["childDetails"].as_array().unwrap();
        assert_eq!(rows.len(), 36);
        assert_eq!(
            durable["recovery"]["operationIds"]
                .as_array()
                .unwrap()
                .len(),
            16
        );
        let ids: HashSet<_> = rows
            .iter()
            .map(|row| row["childOperationId"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 36);
        for row in rows {
            assert_eq!(row["parentOperationId"], op);
            assert_eq!(row["provider"], "rust_target");
            assert_eq!(row["deletedFiles"], 1);
            assert_eq!(
                row["coordinationCreatedBindings"].as_array().unwrap().len(),
                1
            );
            let binding = &row["coordinationCreatedBindings"][0];
            let file = Path::new(binding["path"].as_str().unwrap());
            assert_eq!(binding["ino"], file.metadata().unwrap().ino());
            assert_eq!(binding["identityRecorded"], true);
        }
        for (path, bytes, inode) in controls {
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(path.metadata().unwrap().ino(), inode);
        }
        // Even if individual old child receipts are evicted later, the durable
        // root locator contains every omitted action and created-inode binding.
        assert_eq!(result["effects"]["detail"]["field"], "childDetails");
        assert_eq!(result["effects"]["detail"]["allChildren"], true);
    }
    #[test]
    fn partial_cargo_acquisition_reports_coordination_creation_without_artifact_deletion() {
        let f = Owned::new(1);
        let release = f.target.join("release/deps/other.o");
        fs::create_dir_all(release.parent().unwrap()).unwrap();
        fs::write(&release, b"owned release output").unwrap();
        let busy = f.target.join("release/.cargo-lock");
        fs::write(&busy, []).unwrap();
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&busy)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0);
        assert_eq!(result["effects"]["actionCount"], 1);
        assert_eq!(result["effects"]["coordinationFilesCreated"], 1);
        assert_eq!(result["effects"]["samples"][0]["deletedFiles"], 0);
        assert_eq!(
            result["effects"]["samples"][0]["coordinationFilesCreated"],
            1
        );
        assert_eq!(result["effects"]["detailsDurable"], true);
        assert_ne!(result["executionEvidence"]["state"], "no_op");
        assert!(f.target.join("debug/.cargo-lock").exists());
        assert!(f.target.join("debug/deps/0.o").exists());
        assert!(release.exists());
        let child = result["effects"]["samples"][0]["childOperationId"]
            .as_str()
            .unwrap();
        let coordination: Value = serde_json::from_slice(
            &fs::read(
                f.service
                    .inner
                    .journal
                    .join(format!("{child}.coordination.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(coordination["acquisitionComplete"], false);
        assert_eq!(coordination["locks"][0]["created"], true);
        assert_eq!(
            coordination["locks"][0]["ino"],
            f.target.join("debug/.cargo-lock").metadata().unwrap().ino()
        );
    }
    #[test]
    fn cancellation_after_real_effects_keeps_actual_totals_without_claiming_success() {
        let f = Owned::new(1);
        let other = f.root.join("worktrees/second");
        fs::create_dir_all(other.join("target/debug/deps")).unwrap();
        fs::write(
            other.join("Cargo.toml"),
            b"[package]\nname='owned'\nversion='0.0.0'\n",
        )
        .unwrap();
        fs::write(other.join("target/debug/deps/owned.o"), [9u8; 4096]).unwrap();
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let op = first["operationId"].as_str().unwrap();
        let start = Instant::now();
        loop {
            let status = f.service.goal_status(op, 0, false).unwrap();
            if status["deletedFiles"].as_u64().unwrap_or(0) > 0 {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        f.service.goal_status(op, 0, true).unwrap();
        let result = f.wait(op);
        assert_eq!(result["completion"]["nextAction"], "report_cancelled");
        assert_eq!(result["executionEvidence"]["state"], "stopped_incomplete");
        assert!(result["deletedFiles"].as_u64().unwrap() > 0);
        assert_eq!(result["effects"]["deletedFiles"], result["deletedFiles"]);
        assert_eq!(result["effects"]["detailsDurable"], true);
        let repeated = f.service.goal_status(op, 0, false).unwrap();
        assert_eq!(result["effects"], repeated["effects"]);
    }

    #[test]
    fn created_coordination_evidence_publication_failure_prevents_completion() {
        let f = Owned::new(1);
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":1,"waitMs":2000}),
                || Ok(()),
            )
            .unwrap();
        let op = first["operationId"].as_str().unwrap();
        let child = id().unwrap();
        let cancel = AtomicBool::new(false);
        let plan = f
            .service
            .build_plan(&id().unwrap(), "rust_target", &f.target, &cancel)
            .unwrap();
        f.service
            .insert(
                &child,
                json!({"jobId":child,"operation":"execute","state":"running"}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        fs::create_dir(
            f.service
                .inner
                .journal
                .join(format!("{child}.coordination.json")),
        )
        .unwrap();
        f.service.run_execution_context(
            &child,
            &plan,
            &cancel,
            || Ok(()),
            ExecutionContext {
                authorization_scope: Some(f.root.clone()),
                parent_goal: Some(op.into()),
                ..ExecutionContext::default()
            },
        );
        let outcome = f.service.status(&child).unwrap();
        assert_eq!(outcome["deletedFiles"], 0);
        assert_eq!(outcome["coordinationLocksCreated"], 1);
        assert_eq!(outcome["coordinationDetailsComplete"], false);
        assert_eq!(
            outcome["coordinationCreatedBindings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(f.target.join("debug/deps/0.o").exists());
        let mut value = f.service.status(op).unwrap();
        start_child_detail(&mut value, &child, "rust_target", &f.target);
        start_child_detail(&mut value, &child, "rust_target", &f.target);
        settle_child_detail(&mut value, &child, &outcome);
        settle_child_detail(&mut value, &child, &outcome);
        completion_contract(&mut value);
        assert_eq!(value["effects"]["actionCount"], 1);
        assert_eq!(value["effects"]["coordinationFilesCreated"], 1);
        assert_eq!(value["effects"]["detailsDurable"], false);
        assert_eq!(value["completion"]["nextAction"], "review_partial_result");
        assert_eq!(value["executionEvidence"]["state"], "review_required");
        assert_eq!(value["protectionEvidence"]["detailsDurable"], false);
        assert_eq!(value["protectionEvidence"]["execution"]["notReached"], 1);
    }

    #[test]
    fn already_satisfied_is_a_native_noop_without_barrier_or_deletion() {
        let f = Owned::new(4);
        let result = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":1,"waitMs":2000}),
                || Err::<(), _>("barrier must not run".into()),
            )
            .unwrap();
        assert_eq!(result["state"], "completed");
        assert_eq!(result["noOp"], true);
        assert_eq!(result["protectionEvidence"]["state"], "not_reached");
        assert_eq!(result["protectionEvidence"]["available"], false);
        assert_eq!(result["protectionEvidence"]["childCount"], 0);
        assert_eq!(result["deletedFiles"], 0);
        assert_eq!(result["completion"]["nextAction"], "report_result");
        assert_eq!(result["completion"]["spaceGoalVerified"], true);
        assert_eq!(result["completion"]["cleanupSettled"], true);
        assert_eq!(result["executionEvidence"]["scope"], "no_artifacts_deleted");
        assert_eq!(result["executionEvidence"]["state"], "no_op");
        assert_eq!(result["effects"]["actionCount"], 0);
        assert_eq!(result["effects"]["coordinationFilesCreated"], 0);
        assert_eq!(result["effects"]["detailsDurable"], true);
        assert_eq!(
            result["executionEvidence"]["wholeTreeContentHashAudit"],
            false
        );
        assert!(f.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn reached_goal_distinguishes_retained_candidates_from_operation_interruptions() {
        let f = Owned::new(1);
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":1,"waitMs":2000}),
                || Ok(()),
            )
            .unwrap();
        let op = response["operationId"].as_str().unwrap();
        let base = f.service.status(op).unwrap();
        let mut value = base.clone();
        let mut issues = Errors::default();
        push_issue(
            &mut issues,
            IssueKind::CandidateRetention,
            "candidate_active",
            "Live process command references the selected cache; retained",
        );
        push_issue(
            &mut issues,
            IssueKind::CandidateRetention,
            "candidate_inspection_unavailable",
            "Candidate inspection failed; retained",
        );
        record_errors(&mut value, &issues);
        value["coverage"]["discoveryComplete"] = json!(false);
        f.service.publish(op, value);
        let done = f.service.goal_status(op, 0, false).unwrap();
        assert_eq!(done["completion"]["nextAction"], "report_result");
        assert_eq!(done["errorsCount"], 2);
        assert_eq!(done["diagnostics"]["candidateRetentionEvents"], 2);
        assert_eq!(done["diagnostics"]["operationIssueEvents"], 0);
        assert_eq!(done["diagnostics"]["samples"][1]["errorIndex"], 1);
        assert_eq!(
            done["errors"][0],
            "Live process command references the selected cache; retained"
        );
        // Zero deleted files do not turn an execution invariant failure into
        // protective retention, even when another path has met the space goal.
        let (kind, code) = child_issue(
            &json!({"deletedFiles":0,"recoveryFiles":0,"failureDisposition":"execution_issue"}),
        );
        assert!(matches!(kind, IssueKind::Operation));
        let mut critical = base.clone();
        let mut events = Errors::default();
        push_scoped_issue(
            &mut events,
            kind,
            code,
            "Selected cache directory was replaced; retained",
            &f.target,
        );
        record_errors(&mut critical, &events);
        f.service.publish(op, critical);
        let critical = f.service.goal_status(op, 0, false).unwrap();
        assert_eq!(critical["completion"]["spaceGoalVerified"], true);
        assert_eq!(
            critical["completion"]["nextAction"],
            "review_partial_result"
        );
        assert_eq!(critical["diagnostics"]["operationIssueEvents"], 1);
        assert_eq!(
            critical["diagnostics"]["candidateScopes"]["samples"][0]["scopeDisplay"],
            f.target.to_str().unwrap()
        );
        // Reaching an external lower target must never complete this own goal.
        let mut unmet = base.clone();
        unmet["targetAvailableBytes"] = json!(f.unattainable());
        f.service.publish(op, unmet);
        assert_ne!(
            f.service.goal_status(op, 0, false).unwrap()["completion"]["nextAction"],
            "report_result"
        );
        for (key, value) in [
            ("cancelRequested", json!(true)),
            ("deadlineExceeded", json!(true)),
            ("operationInterrupted", json!(true)),
            ("durabilityIncomplete", json!(true)),
            ("mutationSettled", json!(false)),
        ] {
            let mut issue = base.clone();
            issue[key] = value;
            f.service.publish(op, issue);
            assert_ne!(
                f.service.goal_status(op, 0, false).unwrap()["completion"]["nextAction"],
                "report_result",
                "{key}"
            );
        }
        let mut recovery = base;
        recovery["recovery"]["files"] = json!(1);
        f.service.publish(op, recovery);
        assert_eq!(
            f.service.goal_status(op, 0, false).unwrap()["completion"]["nextAction"],
            "review_recovery"
        );
    }
    #[test]
    fn multibyte_scoped_notices_keep_counts_and_critical_completion_inside_escaped_budget() {
        let f = Owned::new(1);
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":1,"waitMs":2000}),
                || Ok(()),
            )
            .unwrap();
        let mut value = f
            .service
            .status(response["operationId"].as_str().unwrap())
            .unwrap();
        let mut events = Errors::default();
        let path = PathBuf::from(format!("/{}final-scope", "😀\"\\\n".repeat(1000)));
        for _ in 0..12 {
            push_scoped_issue(
                &mut events,
                IssueKind::CandidateRetention,
                "candidate_active",
                &"😀\"\\\n".repeat(90),
                &path,
            );
        }
        record_errors(&mut value, &events);
        for index in 0..6 {
            let child = format!("{index:032x}");
            start_child_detail(&mut value, &child, "rust_target", &path);
            settle_child_detail(
                &mut value,
                &child,
                &json!({"state":"partial","deletedFiles":1,"deletedLogicalBytes":4096,"deletedAllocatedBytes":4096,"recoveryFiles":0,"coordinationLocksCreated":0,"coordinationDetailsComplete":true}),
            );
        }
        value["deletedFiles"] = json!(6);
        value["deletedLogicalBytes"] = json!(6 * 4096);
        value["deletedAllocatedBytes"] = json!(6 * 4096);
        value["cacheEvidence"]["sourceRoots"] = json!(["\"".repeat(9000)]);
        value["operationInterrupted"] = json!(true);
        value["recovery"]["files"] = json!(2);
        completion_contract(&mut value);
        let bounded = budget_receipt(&value);
        assert!(
            encoded_receipt_size(&bounded) <= 7400,
            "{}",
            encoded_receipt_size(&bounded)
        );
        assert_eq!(bounded["completion"]["nextAction"], "review_recovery");
        assert_eq!(bounded["operationInterrupted"], true);
        assert_eq!(bounded["errorsCount"], 12);
        assert_eq!(bounded["effects"]["actionCount"], 6);
        assert_eq!(bounded["effects"]["deletedFiles"], 6);
        assert_eq!(bounded["effects"]["truncated"], true);
        assert_eq!(bounded["effects"]["detail"]["field"], "childDetails");
        assert_eq!(bounded["diagnostics"]["candidateRetentionEvents"], 12);
        assert_eq!(bounded["diagnostics"]["candidateScopes"]["count"], 12);
        assert_eq!(bounded["diagnostics"]["candidateScopes"]["truncated"], true);
        assert!(
            bounded["diagnostics"]["candidateScopes"]["samples"]
                .as_array()
                .unwrap()
                .len()
                <= 4
        );
        // The emergency path is independently bounded and retains critical state.
        value["coverage"]["cargoChildProbes"] = json!(CARGO_CHILD_PROBES);
        value["coverage"]["cargoChildProbeLimit"] = json!(CARGO_CHILD_PROBES);
        value["coverage"]["cargoChildProbesSkipped"] = json!(7);
        value["coverage"]["cargoChildProbeBudgetExhausted"] = json!(true);
        value["coverage"]["unexpectedLargeDetail"] = json!("x".repeat(9000));
        let minimum = budget_receipt(&value);
        assert!(encoded_receipt_size(&minimum) <= 7400);
        assert_eq!(minimum["completion"], bounded["completion"]);
        assert_eq!(minimum["recovery"]["files"], 2);
        assert_eq!(minimum["errorsCount"], 12);
        assert_eq!(minimum["effects"]["actionCount"], 6);
        assert_eq!(minimum["effects"]["deletedFiles"], 6);
        assert_eq!(minimum["effects"]["truncated"], true);
        assert_eq!(minimum["coverage"]["cargoChildProbes"], CARGO_CHILD_PROBES);
        assert_eq!(
            minimum["coverage"]["cargoChildProbeLimit"],
            CARGO_CHILD_PROBES
        );
        assert_eq!(minimum["coverage"]["cargoChildProbesSkipped"], 7);
        assert_eq!(minimum["coverage"]["cargoChildProbeBudgetExhausted"], true);
    }

    #[test]
    fn unavailable_inspection_is_typed_unknown_retention_not_active_or_operation_failure() {
        let f = Owned::new(1);
        *f.service.inner.process_guard.lock().unwrap() =
            Some(Err("Live process lookalike: inspection unavailable".into()));
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0);
        assert_eq!(result["coverage"]["protectedCandidates"], 0);
        assert!(result["coverage"]["unknownCandidates"].as_u64().unwrap() > 0);
        assert_eq!(result["diagnostics"]["candidateRetentionEvents"], 1);
        assert_eq!(result["diagnostics"]["operationIssueEvents"], 0);
        assert_eq!(
            result["diagnostics"]["samples"][0]["code"],
            "candidate_inspection_unavailable"
        );
        assert!(f.target.join("debug/deps/0.o").exists());
    }

    #[test]
    fn replaced_scope_revokes_a_previously_verified_completion() {
        let f = Owned::new(1);
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.target,"targetAvailableBytes":1,"waitMs":2000}),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(first["completion"]["nextAction"], "report_result");
        fs::rename(&f.target, f.target.with_file_name("old-owned-target")).unwrap();
        fs::create_dir(&f.target).unwrap();
        let result = f
            .service
            .goal_status(first["operationId"].as_str().unwrap(), 0, false)
            .unwrap();
        assert_eq!(result["completion"]["spaceGoalVerified"], false);
        assert_eq!(
            result["completion"]["nextAction"],
            "review_space_measurement"
        );
        assert!(result.get("actualSpaceError").is_some());
    }

    #[test]
    fn unattainable_goal_returns_real_partial_and_preserves_sources_and_unknown_data() {
        let f = Owned::new(600);
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["state"], "partial", "{result}");
        assert_eq!(result["deletedFiles"], 600);
        assert_eq!(result["goalReached"], false);
        assert_eq!(result["completion"]["nextAction"], "report_unmet_goal");
        assert_eq!(result["completion"]["spaceGoalVerified"], false);
        assert!(f.root.join("state.sqlite").exists());
        assert!(f.root.join("worktrees/project/src/lib.rs").exists());
        assert!(result["availableBytes"].is_u64());
        assert!(result["observedAvailableBytesDelta"].is_i64());
        assert!(serde_json::to_vec(&result).unwrap().len() < 8192);
        assert_eq!(
            fs::read_dir(f.target.join("debug/deps")).unwrap().count(),
            0
        );
    }
    #[test]
    fn live_requests_deduplicate_and_cancellation_during_barrier_retains_artifacts() {
        let f = Owned::new(2);
        let (entered, rx) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let args = json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0});
        let first = f
            .service
            .reclaim_with_before(&args, move || {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5))
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let duplicate = f
            .service
            .reclaim_with_before(&args, || Err::<(), _>("duplicate must not execute".into()))
            .unwrap();
        assert_eq!(first["operationId"], duplicate["operationId"]);
        assert_eq!(first["executionEvidence"]["state"], "in_progress");
        assert_eq!(
            first["executionEvidence"]["method"],
            "native_work_in_progress"
        );
        assert_eq!(first["effects"]["actionCount"], 0);
        let op = first["operationId"].as_str().unwrap();
        f.service.goal_status(op, 0, true).unwrap();
        release.send(()).unwrap();
        let result = f.wait(op);
        assert_eq!(result["state"], "cancelled");
        assert_eq!(result["completion"]["nextAction"], "report_cancelled");
        assert_eq!(result["deletedFiles"], 0);
        assert!(f.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn exact_execution_rechecks_expiry_after_queue_and_honors_existing_volume_goal() {
        let f = Owned::new(600);
        let cancel = AtomicBool::new(false);
        let mut plan = f
            .service
            .build_plan(&id().unwrap(), "rust_target", &f.target, &cancel)
            .unwrap();
        plan.created = Some(Instant::now() - Duration::from_secs(PLAN_SECONDS + 1));
        let child = id().unwrap();
        f.service
            .insert(
                &child,
                json!({"jobId":child,"state":"running"}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        f.service.run_execution_context(
            &child,
            &plan,
            &cancel,
            || Ok(()),
            ExecutionContext::default(),
        );
        let expired = f.service.status(&child).unwrap();
        assert_eq!(expired["state"], "failed");
        assert!(expired["error"].as_str().unwrap().contains("expired"));
        assert!(f.target.join("debug/deps/0.o").exists());
        plan.created = Some(Instant::now());
        let child = id().unwrap();
        f.service
            .insert(
                &child,
                json!({"jobId":child,"state":"running"}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let (_, scope) = safe_root(&f.root).unwrap();
        let goal = GoalLimit {
            scope: f.root.clone(),
            identity: Identity::metadata(&scope.metadata().unwrap()),
            target: 1,
        };
        f.service.run_execution_context(
            &child,
            &plan,
            &cancel,
            || Ok(()),
            ExecutionContext {
                goal: Some(goal),
                ..ExecutionContext::default()
            },
        );
        let stopped = f.service.status(&child).unwrap();
        assert_eq!(stopped["deletedFiles"], 0);
        assert_eq!(stopped["unprocessedFiles"], 600);
        assert!(f.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn unknown_provider_readonly_and_symlink_scope_cannot_mutate() {
        use std::os::unix::fs::symlink;
        let f = Owned::new(1);
        assert!(f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":1,"providers":["arbitrary"]}),
                || Ok(())
            )
            .is_err());
        let readonly = AgentCleanup::new(f.root.join("readonly"), false).unwrap();
        assert!(readonly
            .reclaim_with_before(&json!({"scope":f.root,"targetAvailableBytes":1}), || Ok(()))
            .is_err());
        let alias = f.root.join("alias");
        symlink(&f.target, &alias).unwrap();
        let result = f
            .service
            .reclaim_with_before(
                &json!({"scope":alias,"targetAvailableBytes":1,"waitMs":2000}),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(result["state"], "failed");
        assert!(f.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn idle_cargo_lock_inode_is_preserved_and_busy_lock_blocks_even_with_process_override() {
        let f = Owned::new(3);
        let lock = f.target.join("debug/.cargo-lock");
        fs::write(&lock, b"").unwrap();
        let original = lock.metadata().unwrap().ino();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let blocked = f.wait(first["operationId"].as_str().unwrap());
        assert_eq!(blocked["deletedFiles"], 0);
        assert_eq!(blocked["coverage"]["protectedCandidates"], 1);
        assert!(f.target.join("debug/deps/0.o").exists());
        drop(file);
        let second = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let done = f.wait(second["operationId"].as_str().unwrap());
        assert_eq!(done["deletedFiles"], 3);
        assert_eq!(lock.metadata().unwrap().ino(), original);
    }
    #[test]
    fn narrow_scope_does_not_create_a_missing_parent_coordination_lock() {
        let f = Owned::new(2);
        let scope = f.target.join("debug/deps");
        let (_, directory) = safe_root(&f.root).unwrap();
        let (_, total) = volume_available(&directory).unwrap();
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":scope,"targetAvailableBytes":total-1,"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let blocked = f.wait(first["operationId"].as_str().unwrap());
        assert_eq!(blocked["deletedFiles"], 0);
        assert!(!f.target.join("debug/.cargo-lock").exists());
        assert!(f.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn escaped_budget_preserves_operation_identity_and_post_publication_volume_truth() {
        let f = Owned::new(1);
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":1,"waitMs":2000}),
                || Ok(()),
            )
            .unwrap();
        let op = first["operationId"].as_str().unwrap();
        let mut stored = f.service.status(op).unwrap();
        let (_, file) = safe_root(&f.root).unwrap();
        stored["targetAvailableBytes"] = json!(volume_available(&file).unwrap().1);
        stored["availableBytes"] = json!(u64::MAX);
        stored["goalReached"] = json!(true);
        stored["cacheEvidence"]["sourceRoots"] = json!(["\"".repeat(5000)]);
        f.service.publish(op, stored);
        let result = f.service.goal_status(op, 0, false).unwrap();
        assert_eq!(result["operationId"], op);
        assert_eq!(result["state"], "partial");
        assert_eq!(result["goalReached"], false);
        assert!(result["availableBytes"].as_u64().unwrap() < u64::MAX);
        assert_eq!(result["receiptTruncated"], true);
        assert_eq!(result["completion"]["spaceGoalVerified"], false);
        assert_ne!(result["completion"]["nextAction"], "report_result");
        assert!(encoded_receipt_size(&result) < 8192);
    }
    #[test]
    fn real_mixed_size_deletion_bounds_batches_by_bytes_and_reports_atomic_large_file() {
        let f = Owned::new(0);
        for i in 0..9 {
            fs::write(
                f.target.join(format!("debug/deps/mixed-{i}")),
                vec![i as u8; 4 * 1024 * 1024],
            )
            .unwrap();
        }
        let large = f.target.join("debug/deps/large");
        fs::write(&large, vec![7u8; 24 * 1024 * 1024]).unwrap();
        let largest = large.metadata().unwrap().blocks() * 512;
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 10, "{result}");
        assert_eq!(result["largestDeletedFileAllocatedBytes"], largest);
        assert!(
            result["maxBatchAllocatedBytes"].as_u64().unwrap()
                <= BATCH_ALLOCATED_BYTES.max(largest),
            "{result}"
        );
        assert_eq!(result["batchAllocatedLimitBytes"], BATCH_ALLOCATED_BYTES);
        let operation = result["recovery"]["operationIds"][0].as_str().unwrap();
        let bindings: Value = serde_json::from_slice(
            &fs::read(
                f.service
                    .inner
                    .journal
                    .join(format!("{operation}.coordination.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(bindings["parentOperationId"], result["operationId"]);
        let created = &bindings["locks"][0];
        assert_eq!(created["created"], true);
        assert_eq!(
            created["ino"],
            f.target.join("debug/.cargo-lock").metadata().unwrap().ino()
        );
    }
    fn git_fixture(cwd: &Path, args: &[&str]) {
        let output = Command::new("/usr/bin/git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-C",
            ])
            .arg(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn factual_gitfile_protection_counts_links_and_source_events_without_tracked_exclusions() {
        use std::os::unix::fs::symlink;
        let f = Owned::new(0);
        let repo = f.root.join("repository");
        fs::create_dir(&repo).unwrap();
        git_fixture(&repo, &["init", "--quiet"]);
        fs::write(repo.join("README"), b"owned tracked source").unwrap();
        git_fixture(&repo, &["add", "README"]);
        git_fixture(&repo, &["commit", "--quiet", "-m", "owned"]);
        let linked = f.root.join("linked");
        git_fixture(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(
            linked.join("Cargo.toml"),
            b"[package]\nname='owned'\nversion='0.0.0'\n",
        )
        .unwrap();
        let target = linked.join("target/debug");
        fs::create_dir_all(target.join("embedded")).unwrap();
        fs::write(target.join("artifact.o"), b"rebuildable").unwrap();
        fs::write(target.join("embedded/Cargo.toml"), b"owned nested source").unwrap();
        fs::write(target.join("embedded/input.o"), b"source-shaped object").unwrap();
        fs::hard_link(linked.join("README"), target.join("input.o")).unwrap();
        symlink(linked.join("README"), target.join("reference.o")).unwrap();
        let before = linked.join("README").metadata().unwrap().ino();
        let first = f
            .service
            .reclaim_with_before(
                &json!({"scope":linked,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let operation = first["operationId"].as_str().unwrap();
        let done = f.wait(operation);
        let proof = &done["protectionEvidence"];
        assert_eq!(done["deletedFiles"], 1);
        assert_eq!(done["coverage"]["gitTrackedFiles"], 0);
        assert_eq!(proof["state"], "recorded");
        assert_eq!(proof["plan"]["gitInspected"], 1);
        assert_eq!(proof["execution"]["gitInspected"], 1);
        for key in ["hardLinks", "symlinks", "sourceBoundaries"] {
            assert_eq!(proof["plan"]["retained"][key], 1, "{proof}");
            assert_eq!(proof["execution"]["retained"][key], 0);
        }
        assert_eq!(proof["plan"]["retained"]["gitTrackedEntries"], 0);
        assert_eq!(proof["revalidation"]["metadataBindingChecks"], 1);
        assert_eq!(proof["revalidation"]["successfulReloads"], 0);
        assert_eq!(proof["detailsDurable"], true);
        assert!(
            protection_wire_bytes(proof) <= 1536,
            "{} {proof}",
            protection_wire_bytes(proof)
        );
        let durable = f.service.status(operation).unwrap();
        let git = &durable["childDetails"][0]["protectionEvidence"]["execution"]["git"];
        assert_eq!(git["state"], "repository_inspected");
        assert_eq!(git["indexPresent"], true);
        assert!(git["markerIdentity"].is_array());
        assert!(git["indexIdentity"].is_array());
        assert_eq!(
            f.service.goal_status(operation, 0, false).unwrap()["protectionEvidence"],
            *proof
        );
        assert_eq!(linked.join("README").metadata().unwrap().ino(), before);
        assert_eq!(
            fs::read(linked.join("README")).unwrap(),
            b"owned tracked source"
        );
        assert!(target
            .join("reference.o")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.join("embedded/input.o").exists());
    }

    #[test]
    fn protection_summary_deduplicates_phases_and_survives_both_wire_fallbacks() {
        let mut evidence = ProtectionObservations::default();
        evidence.plan.git = git::Observation::unavailable();
        for phase in [&mut evidence.plan, &mut evidence.execution] {
            phase.retained = RetainedObservations {
                hard_links: u64::MAX,
                symlinks: u64::MAX,
                symlink_referents: u64::MAX,
                source_boundaries: u64::MAX,
                other_retentions: u64::MAX,
                git_tracked_entries: u64::MAX,
            };
        }
        evidence.revalidation = RevalidationObservations {
            attempts: u64::MAX,
            metadata_binding_checks: u64::MAX,
            successful_reloads: u64::MAX,
            failures: u64::MAX,
        };
        let row = json!({"childOperationId":"a".repeat(32),"settled":true,"protectionEvidence":evidence,"successfulReferenceChecks":u64::MAX,"incomingReferenceCoverage":{"state":"scoped_closure_inspected","complete":true},"processReferenceCoverage":{"state":"scoped_process_observation"}});
        let mut value = json!({"operationId":"b".repeat(32),"state":"partial","mutationSettled":true,"durabilityIncomplete":true,"recovery":{"files":1},"completion":{"nextAction":"review_durability","spaceGoalVerified":false,"cleanupSettled":false},"childDetails":[row.clone(),row],"errors":[],"errorsCount":0});
        let proof = protection_summary(&value);
        assert_eq!(proof["childCount"], 1);
        assert_eq!(proof["settledChildCount"], 1);
        assert_eq!(proof["plan"]["unavailable"], 1);
        assert_eq!(proof["execution"]["notReached"], 1);
        assert_eq!(proof["plan"]["retained"]["hardLinks"], u64::MAX);
        assert_eq!(proof["detailsDurable"], false);
        assert_eq!(proof["references"]["freshChecks"], u64::MAX);
        assert!(
            protection_wire_bytes(&proof) <= 1536,
            "{} {proof}",
            protection_wire_bytes(&proof)
        );
        value["protectionEvidence"] = proof;
        value["errors"] = json!(["😀\"\\\n".repeat(10000)]);
        value["errorsCount"] = json!(1);
        value["coverage"]["large"] = json!("😀\"\\\n".repeat(10000));
        for receipt in [budget_receipt(&value), minimal_receipt(&value)] {
            assert!(encoded_receipt_size(&receipt) <= 8192);
            assert_eq!(receipt["completion"]["nextAction"], "review_durability");
            assert_eq!(receipt["durabilityIncomplete"], true);
            assert_eq!(
                receipt["protectionEvidence"]["plan"],
                value["protectionEvidence"]["plan"]
            );
            assert_eq!(
                receipt["protectionEvidence"]["execution"],
                value["protectionEvidence"]["execution"]
            );
            assert_eq!(
                receipt["protectionEvidence"]["detail"]["operationId"],
                value["operationId"]
            );
            assert_eq!(receipt["protectionEvidence"]["detailsDurable"], false);
            assert_eq!(
                receipt["protectionEvidence"]["references"],
                value["protectionEvidence"]["references"]
            );
            assert!(protection_wire_bytes(&receipt["protectionEvidence"]) <= 1536);
        }
    }
    #[test]
    fn reference_summary_distinguishes_success_unknown_and_failed_inspection() {
        let evidence = json!(ProtectionObservations::default());
        let failed = json!({"childOperationId":"a".repeat(32),"settled":true,"protectionEvidence":evidence,"successfulReferenceChecks":2,"incomingReferenceCoverage":{"state":"inspection_unavailable","complete":false},"processReferenceCoverage":{"state":"scoped_process_observation"},"failureDisposition":"inspection_unavailable"});
        let legacy =
            json!({"childOperationId":"b".repeat(32),"settled":true,"protectionEvidence":evidence});
        let active = json!({"childOperationId":"c".repeat(32),"settled":true,"protectionEvidence":evidence,"successfulReferenceChecks":0,"incomingReferenceCoverage":{"state":"not_reached"},"processReferenceCoverage":{"state":"observed_active"},"failureDisposition":"observed_active"});
        let partial = json!({"childOperationId":"f".repeat(32),"settled":true,"protectionEvidence":evidence,"successfulReferenceChecks":0});
        let running = json!({"childOperationId":"d".repeat(32),"settled":false,"successfulReferenceChecks":99});
        let value = json!({"operationId":"e".repeat(32),"childDetails":[failed.clone(),failed,legacy,active,running,partial]});
        let proof = protection_summary(&value);
        assert_eq!(proof["references"]["freshChecks"], 2);
        assert_eq!(proof["references"]["unavailableChildren"], 1);
        assert_eq!(proof["references"]["unknownChildren"], 2);
        assert_eq!(proof["incomplete"], true);
    }
    #[test]
    fn genuine_gitfile_worktree_retains_tracked_object_but_deletes_untracked_cache() {
        let f = Owned::new(0);
        let repo = f.root.join("repository");
        fs::create_dir(&repo).unwrap();
        git_fixture(&repo, &["init", "--quiet"]);
        fs::write(repo.join("README"), b"owned source").unwrap();
        git_fixture(&repo, &["add", "README"]);
        git_fixture(&repo, &["commit", "--quiet", "-m", "owned fixture"]);
        let linked = f.root.join("linked");
        git_fixture(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        assert!(linked.join(".git").is_file());
        fs::write(
            linked.join("Cargo.toml"),
            b"[package]
name='linked'
version='0.0.0'
",
        )
        .unwrap();
        let outputs = linked.join("target/debug");
        fs::create_dir_all(&outputs).unwrap();
        let tracked = outputs.join("tracked.o");
        let untracked = outputs.join("untracked.o");
        fs::write(&tracked, b"tracked input").unwrap();
        fs::write(&untracked, b"rebuildable").unwrap();
        git_fixture(&linked, &["add", "-f", "target/debug/tracked.o"]);
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":linked,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 1, "{result}");
        assert_eq!(result["coverage"]["gitTrackedFiles"], 1);
        assert_eq!(fs::read(&tracked).unwrap(), b"tracked input");
        assert!(!untracked.exists());
    }
    #[test]
    fn tracked_after_preview_is_revalidated_and_broken_git_metadata_retains_candidate() {
        let f = Owned::new(2);
        git_fixture(&f.root, &["init", "--quiet"]);
        let cancel = AtomicBool::new(false);
        let plan = f
            .service
            .build_plan(&id().unwrap(), "rust_target", &f.target, &cancel)
            .unwrap();
        assert_eq!(plan.entries.len(), 2);
        git_fixture(
            &f.root,
            &["add", "-f", "worktrees/project/target/debug/deps/0.o"],
        );
        let child = id().unwrap();
        f.service
            .insert(
                &child,
                json!({"jobId":child,"state":"running"}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        f.service.run_execution(&child, &plan, &cancel, || Ok(()));
        let result = f.service.status(&child).unwrap();
        assert_eq!(result["deletedFiles"], 1, "{result}");
        assert_eq!(result["gitTrackedFiles"], 1);
        assert_eq!(
            result["protectionEvidence"]["plan"]["git"]["indexPresent"],
            false
        );
        assert_eq!(
            result["protectionEvidence"]["execution"]["git"]["indexPresent"],
            true
        );
        assert_eq!(
            result["protectionEvidence"]["execution"]["retained"]["gitTrackedEntries"],
            1
        );
        assert!(f.target.join("debug/deps/0.o").exists());
        let broken = Owned::new(1);
        fs::write(
            broken.root.join(".git"),
            b"gitdir: missing-owned-metadata\n",
        )
        .unwrap();
        let response=broken.service.reclaim_with_before(&json!({"scope":broken.root,"targetAvailableBytes":broken.unattainable(),"waitMs":0}),||Ok(())).unwrap();
        let result = broken.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0);
        assert!(broken.target.join("debug/deps/0.o").exists());
        assert!(result["errorsCount"].as_u64().unwrap() > 0);
        assert_eq!(result["protectionEvidence"]["available"], false);
        assert_eq!(result["protectionEvidence"]["childCount"], 0);
        let unavailable = Owned::new(1);
        git_fixture(&unavailable.root, &["init", "--quiet"]);
        let plan = unavailable
            .service
            .build_plan(&id().unwrap(), "rust_target", &unavailable.target, &cancel)
            .unwrap();
        fs::write(
            unavailable.root.join(".git/index"),
            b"invalid owned index after planning",
        )
        .unwrap();
        let child = id().unwrap();
        unavailable
            .service
            .insert(
                &child,
                json!({"jobId":child,"state":"running"}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        unavailable
            .service
            .run_execution(&child, &plan, &cancel, || Ok(()));
        let outcome = unavailable.service.status(&child).unwrap();
        assert_eq!(outcome["deletedFiles"], 0);
        assert_eq!(
            outcome["protectionEvidence"]["plan"]["git"]["state"],
            "repository_inspected"
        );
        assert_eq!(
            outcome["protectionEvidence"]["execution"]["git"]["state"],
            "unavailable"
        );
        assert_eq!(outcome["failureDisposition"], "execution_issue");
        assert!(unavailable.target.join("debug/deps/0.o").exists());
    }
    #[test]
    fn embedded_git_without_manifest_is_a_protected_source_boundary() {
        let f = Owned::new(2);
        let embedded = f.target.join("debug/embedded");
        fs::create_dir(&embedded).unwrap();
        git_fixture(&embedded, &["init", "--quiet"]);
        let tracked = embedded.join("tracked.o");
        fs::write(&tracked, b"embedded source input").unwrap();
        git_fixture(&embedded, &["add", "tracked.o"]);
        let inode = tracked.metadata().unwrap().ino();
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 2, "{result}");
        assert_eq!(tracked.metadata().unwrap().ino(), inode);
        assert_eq!(fs::read(&tracked).unwrap(), b"embedded source input");
        assert!(embedded.join(".git").is_dir());
    }
    #[test]
    fn source_markers_added_after_preview_protect_leaf_and_ancestors_above_selected_scope() {
        for (narrow, marker, location) in [
            (false, ".git", "debug/deps"),
            (true, ".git", ""),
            (true, "Cargo.toml", ""),
        ] {
            let f = Owned::new(2);
            fs::write(f.target.join("debug/.cargo-lock"), b"").unwrap();
            let scope = if narrow {
                f.target.join("debug/deps")
            } else {
                f.target.clone()
            };
            let cancel = AtomicBool::new(false);
            let plan = f
                .service
                .build_plan(&id().unwrap(), "rust_target", &scope, &cancel)
                .unwrap();
            let new_marker = f.target.join(location).join(marker);
            fs::write(new_marker, b"new source boundary").unwrap();
            let child = id().unwrap();
            f.service
                .insert(
                    &child,
                    json!({"jobId":child,"state":"running"}),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            f.service.run_execution(&child, &plan, &cancel, || Ok(()));
            let result = f.service.status(&child).unwrap();
            assert_eq!(result["deletedFiles"], 0, "{result}");
            assert!(f.target.join("debug/deps/0.o").exists());
            assert!(f.target.join("debug/deps/1.o").exists());
        }
    }
    #[test]
    fn real_open_active_input_with_absent_cargo_lock_creates_no_coordination_inode() {
        struct Actor(std::process::Child);
        impl Drop for Actor {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let f = Owned::new(2);
        *f.service.inner.process_guard.lock().unwrap() = None;
        let _actor = Actor(
            Command::new("/bin/sleep")
                .arg("30")
                .stdin(File::open(f.target.join("debug/deps/0.o")).unwrap())
                .spawn()
                .unwrap(),
        );
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0, "{result}");
        assert_eq!(result["coverage"]["protectedCandidates"], 1, "{result}");
        assert_eq!(result["protectionEvidence"]["execution"]["notReached"], 1);
        assert_eq!(result["protectionEvidence"]["execution"]["gitInspected"], 0);
        assert_eq!(result["protectionEvidence"]["execution"]["noMarker"], 0);
        assert!(!f.target.join("debug/.cargo-lock").exists());
        assert!(f.target.join("debug/deps/0.o").exists());
        assert!(f.target.join("debug/deps/1.o").exists());
    }
    #[test]
    fn journal_publication_failure_retains_operation_identity_and_explicit_durability_error() {
        let f = Owned::new(2);
        let saved = f.root.join("saved-journal");
        fs::rename(&f.service.inner.journal, &saved).unwrap();
        fs::write(&f.service.inner.journal, b"owned journal failure fixture").unwrap();
        let response = f
            .service
            .reclaim_with_before(
                &json!({"scope":f.root,"targetAvailableBytes":f.unattainable(),"waitMs":0}),
                || Ok(()),
            )
            .unwrap();
        let result = f.wait(response["operationId"].as_str().unwrap());
        assert_eq!(result["deletedFiles"], 0);
        assert_eq!(result["durabilityIncomplete"], true);
        assert_eq!(result["completion"]["nextAction"], "review_durability");
        assert!(result["journalError"].is_string());
        assert!(result["operationId"].is_string());
        assert!(f.target.join("debug/deps/0.o").exists());
        assert!(result["errorsCount"].as_u64().unwrap() > 0);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_tracked_case_and_unicode_aliases_preserve_objects_and_allow_untracked_cleanup() {
        for (leaf, alias, folder, folder_alias) in [
            (
                "TrackedInput.o",
                "trackedinput.o",
                "incremental",
                "incremental",
            ),
            (
                "Trackedé.o",
                "Trackede\u{301}.o",
                "incremental",
                "incremental",
            ),
            (
                "tracked.o",
                "tracked.o",
                "IncrementalÉ",
                "incrementale\u{301}",
            ),
        ] {
            let f = Owned::new(0);
            let directory = f.target.join("debug").join(folder);
            fs::create_dir(&directory).unwrap();
            let tracked = directory.join(leaf);
            fs::write(&tracked, b"source tracked object must survive").unwrap();
            git_fixture(&f.root, &["init", "--quiet", "--template="]);
            git_fixture(&f.root, &["config", "core.precomposeunicode", "true"]);
            let indexed = format!("worktrees/project/target/debug/{folder}/{leaf}");
            git_fixture(&f.root, &["add", "-f", &indexed]);
            let inode = tracked.metadata().unwrap().ino();
            if folder != folder_alias {
                let alias_directory = f.target.join("debug").join(folder_alias);
                if !alias_directory.exists() {
                    eprintln!("Filesystem does not expose requested case/Unicode directory alias; APFS native suite covers this guard");
                    continue;
                }
                fs::rename(&directory, &alias_directory).unwrap();
            }
            let scope = f.target.join("debug").join(folder_alias);
            let original = scope.join(leaf);
            let alias_path = scope.join(alias);
            if leaf != alias {
                if !alias_path.exists() {
                    eprintln!("Filesystem does not expose requested case/Unicode filename alias; APFS native suite covers this guard");
                    continue;
                }
                fs::rename(&original, &alias_path).unwrap();
            }
            assert_eq!(alias_path.metadata().unwrap().ino(), inode);
            let untracked = scope.join("normal-untracked.o");
            fs::write(&untracked, b"rebuildable output").unwrap();
            fs::write(f.target.join("debug/.cargo-lock"), b"").unwrap();
            let cancel = AtomicBool::new(false);
            let plan = f
                .service
                .build_plan(&id().unwrap(), "rust_target", &scope, &cancel)
                .unwrap();
            assert_eq!(
                plan.entries.len(),
                1,
                "alias was planned as rebuildable: {indexed}"
            );
            assert_eq!(plan.git_tracked_files, 1);
            let child = id().unwrap();
            f.service
                .insert(
                    &child,
                    json!({"jobId":child,"state":"running"}),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            f.service.run_execution(&child, &plan, &cancel, || Ok(()));
            let result = f.service.status(&child).unwrap();
            assert_eq!(result["deletedFiles"], 1, "{result}");
            assert_eq!(
                fs::read(&alias_path).unwrap(),
                b"source tracked object must survive"
            );
            assert_eq!(alias_path.metadata().unwrap().ino(), inode);
            assert!(!untracked.exists());
        }
    }
}
