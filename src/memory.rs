use crate::config::Config;
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

// Process-wide allocation also keeps parallel read workers collision-free.
pub fn source_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "S{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
pub fn id() -> String {
    Uuid::new_v4().to_string()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    NeedsReview,
    Superseded,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Decision,
    Failure,
    Question,
    Procedure,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub observed_at: DateTime<Utc>,
    pub origin: String,
    pub path: Option<String>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub hash: Option<String>,
    pub excerpt: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub key: Option<String>,
    pub title: String,
    pub summary: String,
    pub body: String,
    pub tags: Vec<String>,
    pub kind: MemoryKind,
    pub status: MemoryStatus,
    pub inferred: bool,
    pub sources: Vec<Source>,
    pub metadata: serde_json::Value,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryInput {
    pub key: Option<String>,
    pub title: String,
    pub summary: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub kind: MemoryKind,
    #[serde(default)]
    pub inferred: bool,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub expected_revision: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryMeta {
    pub id: String,
    pub key: Option<String>,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub status: MemoryStatus,
    pub revision: u64,
}
impl Memory {
    pub fn meta(&self) -> MemoryMeta {
        MemoryMeta {
            id: self.id.clone(),
            key: self.key.clone(),
            title: self.title.clone(),
            summary: self.summary.clone(),
            tags: self.tags.clone(),
            status: self.status.clone(),
            revision: self.revision,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    pub entries: BTreeMap<String, Memory>,
    pub generation: u64,
}
impl MemoryStore {
    pub fn bytes(&self) -> usize {
        self.entries
            .values()
            .map(|m| serde_json::to_vec(m).map_or(usize::MAX, |b| b.len()))
            .sum()
    }
    pub fn get(&self, id_or_key: &str) -> Result<&Memory> {
        self.entries
            .get(id_or_key)
            .or_else(|| {
                self.entries
                    .values()
                    .find(|m| m.key.as_deref() == Some(id_or_key))
            })
            .ok_or_else(|| anyhow::anyhow!("memory_not_found: {id_or_key}"))
    }
    pub fn save(
        &mut self,
        input: MemoryInput,
        sources: Vec<Source>,
        config: &Config,
    ) -> Result<MemoryMeta> {
        if input.title.trim().is_empty()
            || input.summary.trim().is_empty()
            || input.body.trim().is_empty()
        {
            bail!("Memory title, summary and body are required");
        }
        if input.body.len() > config.memory_body_bytes {
            bail!("memory_body_limit");
        }
        if input.key.as_ref().is_some_and(|k| k.trim().is_empty()) {
            bail!("Empty memory key");
        }
        let old = input.key.as_deref().and_then(|k| self.get(k).ok()).cloned();
        if let Some(m) = &old {
            if input.kind == MemoryKind::Fact
                && !input.inferred
                && !m.sources.is_empty()
                && sources.is_empty()
            {
                bail!(
                    "memory_sources_required: an observed fact cannot discard its existing sources; supply valid source_ids or explicitly mark the new claim inferred for review"
                );
            }
            if input.expected_revision != Some(m.revision) {
                bail!("revision_conflict: expected {}", m.revision);
            }
        } else if input.expected_revision.is_some() {
            bail!("revision_conflict: memory does not exist");
        }
        if let Some(m) = self.entries.values().find(|m| {
            m.status != MemoryStatus::Superseded
                && m.body == input.body
                && m.sources == sources
                && m.key == input.key
                && m.title == input.title
                && m.summary == input.summary
                && m.tags == input.tags
                && m.kind == input.kind
                && m.inferred == input.inferred
                && m.metadata == input.metadata
        }) {
            return Ok(m.meta());
        }
        let status = if input.inferred || (input.kind == MemoryKind::Fact && sources.is_empty()) {
            MemoryStatus::NeedsReview
        } else {
            MemoryStatus::Active
        };
        let now = Utc::now();
        let m = Memory {
            id: old.as_ref().map(|m| m.id.clone()).unwrap_or_else(id),
            key: input.key,
            title: input.title,
            summary: input.summary,
            body: input.body,
            tags: input.tags,
            kind: input.kind,
            status,
            inferred: input.inferred,
            sources,
            metadata: input.metadata,
            revision: old.as_ref().map_or(1, |m| m.revision + 1),
            created_at: old.as_ref().map_or(now, |m| m.created_at),
            updated_at: now,
        };
        // Enforce the same per-entry token reservation used by configuration validation.
        let metadata_tokens =
            crate::context::count(&serde_json::to_value(m.meta())?, &config.model);
        if metadata_tokens > 160 {
            bail!(
                "memory_metadata_limit: metadata uses {metadata_tokens}/160 estimated tokens including ID/key/JSON; shorten key/title/summary/tags, keep details in body"
            );
        }
        let next_bytes = self.bytes().saturating_sub(
            old.as_ref()
                .map_or(0, |m| serde_json::to_vec(m).unwrap().len()),
        ) + serde_json::to_vec(&m)?.len();
        if self.entries.len() + usize::from(old.is_none()) > config.memory_count
            || next_bytes > config.memory_bytes
        {
            bail!("memory_capacity: explicitly delete/replace candidates; no memory was removed");
        }
        let meta = m.meta();
        self.entries.insert(m.id.clone(), m);
        self.generation += 1;
        Ok(meta)
    }
    pub fn delete(&mut self, ident: &str, protected: &BTreeSet<String>) -> Result<()> {
        let key = self.get(ident)?.id.clone();
        if protected.contains(&key) {
            bail!("memory_referenced: detach or replace first");
        }
        self.entries.remove(&key);
        self.generation += 1;
        Ok(())
    }
    pub fn replace(
        &mut self,
        ids: &[String],
        input: MemoryInput,
        sources: Vec<Source>,
        config: &Config,
    ) -> Result<MemoryMeta> {
        let mut candidate = self.clone();
        for id in ids {
            let key = candidate.get(id)?.id.clone();
            candidate.entries.remove(&key);
        }
        let result = candidate.save(input, sources, config)?;
        candidate.generation = self.generation + 1;
        *self = candidate;
        Ok(result)
    }
    pub fn recent(&self, n: usize) -> Vec<MemoryMeta> {
        let mut rows: Vec<_> = self.entries.values().collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        rows.into_iter().take(n).map(Memory::meta).collect()
    }
    pub fn search(&self, query: &str, tags: &[String]) -> Vec<MemoryMeta> {
        let q = query.to_lowercase();
        let words: Vec<_> = q.split_whitespace().collect();
        let mut matches: Vec<_> = self
            .entries
            .values()
            .filter(|m| tags.iter().all(|t| m.tags.contains(t)))
            .filter_map(|m| {
                let title =
                    format!("{} {} {}", m.title, m.summary, m.tags.join(" ")).to_lowercase();
                let body = m.body.to_lowercase();
                let score = if q.is_empty() {
                    1
                } else if m.id == query || m.key.as_deref() == Some(query) {
                    10000
                } else {
                    words
                        .iter()
                        .map(|w| {
                            usize::from(title.contains(w)) * 10 + usize::from(body.contains(w))
                        })
                        .sum()
                };
                (score > 0).then_some((score, m))
            })
            .collect();
        matches.sort_by(|(sa, a), (sb, b)| {
            sb.cmp(sa)
                .then(b.updated_at.cmp(&a.updated_at))
                .then(a.id.cmp(&b.id))
        });
        matches.into_iter().map(|(_, m)| m.meta()).collect()
    }
    pub fn page(
        &self,
        query: &str,
        tags: &[String],
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<serde_json::Value> {
        let offset = if let Some(c) = cursor {
            let (generation, offset) = c
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("invalid_cursor"))?;
            if generation.parse::<u64>()? != self.generation {
                bail!("cursor_expired");
            }
            offset.parse::<usize>()?
        } else {
            0
        };
        let rows = self.search(query, tags);
        if offset > rows.len() {
            bail!("invalid_cursor");
        }
        let end = offset.saturating_add(limit.clamp(1, 100)).min(rows.len());
        Ok(
            serde_json::json!({"items":rows[offset..end],"next_cursor":(end<rows.len()).then(||format!("{}:{}",self.generation,end))}),
        )
    }
    pub fn stale_path(&mut self, path: &str, hash: Option<&str>) {
        let mut changed = false;
        for m in self.entries.values_mut() {
            if m.sources
                .iter()
                .any(|s| s.path.as_deref() == Some(path) && s.hash.as_deref() != hash)
                && m.status == MemoryStatus::Active
            {
                m.status = MemoryStatus::NeedsReview;
                m.revision += 1;
                changed = true;
            }
        }
        if changed {
            self.generation += 1;
        }
    }
    pub fn candidates(&self, protected: &BTreeSet<String>) -> Vec<serde_json::Value> {
        self.entries.values().filter(|m|!protected.contains(&m.id)).map(|m|serde_json::json!({"id":m.id,"reason":if m.status==MemoryStatus::Superseded{"superseded"}else{"not_referenced"}})).take(20).collect()
    }
}
