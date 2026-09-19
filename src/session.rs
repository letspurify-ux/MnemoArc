use crate::{
    config::{Config, Project},
    memory::{MemoryStore, Source, id},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskState {
    pub purpose: String,
    pub scope: String,
    pub deliverables: Vec<String>,
    pub constraints: Vec<String>,
    pub completion: Vec<String>,
    pub done: Vec<String>,
    pub findings: Vec<String>,
    pub current: String,
    pub next: String,
    pub unresolved: Vec<String>,
    pub memory_ids: Vec<String>,
    pub details: Vec<Value>,
    pub revision: u64,
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
    pub starting_state_revision: u64,
    pub starting_memory_generation: u64,
    pub failed: bool,
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
    pub last_error: Option<String>,
    pub run_guidance: Value,
}
impl Session {
    pub fn new(project: Project, config: Config) -> Self {
        let task = TaskState {
            purpose: project.purpose.clone(),
            scope: project.root.display().to_string(),
            deliverables: vec![project.output.display().to_string()],
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
            active_tools: BTreeSet::new(),
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
            last_error: None,
            run_guidance: json!({}),
        }
    }
    pub fn protected(&self) -> BTreeSet<String> {
        self.task
            .memory_ids
            .iter()
            .cloned()
            .chain(
                self.investigations
                    .iter()
                    .flat_map(|i| i.memory_refs.keys().cloned()),
            )
            .collect()
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
                    .ok_or_else(|| anyhow::anyhow!("unknown_source: {id}"))
            })
            .collect()
    }
    pub fn add_user(&mut self, text: String) {
        self.latest_request = text.clone();
        let source = Source {
            id: id(),
            observed_at: chrono::Utc::now(),
            origin: "user".into(),
            path: None,
            start_line: None,
            end_line: None,
            hash: None,
            excerpt: text.chars().take(2000).collect(),
        };
        self.sources.insert(source.id.clone(), source);
        self.history
            .push(vec![json!({"role":"user","content":text})], true);
    }
    pub fn check_limits(&self, c: &Config) -> Result<()> {
        c.validate()?;
        if self.memory.entries.len() > c.memory_count
            || self.memory.bytes() > c.memory_bytes
            || self.history.bytes() > c.history_bytes
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
