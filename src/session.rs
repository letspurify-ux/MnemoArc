use crate::{
    config::{Config, Project},
    memory::{MemoryStore, Source, id, serialized_bytes},
    shared::Shared,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, atomic::AtomicBool};

mod run_history;
pub use run_history::{RUN_HISTORY_LIMIT, RunRecord};

#[derive(Clone, Debug)]
pub struct FollowUpQuestion {
    pub text: String,
    pub(crate) bundle_id: u64,
    pub(crate) prior_status: String,
    pub(crate) prior_error: Option<String>,
    pub(crate) prior_activity: Value,
    pub(crate) prior_rounds: usize,
    pub(crate) automatic: bool,
}

/// A user-authorized change, retained independently of compacted conversation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskAmendment {
    pub request: String,
    pub goal: Option<String>,
    pub completion: Option<Vec<String>>,
    pub constraints: Option<Vec<String>>,
    pub deliverables: Option<Vec<String>>,
    pub retire_investigation_ids: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    pub done: bool,
    #[serde(default)]
    pub result: String,
    /// Why a completed item was reopened; cleared when it completes again.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reopen_reason: String,
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

/// Criteria supplied before a user request starts. The agent may refine
/// TaskState while working, but those working checks must not become new
/// requirements for document or completion review.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestReviewCriteria {
    pub completion: Vec<String>,
    pub constraints: Vec<String>,
    pub deliverables: Vec<String>,
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
                reopen_reason: String::new(),
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
        matches!(self.status.as_str(), "verified" | "gap" | "superseded")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub id: u64,
    pub messages: Shared<Vec<Value>>,
    pub active: bool,
    pub reviewed: bool,
    pub complete: bool,
}
impl Bundle {
    fn bytes(&self) -> usize {
        // Only flags and the ID change during retirement. Count their small
        // envelope and reuse the immutable messages' serialized byte count.
        #[derive(Serialize)]
        struct Envelope {
            id: u64,
            messages: [Value; 0],
            active: bool,
            reviewed: bool,
            complete: bool,
        }
        let envelope = Envelope {
            id: self.id,
            messages: [],
            active: self.active,
            reviewed: self.reviewed,
            complete: self.complete,
        };
        serialized_bytes(&envelope)
            .saturating_sub(2)
            .saturating_add(self.messages.bytes())
    }
}
#[derive(Clone, Debug, Default)]
pub struct SessionHistory {
    pub bundles: Shared<VecDeque<Bundle>>,
    pub next_id: u64,
    pub pruned_through: Option<u64>,
}
impl SessionHistory {
    pub fn push(&mut self, messages: Vec<Value>, complete: bool) -> u64 {
        self.push_shared(messages.into(), complete)
    }
    pub(crate) fn push_shared(&mut self, messages: Shared<Vec<Value>>, complete: bool) -> u64 {
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
            .fold(0usize, |total, bundle| total.saturating_add(bundle.bytes()))
    }
    pub(crate) fn check_append(&self, messages: Vec<Value>, limit: usize) -> Result<()> {
        let bundle = Bundle {
            id: self.next_id.saturating_add(1),
            messages: messages.into(),
            active: true,
            reviewed: false,
            complete: true,
        };
        if self.bytes().saturating_add(bundle.bytes()) > limit {
            bail!(
                "history_capacity: request exceeds retained history capacity; clean up history or start another session"
            );
        }
        Ok(())
    }
    pub fn active(&self) -> Vec<Value> {
        self.bundles
            .iter()
            .filter(|b| b.active)
            .flat_map(|b| b.messages.clone())
            .collect()
    }
    pub fn prune(&mut self, limit: usize) -> Result<()> {
        self.prune_retiring(limit, &BTreeSet::new())
    }
    pub(crate) fn prune_retiring(&mut self, limit: usize, retired: &BTreeSet<u64>) -> Result<()> {
        // Plan removal using the flags a confirmed checkpoint will publish.
        // A failed plan changes nothing and needs no copy of retained messages.
        let retained_bytes = |bundle: &Bundle| {
            let bytes = bundle.bytes();
            if retired.contains(&bundle.id) {
                // JSON `false` is one byte longer than `true`. Account for
                // the final flags even at an exact history capacity boundary.
                bytes
                    .saturating_add(usize::from(bundle.active))
                    .saturating_sub(usize::from(!bundle.reviewed))
            } else {
                bytes
            }
        };
        let mut bytes = self.bundles.iter().fold(0usize, |total, bundle| {
            total.saturating_add(retained_bytes(bundle))
        });
        let mut remove = 0;
        for first in &self.bundles {
            if bytes <= limit {
                break;
            }
            if !first.complete
                || ((first.active || !first.reviewed) && !retired.contains(&first.id))
            {
                bail!("history_capacity: checkpoint required; original history retained");
            }
            bytes = bytes.saturating_sub(retained_bytes(first));
            remove += 1;
        }
        for bundle in &mut self.bundles {
            if retired.contains(&bundle.id) {
                bundle.active = false;
                bundle.reviewed = true;
            }
        }
        for _ in 0..remove {
            self.pruned_through = self.bundles.pop_front().map(|b| b.id);
        }
        // Retiring messages also has to release their deque slots. Otherwise
        // an empty history (or lower configured limit) retains its peak storage.
        // Keep spare capacity for ordinary small checkpoints.
        if remove > 0 && self.bundles.capacity() > self.bundles.len().saturating_mul(2) {
            self.bundles.shrink_to_fit();
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Debug, Default, Serialize)]
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
    /// "budget" when the reserve is reached, "stall" after sustained no progress,
    /// "review_unrepaired" after unchanged rejected finals, "empty_response"
    /// after consecutive empty model replies.
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
    /// Shared across web sessions after an external write times out with an
    /// unknown outcome. Further runs require review and a process restart.
    pub write_outcome_uncertain: Arc<AtomicBool>,
    pub pending_config: Option<Config>,
    pub task: TaskState,
    pub request_review_criteria: RequestReviewCriteria,
    /// Workflow selected when the session is created (one of WORKFLOW_MODES).
    pub workflow_mode: String,
    pub workflow_locked: bool,
    pub original_request: String,
    pub current_request: String,
    pub task_amendments: Vec<TaskAmendment>,
    /// A query executes on a private copy with a runtime-enforced read allowlist.
    pub read_only_turn: bool,
    pub memory: MemoryStore,
    pub history: SessionHistory,
    pub sources: Shared<BTreeMap<String, Source>>,
    pub file_cursors: Shared<BTreeMap<String, FileCursor>>,
    pub read_coverage: Shared<BTreeMap<String, ReadCoverage>>,
    /// Coverage page revisions let legacy offset callers detect intervening
    /// reads even when they do not echo expected_coverage_revision.
    pub coverage_cursors: Shared<BTreeMap<String, String>>,
    pub active_tools: BTreeSet<String>,
    pub pending_tools: Option<BTreeSet<String>>,
    pub investigations: Vec<Investigation>,
    pub checkpoint: Option<Checkpoint>,
    pub ledger: Shared<BTreeMap<String, (String, Value)>>,
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
    pub question: Option<FollowUpQuestion>,
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
    /// file_list cursor fingerprint -> its mode and file or directory scope,
    /// so a continuation that omits them keeps its original scope.
    pub list_cursor_scopes: VecDeque<(String, Value)>,
    pub activity: Value,
    pub task_rounds: usize,
    pub document_review: crate::tools::document_review::ReviewState,
    pub completion_review: crate::tools::completion_review::ReviewState,
    /// First task history id and current requirements with change provenance;
    /// document and completion reviews compare results against them.
    pub answer_review_start: u64,
    pub answer_review_question: String,
    // Some(true): truncated tool batch; Some(false): text continuation.
    pub continuation: Option<bool>,
}
/// Workflows a user selects when creating a session.
/// General file/document work runs in answer without document verification.
pub const WORKFLOW_MODES: [&str; 2] = ["answer", "source_document"];

impl Session {
    /// Select how this session's requests are handled and apply it to the
    /// current task. The model cannot change the selection.
    pub fn select_workflow(&mut self, mode: &str) -> anyhow::Result<()> {
        if !WORKFLOW_MODES.contains(&mode) {
            anyhow::bail!("invalid_workflow: use one of {WORKFLOW_MODES:?}");
        }
        if self.workflow_locked && self.workflow_mode != mode {
            bail!("workflow_locked: choose a different workflow in a new session");
        }
        self.workflow_mode = mode.into();
        self.apply_workflow_mode();
        Ok(())
    }

    /// Apply the user's workflow selection to the current task and enable its
    /// tools, including when the initial task is created.
    pub fn apply_workflow_mode(&mut self) {
        let require_investigation = self.workflow_mode == "source_document";
        if self.task.workflow != self.workflow_mode
            || self.task.require_investigation != require_investigation
        {
            self.task.workflow = self.workflow_mode.clone();
            self.task.require_investigation = require_investigation;
            self.task.revision = self.task.revision.saturating_add(1);
        }
        self.drop_forbidden_workflow_tools();
        self.activate_workflow_tools();
    }

    /// Optional tools the user's workflow selection excludes. Chat answers
    /// do not track source-documentation items or run reviews, so
    /// `investigation` (whose final_check would turn the task into document
    /// work) and `document_audit` are withheld.
    pub fn workflow_forbidden_tools(&self) -> &'static [&'static str] {
        if self.workflow_mode == "answer" {
            &["investigation", "document_audit"]
        } else {
            &[]
        }
    }

    /// Remove tools the current workflow selection excludes, including from a
    /// selection still waiting for the next request.
    pub fn drop_forbidden_workflow_tools(&mut self) {
        for name in self.workflow_forbidden_tools() {
            self.active_tools.remove(*name);
            if let Some(pending) = &mut self.pending_tools {
                pending.remove(*name);
            }
        }
    }

    /// Document workflows need their edit and verification tools active.
    pub fn activate_workflow_tools(&mut self) {
        if self.task.require_investigation {
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

    /// Whether document verification (progress recovery, closing, the
    /// verify phase and final document checks) governs this task. A document
    /// written in the answer workflow is a plain edit, not document work.
    pub fn is_document_work(&self) -> bool {
        self.task.require_investigation
            || self.task.workflow == "source_document"
            || !self.investigations.is_empty()
    }

    pub fn can_resume(&self) -> bool {
        let status = self
            .question
            .as_ref()
            .map_or(self.status.as_str(), |question| {
                question.prior_status.as_str()
            });
        !self.latest_request.is_empty()
            && (matches!(
                status,
                "blocked" | "partial" | "cancelled" | "complete_with_gaps"
            ) || self.investigations.iter().any(|item| !item.is_settled())
                || self.checkpoint.is_some()
                || self.continuation.is_some()
                || self.task.current_todo().is_some()
                || self.document_review.pending
                || self.completion_review.pending
                || !self.completion_gaps.is_empty())
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

    pub fn new(mut project: Project, config: Config) -> Self {
        project.ensure_id();
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
            write_outcome_uncertain: Arc::new(AtomicBool::new(false)),
            pending_config: None,
            task,
            request_review_criteria: Default::default(),
            memory: Default::default(),
            history: Default::default(),
            sources: Shared::default(),
            file_cursors: Shared::default(),
            read_coverage: Shared::default(),
            coverage_cursors: Shared::default(),
            active_tools: [
                "file_read",
                "file_edit",
                "file_write",
                "file_patch",
                // Simple edits of the configured output run in answer.
                "document_edit",
                "document_edit_batch",
                "document_inspect",
                "file_list",
                "source_search",
                "code_outline",
                "symbol_search",
                "symbol_relations",
                "symbol_read",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            pending_tools: None,
            workflow_mode: "answer".into(),
            workflow_locked: false,
            original_request: String::new(),
            current_request: String::new(),
            task_amendments: vec![],
            read_only_turn: false,
            investigations: vec![],
            checkpoint: None,
            ledger: Shared::default(),
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
            question: None,
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
    pub fn queue_question(&mut self, text: String) -> Result<()> {
        if text.trim().is_empty() {
            bail!("empty_question: enter a question");
        }
        if self.latest_request.is_empty() {
            bail!("no_current_task: start a task before asking about it");
        }
        if self.question.is_some() || self.status == "running" {
            bail!("session_busy: wait for the current run to finish");
        }
        // Reserve room for both messages without pruning the suspended task's history.
        let message = json!({"role":"user","content":text,"follow_up":true});
        let reserved = serde_json::to_vec(&message)?
            .len()
            .saturating_add(self.config.output_tokens.saturating_mul(32))
            .saturating_add(4096);
        if self.history.bytes().saturating_add(reserved) > self.config.history_bytes {
            bail!(
                "history_capacity: not enough space for a follow-up answer; clean up history first"
            );
        }
        let bundle_id = self.history.push(vec![message], true);
        let bundle = self.history.bundles.back_mut().unwrap();
        bundle.active = false;
        bundle.reviewed = true;
        self.question = Some(FollowUpQuestion {
            text,
            bundle_id,
            prior_status: self.status.clone(),
            prior_error: self.last_error.clone(),
            prior_activity: self.activity.clone(),
            prior_rounds: self.task_rounds,
            automatic: false,
        });
        Ok(())
    }

    /// The public message entry point: one task is initialized per session.
    pub fn receive_message(&mut self, text: String) -> Result<()> {
        let mut next = self.clone();
        next.receive_message_inner(text)?;
        *self = next;
        Ok(())
    }

    fn receive_message_inner(&mut self, text: String) -> Result<()> {
        if text.trim().is_empty() {
            bail!("empty_message: enter a message");
        }
        if self.latest_request.is_empty() {
            self.history.check_append(
                vec![json!({"role":"user","content":text})],
                self.config.history_bytes,
            )?;
            self.start_new_task(text);
        } else {
            self.queue_question(text)?;
            self.question.as_mut().unwrap().automatic = true;
        }
        self.check_runtime_capacity()
    }

    /// Promote a routed message without replacing the task's plan or evidence.
    pub(crate) fn accept_amendment(&mut self, mut amendment: TaskAmendment) -> Result<()> {
        let question = self
            .question
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no_pending_message"))?;
        amendment.request = question.text.clone();
        if amendment
            .goal
            .as_ref()
            .is_some_and(|goal| goal.trim().is_empty())
        {
            bail!("invalid_task_goal: goal must not be empty");
        }
        for values in [
            &amendment.completion,
            &amendment.constraints,
            &amendment.deliverables,
        ]
        .into_iter()
        .flatten()
        {
            if values.len() > 100 || values.iter().any(|value| value.trim().is_empty()) {
                bail!("invalid_task_requirements: use at most 100 non-empty entries");
            }
        }
        if let Some(ids) = &amendment.retire_investigation_ids
            && (amendment.goal.is_none()
                || ids.len() > 100
                || ids
                    .iter()
                    .any(|id| !self.investigations.iter().any(|item| item.id == *id)))
        {
            bail!(
                "invalid_task_requirements: retiring evidence items requires a changed goal and existing investigation IDs"
            );
        }
        let changed_requirements = amendment.goal.is_some();
        let explicit_requirements_changed = amendment.goal.is_some()
            || amendment.completion.is_some()
            || amendment.constraints.is_some()
            || amendment.deliverables.is_some();
        let mut next = self.clone();
        let question = next.question.take().unwrap();
        next.current_request = question.text.clone();
        next.observe_user(&question.text);
        if let Some(goal) = &amendment.goal {
            next.latest_request = goal.clone();
        }
        if let Some(values) = &amendment.completion {
            next.request_review_criteria.completion = values.clone();
            next.task.completion = if values.is_empty() {
                next.initial_completion(&next.latest_request)
            } else {
                values.clone()
            };
        }
        if let Some(values) = &amendment.constraints {
            next.request_review_criteria.constraints = values.clone();
            next.task.constraints = values.clone();
        }
        if let Some(values) = &amendment.deliverables {
            next.request_review_criteria.deliverables = values.clone();
            next.task.deliverables = values.clone();
        }
        if amendment.goal.is_some() && amendment.completion.is_none() {
            next.task.completion = next.initial_completion(&next.latest_request);
        }
        if let Some(ids) = &amendment.retire_investigation_ids {
            for item in &mut next.investigations {
                if ids.contains(&item.id) {
                    item.status = "superseded".into();
                    item.note = format!("User changed the task scope: {}", question.text);
                }
            }
        }
        next.task_amendments.push(amendment);
        next.answer_review_question = json!({
            "current_goal":next.latest_request,"initial_request":next.original_request,
            "user_changes":next.task_amendments,
            "policy":"The latest explicit user change supersedes an earlier conflicting requirement. Unchanged requirements remain in force. Initial request and change history are provenance, not additional requirements to reimpose."
        }).to_string();
        if let Some(bundle) = next
            .history
            .bundles
            .iter_mut()
            .find(|bundle| bundle.id == question.bundle_id)
        {
            bundle.active = true;
            for message in &mut bundle.messages {
                message.as_object_mut().unwrap().remove("follow_up");
                message["task_update"] = json!(true);
            }
        }
        next.ledger.clear();
        next.continuation = None;
        next.task.phase.clear();
        next.task.revision = next.task.revision.saturating_add(1);
        next.progress_recovery = Default::default();
        next.run_guidance = json!({});
        next.completion_gaps.clear();
        // Keep artifacts, investigations and review history. Requirements hashes
        // invalidate old verdicts even when the document itself is unchanged.
        next.document_review.pending = false;
        next.document_review.approved_hash = None;
        next.document_review.repair_started_round = None;
        next.document_review.repair_requests = 0;
        if changed_requirements {
            next.document_review.invalidate_requirements();
        }
        // Routing sets the current status to running; only the status saved
        // before this message tells whether the previous work was complete.
        next.completion_review.invalidate_requirements(
            explicit_requirements_changed,
            question.prior_status == "complete",
        );
        if let Some(cp) = &mut next.checkpoint {
            cp.attempts = 0;
            cp.acknowledged = false;
            cp.failed_attempts = 0;
            cp.last_failure = None;
            cp.failed = false;
        }
        next.promote_run_to_work();
        next.check_runtime_capacity()?;
        *self = next;
        Ok(())
    }

    pub(crate) fn restore_after_question(&mut self) {
        if let Some(question) = self.question.take() {
            self.status = question.prior_status;
            self.last_error = question.prior_error;
            self.activity = question.prior_activity;
            self.task_rounds = question.prior_rounds;
        }
    }

    pub fn is_continuation(text: &str) -> bool {
        matches!(
            text.trim()
                .trim_end_matches(['.', '!'])
                .to_lowercase()
                .as_str(),
            "계속 진행" | "계속" | "이어서 진행" | "continue" | "resume"
        )
    }

    pub fn add_user(&mut self, text: String) {
        let continuation = Self::is_continuation(&text);
        self.add_request(text, continuation);
    }

    pub fn start_new_task(&mut self, text: String) {
        self.add_request(text, false);
        // An explicit new task must not acknowledge the old task's unfinished
        // checkpoint. Its active history remains available for fresh cleanup.
        self.checkpoint = None;
        self.finish_maintenance();
    }

    fn observe_user(&mut self, text: &str) {
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
    }

    fn add_request(&mut self, text: String, continuation: bool) {
        let first_request = self.latest_request.is_empty() && self.history.bundles.is_empty();
        if !continuation {
            // Tool-call IDs are scoped to one model request sequence. Retaining
            // successful results across a new user task can replay a stale read
            // or suppress a new mutation if a provider reuses an ID.
            self.ledger.clear();
            self.completion_review = Default::default();
            self.completion_review.required = self.config.completion_review_enabled
                && first_request
                && !self.task.completion.is_empty();
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
                // A new task no longer needs these slots. clear() keeps the
                // previous task's peak allocation outside the metadata budget.
                self.investigations = Vec::new();
                self.reviews = 0;
                self.document_review = Default::default();
                self.document_written = false;
                self.last_document_write = None;
                self.task_rounds = 0;
                self.run_guidance = json!({});
                self.progress_recovery = Default::default();
                self.completion_gaps = Vec::new();
            }
            // Capture caller-provided criteria before initial_completion and
            // later model task_state updates add working acceptance checks.
            // On later requests, task.constraints can contain checks the agent
            // added during the previous task; retain only the earlier caller
            // constraints for document and completion reviews.
            self.request_review_criteria = RequestReviewCriteria {
                completion: self.task.completion.clone(),
                constraints: if first_request {
                    self.task.constraints.clone()
                } else {
                    self.request_review_criteria.constraints.clone()
                },
                deliverables: self.task.deliverables.clone(),
            };
            if self.task.completion.is_empty() {
                self.task.completion = self.initial_completion(&text);
            }
            self.apply_workflow_mode();
        }
        // A resume message belongs in history, but must not replace the task
        // requirements used after checkpointing and by completion reviews.
        if !continuation || self.latest_request.is_empty() {
            self.latest_request = text.clone();
            self.original_request = text.clone();
            self.task_amendments.clear();
        }
        self.current_request = text.clone();
        self.observe_user(&text);
        self.history
            .push(vec![json!({"role":"user","content":text})], true);
    }
    /// Add a maintenance request without starting a new user task. Cleanup
    /// must keep the current workflow, evidence requirements and review state
    /// so a pending settings change cannot silently weaken completion checks.
    pub fn add_maintenance(&mut self, text: String) {
        self.finish_maintenance();
        self.history.push(
            vec![json!({"role":"user","content":text,"maintenance":true})],
            true,
        );
    }

    /// Cleanup instructions belong to one run, not to the resumed user task.
    pub(crate) fn finish_maintenance(&mut self) {
        for bundle in &mut self.history.bundles {
            if bundle
                .messages
                .iter()
                .any(|message| message["maintenance"] == true)
            {
                bundle.active = false;
                // A cancelled cleanup may still have a checkpoint referring
                // to this bundle. Hide the instruction, but retain the record
                // until that checkpoint commits and releases its history.
                bundle.reviewed = !self.checkpoint.as_ref().is_some_and(|checkpoint| {
                    checkpoint.bundle_ids.contains(&bundle.id)
                        || checkpoint.maintenance_bundle_ids.contains(&bundle.id)
                });
            }
        }
    }

    pub(crate) fn remember_list_scope(&mut self, fingerprint: String, scope: Value) {
        if !self
            .list_cursor_scopes
            .iter()
            .any(|(known, _)| *known == fingerprint)
        {
            self.list_cursor_scopes.push_back((fingerprint, scope));
            while self.list_cursor_scopes.len() > 32 {
                self.list_cursor_scopes.pop_front();
            }
        }
    }
    /// Return the serialized size of session metadata that is retained outside
    /// the memory and history stores. Runtime turns use the same bound to
    /// avoid silently discarding observations or receipts.
    pub fn ancillary_bytes(&self) -> usize {
        serialized_bytes(&(
            &self.sources,
            &self.ledger,
            &self.investigations,
            &self.task,
            &self.file_cursors,
            &self.read_coverage,
            &self.coverage_cursors,
            &self.request_review_criteria,
            &self.checkpoint,
            &self.config,
            &self.pending_config,
            &self.project,
            &self.active_tools,
            &self.pending_tools,
            &self.workflow_mode,
            &self.workflow_locked,
        ))
        .saturating_add(serialized_bytes(&(
            &self.latest_request,
            &self.original_request,
            &self.current_request,
            &self.task_amendments,
            &self.status,
            &self.last_error,
            self.question.as_ref().map(|q| {
                (
                    &q.text,
                    &q.prior_status,
                    &q.prior_error,
                    &q.prior_activity,
                    q.automatic,
                )
            }),
            &self.run_guidance,
            &self.progress_recovery,
            &self.completion_gaps,
            &self.token_ratios,
            &self.list_cursor_scopes,
            &self.activity,
            &self.answer_review_question,
            &self.run_history,
            self.active_document_review_failure(),
        )))
        .saturating_add(self.document_review.retained_bytes())
        .saturating_add(self.completion_review.retained_bytes())
        .saturating_add(self.config.api_key.as_ref().map_or(0, |key| key.0.len()))
        .saturating_add(
            self.pending_config
                .as_ref()
                .and_then(|c| c.api_key.as_ref())
                .map_or(0, |key| key.0.len()),
        )
    }
    pub(crate) fn check_runtime_capacity(&self) -> Result<()> {
        if self.ancillary_bytes() > self.config.memory_bytes {
            bail!("session_metadata_capacity: start another session or reduce retained details");
        }
        if self.history.bytes() > self.config.history_bytes.saturating_mul(2) {
            bail!("history_hard_limit: cleanup required");
        }
        Ok(())
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

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn removed_scope_keeps_history_without_requiring_its_deleted_section_or_stale_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("input.txt"), "Kept content\n").unwrap();
        std::fs::write(
            dir.path().join("out.md"),
            "# Kept\nKept content. input.txt:1-1\n",
        )
        .unwrap();
        let mut s = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("out.md"),
                ..Default::default()
            },
            Config::default(),
        );
        s.select_workflow("source_document").unwrap();
        s.receive_message("Write kept and removed chapters".into())
            .unwrap();
        let read = crate::tools::execute(&mut s, "file_read", json!({"path":"input.txt"})).unwrap();
        crate::tools::execute(&mut s, "investigation", json!({"action":"upsert","id":"kept","title":"Kept","section":"# Kept","status":"written"})).unwrap();
        crate::tools::execute(&mut s, "investigation", json!({"action":"verify","id":"kept","source_ids":[read["source"]["id"]],"verification_note":"Compared kept content with input.txt:1"})).unwrap();
        let mut old_source = s
            .source_refs(&[read["source"]["id"].as_str().unwrap().into()])
            .unwrap()
            .remove(0);
        old_source.hash = Some("stale-removed-evidence".into());
        s.investigations.push(Investigation {
            id: "removed".into(),
            title: "Removed".into(),
            section: "# Removed".into(),
            status: "written".into(),
            sources: vec![old_source],
            memory_refs: Default::default(),
            document_hash: None,
            note: String::new(),
        });
        s.receive_message("Remove the second chapter from the goal".into())
            .unwrap();
        s.accept_amendment(TaskAmendment {
            goal: Some("Write the kept chapter".into()),
            retire_investigation_ids: Some(vec!["removed".into()]),
            ..Default::default()
        })
        .unwrap();
        let audit = crate::tools::execute(&mut s, "document_audit", json!({})).unwrap();
        assert_eq!(audit["structural_ok"], true, "{audit}");
        assert_eq!(s.investigations[1].status, "superseded");
        let error = crate::tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":"removed","status":"written","section":"# Kept"}),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("investigation_superseded"));
        assert_eq!(s.investigations[1].status, "superseded");
        s.receive_message("Reintroduce the removed chapter".into())
            .unwrap();
        s.accept_amendment(TaskAmendment {
            goal: Some("Write the kept and reintroduced chapters".into()),
            ..Default::default()
        })
        .unwrap();
        crate::tools::execute(&mut s, "investigation", json!({"action":"upsert","id":"new-scope","title":"Removed","section":"# Reintroduced","status":"uninvestigated"})).unwrap();
        assert_eq!(s.investigations.len(), 3);
    }

    #[test]
    fn explicit_changes_preserve_artifacts_and_unaffected_work_and_invalidate_old_reviews() {
        let mut s = Session::new(Project::default(), Config::default());
        s.select_workflow("source_document").unwrap();
        s.receive_message("Write both chapters, around 800 lines".into())
            .unwrap();
        s.document_written = true;
        s.last_document_write = Some((std::path::PathBuf::from("out.md"), "same-hash".into()));
        s.document_review.approved_hash = Some("same-hash".into());
        s.document_review.issues = vec!["Old length requirement".into()];
        s.document_review.validation_log = vec![json!({"prior":"review"})];
        s.completion_review.approved = true;
        s.investigations.push(Investigation {
            id: "old-scope".into(),
            title: "Removed chapter".into(),
            status: "in_progress".into(),
            memory_refs: Default::default(),
            sources: vec![],
            section: "# Removed".into(),
            document_hash: None,
            note: String::new(),
        });
        let original = s.original_request.clone();
        let files = s.last_document_write.clone();
        let sources = s.sources.clone();
        s.receive_message("Remove the second chapter; use around 300 lines".into())
            .unwrap();
        s.accept_amendment(TaskAmendment {
            goal: Some("Write the first chapter, around 300 lines".into()),
            completion: Some(vec!["First chapter only, around 300 lines".into()]),
            retire_investigation_ids: Some(vec!["old-scope".into()]),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.original_request, original);
        assert_eq!(
            s.latest_request,
            "Write the first chapter, around 300 lines"
        );
        assert_eq!(s.last_document_write, files);
        assert_eq!(s.sources.len(), sources.len() + 1);
        assert!(s.document_written && s.question.is_none());
        assert_eq!(s.request_review_criteria.completion, s.task.completion);
        assert_eq!(s.document_review.approved_hash, None);
        assert!(s.document_review.issues.is_empty());
        assert_eq!(s.document_review.validation_log.len(), 1);
        assert!(!s.completion_review.approved);
        assert_eq!(s.investigations[0].status, "superseded");
        assert!(s.investigations[0].is_settled());
        assert_eq!(s.task_amendments.len(), 1);
        assert_eq!(s.task.workflow, "source_document");
    }

    #[test]
    fn rejected_message_admission_is_atomic_and_completed_tasks_do_not_offer_resume() {
        let mut s = Session::new(Project::default(), Config::default());
        s.receive_message("Task".into()).unwrap();
        s.status = "complete".into();
        assert!(!s.can_resume());
        let before = s.history.bytes();
        s.config.memory_bytes = 1;
        assert!(s.receive_message("Follow-up".into()).is_err());
        assert_eq!(s.history.bytes(), before);
        assert!(s.question.is_none());
        s.status = "cancelled".into();
        assert!(s.can_resume());
    }

    #[test]
    fn cloned_histories_keep_mutations_private_and_counts_current() {
        let mut original = SessionHistory::default();
        original.push(
            vec![json!({"role":"assistant","content":"first message"})],
            true,
        );
        original.push(
            vec![json!({"role":"assistant","content":"second message"})],
            true,
        );
        let bytes = original.bytes();
        let mut copy = original.clone();
        copy.bundles[0].active = false;
        assert!(original.bundles[0].active);
        assert_eq!(copy.bytes(), bytes + 1);
        copy.bundles[0].messages[0]["content"] = json!("edited");
        assert_ne!(original.bundles[0].messages[0], copy.bundles[0].messages[0]);
        assert_eq!(original.bytes(), bytes);
        assert_eq!(
            copy.bytes(),
            copy.bundles.iter().map(serialized_bytes).sum::<usize>()
        );
    }

    #[test]
    fn checkpoint_retirement_respects_exact_serialized_capacity() {
        for active in [false, true] {
            for reviewed in [false, true] {
                let mut history = SessionHistory::default();
                let id = history.push(vec![json!({"role":"user","content":"keep"})], true);
                history.bundles[0].active = active;
                history.bundles[0].reviewed = reviewed;
                let mut retired = history.clone();
                retired.bundles[0].active = false;
                retired.bundles[0].reviewed = true;
                let limit = retired.bytes();
                let mut below_capacity = history.clone();

                history
                    .prune_retiring(limit, &BTreeSet::from([id]))
                    .unwrap();
                assert_eq!(history.bundles.len(), 1);
                assert_eq!(history.bytes(), limit);
                assert!(!history.bundles[0].active && history.bundles[0].reviewed);

                below_capacity
                    .prune_retiring(limit - 1, &BTreeSet::from([id]))
                    .unwrap();
                assert!(below_capacity.bundles.is_empty());
                assert_eq!(below_capacity.pruned_through, Some(id));
            }
        }
    }
}
