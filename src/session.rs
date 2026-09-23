use crate::{
    config::{Config, Project},
    memory::{MemoryStore, Source, id},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub project: Project,
    pub config: Config,
    pub pending_config: Option<Config>,
    pub task: TaskState,
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
    pub run_guidance: Value,
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
impl Session {
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
            run_guidance: json!({}),
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
            }
            if self.task.completion.is_empty() {
                self.task.completion = self.initial_completion(&text);
            }
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
