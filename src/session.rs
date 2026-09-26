use crate::{
    config::{Config, Project},
    memory::{MemoryStore, Source, id},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod run_history;
pub use run_history::{RUN_HISTORY_LIMIT, RunRecord};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    pub done: bool,
    #[serde(default)]
    pub result: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskState {
    pub purpose: String,
    pub scope: String,
    pub deliverables: Vec<String>,
    pub constraints: Vec<String>,
    pub completion: Vec<String>,
    pub require_investigation: bool,
    pub workflow: String,
    pub todos: Vec<TodoItem>,
    pub plan_revision: u64,
    pub todo_sequence: u64,
    pub todos_completed_total: usize,
    pub checkpoint_summary: String,
    pub findings: Vec<String>,
    // Read old snapshots, then migrate once before running. These fields are
    // neither exposed to the model nor maintained alongside the ordered plan.
    #[serde(skip_serializing)]
    pub done: Vec<String>,
    #[serde(skip_serializing)]
    pub current: String,
    pub phase: String,
    #[serde(skip_serializing)]
    pub next: String,
    pub unresolved: Vec<String>,
    pub memory_ids: Vec<String>,
    pub details: Vec<Value>,
    pub revision: u64,
}
impl TaskState {
    pub fn current_todo(&self) -> Option<&TodoItem> {
        self.todos.iter().find(|item| !item.done)
    }

    pub fn migrate_legacy_plan(&mut self) {
        let current = std::mem::take(&mut self.current);
        let next = std::mem::take(&mut self.next);
        if self.checkpoint_summary.is_empty() {
            self.checkpoint_summary = current;
        }
        let mut legacy: Vec<_> = std::mem::take(&mut self.done)
            .into_iter()
            .map(|text| (text, true))
            .collect();
        if self.todos.is_empty() {
            legacy.push((next, false));
        }
        for (text, done) in legacy {
            let text: String = text.trim().chars().take(160).collect();
            if text.is_empty() || self.todos.iter().any(|item| item.text == text) {
                continue;
            }
            self.todo_sequence = self.todo_sequence.saturating_add(1);
            self.todos.push(TodoItem {
                id: format!("T{}", self.todo_sequence),
                text,
                done,
                result: String::new(),
            });
            self.todos_completed_total += usize::from(done);
            self.plan_revision = self.plan_revision.saturating_add(1);
        }
        while self.todos.iter().filter(|item| item.done).count() > 5 {
            let at = self.todos.iter().position(|item| item.done).unwrap();
            self.todos.remove(at);
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Investigation {
    pub id: String,
    pub title: String,
    pub status: String,
    pub memory_refs: BTreeMap<String, u64>,
    pub sources: Vec<Source>,
    pub section: String,
    pub document_hash: Option<String>,
    pub note: String,
}
impl Investigation {
    /// Source IDs this item was last verified with. After a document edit
    /// the item returns to "written" but keeps them; a checkpoint may have
    /// dropped them from the model's context (a live run looped 20 rounds).
    pub fn source_ids(&self) -> Vec<String> {
        self.sources
            .iter()
            .take(30)
            .map(|source| source.id.clone())
            .collect()
    }
    /// Verified items and closing-mode gaps are settled. A gap is reported to
    /// the user as unconfirmed; it never counts as verified evidence.
    pub fn is_settled(&self) -> bool {
        matches!(self.status.as_str(), "verified" | "gap")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub id: u64,
    pub messages: Vec<Value>,
    pub active: bool,
    pub reviewed: bool,
    pub complete: bool,
}
#[derive(Clone, Debug, Default)]
pub struct SessionHistory {
    pub bundles: VecDeque<Bundle>,
    pub next_id: u64,
    pub pruned_through: Option<u64>,
}
impl SessionHistory {
    pub fn push(&mut self, messages: Vec<Value>, complete: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.bundles.push_back(Bundle {
            id,
            messages,
            active: true,
            reviewed: false,
            complete,
        });
        id
    }
    pub fn bytes(&self) -> usize {
        self.bundles
            .iter()
            .map(|b| serde_json::to_vec(b).unwrap().len())
            .sum()
    }
    pub fn active(&self) -> Vec<Value> {
        self.bundles
            .iter()
            .filter(|b| b.active)
            .flat_map(|b| b.messages.clone())
            .collect()
    }
    pub fn prune(&mut self, limit: usize) -> Result<()> {
        let mut bytes = self.bytes();
        let mut remove = 0;
        for first in &self.bundles {
            if bytes <= limit {
                break;
            }
            if first.active || !first.reviewed || !first.complete {
                bail!("history_capacity: checkpoint required; original history retained");
            }
            bytes = bytes.saturating_sub(serde_json::to_vec(first)?.len());
            remove += 1;
        }
        for _ in 0..remove {
            self.pruned_through = self.bundles.pop_front().map(|b| b.id);
        }
        Ok(())
    }
    pub fn read(&self, id: u64) -> Result<&Bundle> {
        self.bundles.iter().find(|b| b.id == id).ok_or_else(|| {
            anyhow::anyhow!(
                "history_unavailable: pruned_through={:?}",
                self.pruned_through
            )
        })
    }
    pub fn search(&self, query: &str, after: u64, limit: usize) -> Value {
        let q = query.to_lowercase();
        let rows: Vec<_> = self
            .bundles
            .iter()
            .filter(|b| b.id > after)
            .filter(|b| {
                serde_json::to_string(&b.messages)
                    .unwrap()
                    .to_lowercase()
                    .contains(&q)
            })
            .collect();
        let items:Vec<_>=rows.iter().take(limit.clamp(1,50)).map(|b|json!({"id":b.id,"active":b.active,"excerpt":serde_json::to_string(&b.messages).unwrap().chars().take(240).collect::<String>()})).collect();
        json!({"next_cursor":if rows.len()>items.len(){items.last().and_then(|x|x["id"].as_u64())}else{None},"items":items,"pruned_through":self.pruned_through})
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub bundle_ids: Vec<u64>,
    #[serde(default)]
    pub maintenance_bundle_ids: Vec<u64>,
    pub acknowledged: bool,
    pub attempts: usize,
    #[serde(default)]
    pub failed_attempts: usize,
    #[serde(default)]
    pub last_failure: Option<String>,
    #[serde(default)]
    pub source_lookup_calls: usize,
    pub starting_state_revision: u64,
    pub starting_memory_generation: u64,
    pub failed: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct FileCursor {
    pub path: String,
    pub hash: String,
    pub start_line: usize,
    pub max_lines: usize,
    pub offset: usize,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ReadCoverage {
    pub hash: String,
    /// Half-open Unicode character ranges in LF-normalized text.
    pub ranges: Vec<(usize, usize)>,
}

/// Runtime progress, retained when the same task is resumed. A new user task
/// resets it; merely restarting a run cannot make a repeated result new work.
#[derive(Clone, Debug, Default)]
pub struct ProgressRecovery {
    /// A rejected document final must return to tools before trying to finish.
    pub action_required: bool,
    /// The batch being executed answers a forced repair step, so non-repair
    /// tools are refused while it runs (action_required is already cleared).
    pub repair_step: bool,
    /// Recovery thresholds change the approach; document work retains its budget.
    pub recovery_reason: Option<String>,
    pub rounds_without_progress: usize,
    /// New navigation pages alone cannot keep a stalled task alive forever.
    pub rounds_without_substantive_progress: usize,
    pub repeated_read: bool,
    pub repeated_outcome_rounds: usize,
    pub artifact_edits_without_milestone: usize,
    pub finalization_attempts: usize,
    pub best_document_section_count: usize,
    pub best_document_content_lines: usize,
    pub seen_artifact_versions: VecDeque<String>,
    pub seen_artifact_paths: VecDeque<String>,
    pub seen_navigation_results: VecDeque<String>,
    /// Highest progress score reached in this run and model requests since.
    /// One monotonic measure prevents recovery counters from resetting each other.
    pub best_score: usize,
    pub rounds_since_best: usize,
    /// Distinct delivered sources credited as progress. Frozen once the run
    /// leaves the investigate phase, where only result improvements count.
    pub evidence_credit: usize,
    /// Set once document work must converge: exploration stops and the run
    /// finishes within a fixed number of requests, reporting unresolved items.
    pub closing: Option<Closing>,
    /// Final answers rejected by an unrepaired document review while the
    /// reviewed document stayed unchanged (keyed by that document's hash).
    pub unrepaired_finals: usize,
    pub unrepaired_final_hash: Option<String>,
    /// Hash of the rejected document when a checkpoint cleared the context
    /// during review repair; the next requests restate the findings until
    /// the document changes.
    pub review_repair_resume_hash: Option<String>,
    /// Set when a document-work response hit the output limit: a whole
    /// document rewrite is withheld until a smaller edit succeeds.
    pub whole_write_withheld: bool,
    /// Successful verifications of items that were not verified before the
    /// call (a first verification or one after its section changed). Repair
    /// work re-verifies sections, which the current verified count hides.
    pub verification_events: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Closing {
    /// "budget" when the reserve is reached, "stall" after sustained no progress.
    pub reason: String,
    /// Non-review model requests (and failed review retries) since closing began.
    pub rounds: usize,
    /// Final answers submitted during closing. The second accepts reported gaps.
    pub final_attempts: usize,
    pub document_review_used: bool,
    pub completion_review_used: bool,
}

impl ProgressRecovery {
    pub fn remember_artifact_path(&mut self, path: String) -> bool {
        if self.seen_artifact_paths.contains(&path) {
            return false;
        }
        self.seen_artifact_paths.push_back(path);
        while self.seen_artifact_paths.len() > 256 {
            self.seen_artifact_paths.pop_front();
        }
        true
    }

    pub fn remember_artifact(&mut self, path: String, digest: String) -> bool {
        let version = format!(
            "{:x}",
            Sha256::digest(format!("{path}\0{digest}").as_bytes())
        );
        if self.seen_artifact_versions.contains(&version) {
            return false;
        }
        self.seen_artifact_versions.push_back(version);
        while self.seen_artifact_versions.len() > 256 {
            self.seen_artifact_versions.pop_front();
        }
        true
    }

    pub fn remember_navigation(&mut self, tool: &str, data: &Value) -> bool {
        let mut stable = data.clone();
        if let Some(fields) = stable.as_object_mut() {
            for key in ["archive_id", "cursor", "next_cursor"] {
                fields.remove(key);
            }
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(format!("{tool}\0{stable}").as_bytes())
        );
        if self.seen_navigation_results.contains(&digest) {
            return false;
        }
        self.seen_navigation_results.push_back(digest);
        while self.seen_navigation_results.len() > 256 {
            self.seen_navigation_results.pop_front();
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub project: Project,
    pub config: Config,
    pub pending_config: Option<Config>,
    pub task: TaskState,
    /// Workflow the user selected for this session's requests (one of
    /// WORKFLOW_MODES); applied to every new task.
    pub workflow_mode: String,
    pub memory: MemoryStore,
    pub history: SessionHistory,
    pub sources: BTreeMap<String, Source>,
    pub file_cursors: BTreeMap<String, FileCursor>,
    pub read_coverage: BTreeMap<String, ReadCoverage>,
    /// Coverage page revisions let legacy offset callers detect intervening
    /// reads even when they do not echo expected_coverage_revision.
    pub coverage_cursors: BTreeMap<String, String>,
    pub active_tools: BTreeSet<String>,
    pub pending_tools: Option<BTreeSet<String>>,
    pub investigations: Vec<Investigation>,
    pub checkpoint: Option<Checkpoint>,
    pub ledger: BTreeMap<String, (String, Value)>,
    pub latest_request: String,
    pub status: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_tokens: Option<usize>,
    pub usage_incomplete: bool,
    pub reviews: usize,
    pub checkpoints_completed: usize,
    pub memory_loads: usize,
    pub history_loads: usize,
    pub document_written: bool,
    pub last_document_write: Option<(std::path::PathBuf, String)>,
    pub last_error: Option<String>,
    /// Finished executions survive new questions within this session.
    pub run_history: VecDeque<RunRecord>,
    active_run: Option<run_history::ActiveRun>,
    pub run_guidance: Value,
    pub progress_recovery: ProgressRecovery,
    /// Unresolved items reported with a complete_with_gaps result.
    pub completion_gaps: Vec<String>,
    /// Recent provider input tokens per locally estimated token, for models
    /// without a known tokenizer. See ContextManager::token_ratio.
    pub token_ratios: VecDeque<f64>,
    /// file_list cursor fingerprint -> the mode and path_glob it was issued
    /// for, so a continuation that omits them keeps its original scope.
    pub list_cursor_scopes: VecDeque<(String, Value)>,
    pub activity: Value,
    pub task_rounds: usize,
    pub document_review: crate::tools::document_review::ReviewState,
    pub completion_review: crate::tools::completion_review::ReviewState,
    pub answer_draft: Option<String>,
    pub answer_reviewed: bool,
    pub answer_review_original: Option<String>,
    pub answer_review_issues: Vec<String>,
    pub answer_review_input_tokens: usize,
    pub answer_review_output_tokens: usize,
    pub answer_review_start: u64,
    pub answer_review_question: String,
    // Some(true): truncated tool batch; Some(false): text continuation.
    pub continuation: Option<bool>,
}
/// Workflows a user selects for a session in the session window.
pub const WORKFLOW_MODES: [&str; 3] = ["answer", "source_document", "document_edit"];

impl Session {
    /// Select how this session's requests are handled and apply it to the
    /// current task. The model cannot change the selection.
    pub fn select_workflow(&mut self, mode: &str) -> anyhow::Result<()> {
        if !WORKFLOW_MODES.contains(&mode) {
            anyhow::bail!("invalid_workflow: use one of {WORKFLOW_MODES:?}");
        }
        self.workflow_mode = mode.into();
        self.apply_workflow_mode();
        Ok(())
    }

    /// Apply the user's workflow selection to the current task and enable its
    /// tools. A new request resets the task, so this runs for each one.
    pub fn apply_workflow_mode(&mut self) {
        let require_investigation = self.workflow_mode == "source_document";
        if self.task.workflow != self.workflow_mode
            || self.task.require_investigation != require_investigation
        {
            self.task.workflow = self.workflow_mode.clone();
            self.task.require_investigation = require_investigation;
            self.task.revision = self.task.revision.saturating_add(1);
        }
        self.activate_workflow_tools();
    }

    /// Document workflows need their edit and verification tools active.
    pub fn activate_workflow_tools(&mut self) {
        if self.task.require_investigation || self.task.workflow == "document_edit" {
            for name in [
                "investigation",
                "document_edit",
                "document_edit_batch",
                "document_audit",
            ] {
                self.active_tools.insert(name.into());
                if let Some(pending) = &mut self.pending_tools {
                    pending.insert(name.into());
                }
            }
        }
    }

    pub fn is_document_work(&self) -> bool {
        self.task.require_investigation
            || matches!(
                self.task.workflow.as_str(),
                "source_document" | "document_edit"
            )
            || self.document_written
            || !self.investigations.is_empty()
    }

    fn initial_completion(&self, request: &str) -> Vec<String> {
        let request = request.trim();
        let max_chars = (self.config.state_tokens / 6).clamp(24, 320);
        let excerpt: String = request.chars().take(max_chars).collect();
        let suffix = if request.chars().count() > max_chars {
            "… (전체 요청은 latest_request 참고)"
        } else {
            ""
        };
        vec![format!(
            "사용자 요청의 명시 요구를 충족한다: {excerpt}{suffix}"
        )]
    }

    pub fn new(project: Project, config: Config) -> Self {
        let task = TaskState {
            purpose: project.purpose.clone(),
            scope: project.root.display().to_string(),
            // A configured output is a possible destination, not a requested deliverable.
            deliverables: vec![],
            // The default selection; see workflow_mode.
            workflow: "answer".into(),
            ..Default::default()
        };
        Self {
            id: id(),
            project,
            config,
            pending_config: None,
            task,
            memory: Default::default(),
            history: Default::default(),
            sources: BTreeMap::new(),
            file_cursors: BTreeMap::new(),
            read_coverage: BTreeMap::new(),
            coverage_cursors: BTreeMap::new(),
            active_tools: [
                "file_read",
                "file_edit",
                "file_write",
                "file_patch",
                "document_inspect",
                "file_list",
                "source_search",
                "code_outline",
                "symbol_read",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            pending_tools: None,
            workflow_mode: "answer".into(),
            investigations: vec![],
            checkpoint: None,
            ledger: BTreeMap::new(),
            latest_request: String::new(),
            status: "idle".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: None,
            usage_incomplete: false,
            reviews: 0,
            checkpoints_completed: 0,
            memory_loads: 0,
            history_loads: 0,
            document_written: false,
            last_document_write: None,
            last_error: None,
            run_history: VecDeque::new(),
            active_run: None,
            run_guidance: json!({}),
            progress_recovery: Default::default(),
            completion_gaps: vec![],
            token_ratios: VecDeque::new(),
            list_cursor_scopes: VecDeque::new(),
            activity: json!({}),
            task_rounds: 0,
            document_review: Default::default(),
            completion_review: Default::default(),
            answer_draft: None,
            answer_reviewed: false,
            answer_review_original: None,
            answer_review_issues: vec![],
            answer_review_input_tokens: 0,
            answer_review_output_tokens: 0,
            answer_review_start: 0,
            answer_review_question: String::new(),
            continuation: None,
        }
    }
    pub fn protected(&self) -> BTreeSet<String> {
        self.task
            .memory_ids
            .iter()
            // Task state historically accepted either a memory ID or key.
            // Resolve aliases here as well as at write time so older sessions
            // cannot delete a memory that is still pinned by its key.
            .map(|ident| self.canonical_memory_id(ident))
            .chain(
                self.investigations
                    .iter()
                    .flat_map(|i| i.memory_refs.keys())
                    .map(|ident| self.canonical_memory_id(ident)),
            )
            .collect()
    }
    /// Resolve both current IDs and legacy human-readable memory keys.
    /// Unknown references are retained so protection remains conservative.
    pub fn canonical_memory_id(&self, ident: &str) -> String {
        self.memory
            .get(ident)
            .map(|memory| memory.id.clone())
            .unwrap_or_else(|_| ident.to_string())
    }
    pub fn source_refs(&self, ids: &[String]) -> Result<Vec<Source>> {
        ids.iter()
            .map(|id| {
                self.sources
                    .get(id)
                    .cloned()
                    .or_else(|| {
                        self.memory
                            .entries
                            .values()
                            .flat_map(|m| m.sources.iter())
                            .find(|s| &s.id == id)
                            .cloned()
                    })
                    .ok_or_else(|| {
                        let mut candidates: Vec<_> = self.sources.values().collect();
                        candidates.sort_by_key(|s| std::cmp::Reverse(s.observed_at));
                        let choices: Vec<_> = candidates.into_iter().take(8).map(|s| json!({"id":s.id,"path":s.path,"start_line":s.start_line,"end_line":s.end_line})).collect();
                        anyhow::anyhow!("unknown_source: {id}; Use source_lookup with the matching path to recover an observed ID, or history to inspect the original result. A compact code_outline is navigation only and supplies no evidence ID. If no matching evidence exists, record missing evidence in checkpoint_complete.progress and the current task_plan item and read the source after checkpoint completion; do not save an unsupported fact. Do not substitute unrelated IDs or remove source_ids to bypass this error. Recent sources (not automatic replacements): {}", json!(choices))
                    })
            })
            .collect()
    }
    pub fn add_user(&mut self, text: String) {
        let first_request = self.latest_request.is_empty() && self.history.bundles.is_empty();
        let continuation = matches!(
            text.trim()
                .trim_end_matches(['.', '!'])
                .to_lowercase()
                .as_str(),
            "계속 진행" | "계속" | "이어서 진행" | "continue" | "resume"
        );
        if !continuation {
            // Tool-call IDs are scoped to one model request sequence. Retaining
            // successful results across a new user task can replay a stale read
            // or suppress a new mutation if a provider reuses an ID.
            self.ledger.clear();
            self.completion_review = Default::default();
            self.completion_review.required = first_request && !self.task.completion.is_empty();
            self.answer_draft = None;
            self.answer_reviewed = false;
            self.answer_review_original = None;
            self.answer_review_issues.clear();
            self.answer_review_input_tokens = 0;
            self.answer_review_output_tokens = 0;
            self.answer_review_start = self.history.next_id + 1;
            self.answer_review_question = text.clone();
            self.continuation = None;
            // The first prompt may follow a caller's task_state setup. Keep
            // that plan; later non-continuation messages start a new task.
            if first_request {
                self.task.revision = self.task.revision.saturating_add(1);
            } else {
                // Keep explicit user constraints as session safety rules,
                // but discard the previous task's plan and review state.
                let revision = self.task.revision.saturating_add(1);
                let constraints = std::mem::take(&mut self.task.constraints);
                self.task = TaskState {
                    purpose: self.project.purpose.clone(),
                    scope: self.project.root.display().to_string(),
                    constraints,
                    revision,
                    ..Default::default()
                };
                self.investigations.clear();
                self.reviews = 0;
                self.document_review = Default::default();
                self.document_written = false;
                self.last_document_write = None;
                self.task_rounds = 0;
                self.run_guidance = json!({});
                self.progress_recovery = Default::default();
                self.completion_gaps.clear();
            }
            if self.task.completion.is_empty() {
                self.task.completion = self.initial_completion(&text);
            }
            self.apply_workflow_mode();
        }
        self.latest_request = text.clone();
        let source = Source {
            id: crate::memory::source_id(),
            observed_at: chrono::Utc::now(),
            origin: "user".into(),
            path: None,
            start_line: None,
            end_line: None,
            line_start_complete: true,
            line_end_complete: true,
            evidence_truncated: false,
            hash: None,
            excerpt: text.chars().take(2000).collect(),
        };
        self.sources.insert(source.id.clone(), source);
        self.history
            .push(vec![json!({"role":"user","content":text})], true);
    }
    /// Add a maintenance request without starting a new user task. Cleanup
    /// must keep the current workflow, evidence requirements and review state
    /// so a pending settings change cannot silently weaken completion checks.
    pub fn add_maintenance(&mut self, text: String) {
        self.ledger.clear();
        self.latest_request = text.clone();
        self.history
            .push(vec![json!({"role":"user","content":text})], true);
    }
    /// Return the serialized size of session metadata that is retained outside
    /// the memory and history stores. Runtime turns use the same bound to
    /// avoid silently discarding observations or receipts.
    pub fn ancillary_bytes(&self) -> usize {
        serde_json::to_vec(&(
            &self.sources,
            &self.ledger,
            &self.investigations,
            &self.task,
            &self.file_cursors,
            &self.read_coverage,
            &self.coverage_cursors,
        ))
        .map_or(usize::MAX, |v| v.len())
    }
    pub fn check_limits(&self, c: &Config) -> Result<()> {
        c.validate()?;
        if self.memory.entries.len() > c.memory_count
            || self.memory.bytes() > c.memory_bytes
            || self.history.bytes() > c.history_bytes
            || self.ancillary_bytes() > c.memory_bytes
        {
            bail!("New limits require cleanup first; current settings retained");
        }
        if self
            .memory
            .entries
            .values()
            .any(|m| m.body.len() > c.memory_body_bytes)
        {
            bail!("Existing memory exceeds new body limit");
        }
        Ok(())
    }
}
