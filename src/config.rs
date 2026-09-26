use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Secret(pub String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip)]
    pub api_key: Option<Secret>,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub proxy: Option<String>,
    pub disable_proxy: bool,
    pub model_context: Option<usize>,
    pub context_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_effort: Option<String>,
    /// Controls Qwen-style thinking/reasoning. When false, the client sends
    /// the provider's standard no-reasoning value even if reasoning_effort is
    /// still populated for a later re-enable.
    pub enable_thinking: bool,
    pub legacy_max_tokens: bool,
    pub stream_usage: bool,
    pub memory_count: usize,
    pub recent_count: usize,
    pub related_count: usize,
    pub memory_reuse: bool,
    pub memory_body_bytes: usize,
    pub memory_bytes: usize,
    pub history_bytes: usize,
    pub state_tokens: usize,
    pub index_tokens: usize,
    pub result_tokens: usize,
    pub batch_tokens: usize,
    pub checkpoint_tokens: usize,
    pub high_water: f64,
    pub low_water: f64,
    pub read_parallelism: usize,
    pub request_timeout_secs: u64,
    pub tool_timeout_secs: u64,
    pub retries: usize,
    pub run_timeout_secs: u64,
    pub run_tokens: usize,
    /// Stalled document reviews before focused recovery (not a stop quota).
    pub review_limit: usize,
    /// Edit requests per automatic document re-review interval.
    pub document_repair_limit: usize,
    pub source_answer_review: bool,
    pub source_document_review: bool,
    /// Run the general read-only acceptance review before completing artifacts.
    pub completion_review_enabled: bool,
    pub writing_reserve_ratio: f64,
    pub verification_reserve_ratio: f64,
    /// Remaining-budget fraction at which document work enters closing mode.
    pub closing_reserve_ratio: f64,
    pub repeated_read_limit: usize,
    pub stall_round_limit: usize,
    pub database: crate::database::DatabaseConfig,
    pub projects: Vec<Project>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub root: PathBuf,
    pub output: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub purpose: String,
    pub audience: String,
}
impl Default for Project {
    fn default() -> Self {
        Self { name: "project".into(), root: PathBuf::from("."), output: "docs/source-summary.md".into(), include: vec![], exclude: vec![], purpose: "Explain the architecture, main flows, data structures and error handling with source evidence".into(), audience: "Developers".into() }
    }
}
impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key_env: "OPENAI_API_KEY".into(),
            proxy: None,
            disable_proxy: false,
            model_context: None,
            context_tokens: 64000,
            output_tokens: 8000,
            reasoning_effort: None,
            enable_thinking: true,
            legacy_max_tokens: false,
            stream_usage: true,
            memory_count: 1000,
            recent_count: 20,
            related_count: 5,
            memory_reuse: true,
            memory_body_bytes: 8192,
            memory_bytes: 16 * 1024 * 1024,
            history_bytes: 64 * 1024 * 1024,
            state_tokens: 2000,
            index_tokens: 4000,
            result_tokens: 4000,
            batch_tokens: 8000,
            checkpoint_tokens: 4000,
            high_water: 0.8,
            low_water: 0.6,
            read_parallelism: 4,
            request_timeout_secs: 180,
            tool_timeout_secs: 30,
            retries: 2,
            run_timeout_secs: 1800,
            run_tokens: 500000,
            writing_reserve_ratio: 0.5,
            verification_reserve_ratio: 0.25,
            closing_reserve_ratio: 0.1,
            repeated_read_limit: 2,
            stall_round_limit: 8,
            database: crate::database::DatabaseConfig::default(),
            review_limit: 3,
            document_repair_limit: 8,
            source_answer_review: true,
            source_document_review: true,
            completion_review_enabled: true,
            projects: vec![],
        }
    }
}
impl Config {
    pub fn completion_url(&self) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base_url)?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("base_url must use http or https");
        }
        if url.fragment().is_some() {
            bail!("base_url must not contain a fragment");
        }
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("base_url must be a hierarchical URL"))?;
            path.pop_if_empty();
            path.push("chat");
            path.push("completions");
        }
        Ok(url)
    }

    pub fn validate(&self) -> Result<()> {
        if !(0.0 < self.low_water && self.low_water < self.high_water && self.high_water < 1.0) {
            bail!("Require 0 < low_water < high_water < 1");
        }
        if self.recent_count > self.memory_count
            || (self.memory_reuse
                && self.recent_count.saturating_mul(160).saturating_add(128) > self.index_tokens)
        {
            bail!(
                "recent_count does not fit memory_count/index_tokens (baseline 160 tokens per entry + 128 overhead; oversized entries are omitted from state)"
            );
        }
        if self
            .output_tokens
            .saturating_add(self.batch_tokens)
            .saturating_add(self.checkpoint_tokens)
            .saturating_add(1024)
            >= self.context_tokens
        {
            bail!("Context must leave space for input, output, tools and checkpoint");
        }
        if self.checkpoint_tokens == 0 || crate::context::ContextManager::input_budget(self) < 4096
        {
            bail!(
                "Context must reserve 4096 input tokens plus three cleanup rounds; reduce output/batch/checkpoint limits or increase context"
            );
        }
        if self.result_tokens < 200 || self.batch_tokens < 200 || self.checkpoint_tokens < 200 {
            bail!("Result, batch and checkpoint limits must be at least 200 tokens");
        }
        if let Some(n) = self.model_context
            && self.context_tokens > n
        {
            bail!("Context exceeds model_context");
        }
        if self.memory_body_bytes > self.memory_bytes || self.result_tokens > self.batch_tokens {
            bail!("Individual limits exceed total limits");
        }
        if [
            self.memory_count,
            self.memory_bytes,
            self.memory_body_bytes,
            self.history_bytes,
            self.state_tokens,
            self.result_tokens,
            self.read_parallelism,
            self.review_limit,
            self.document_repair_limit,
            self.output_tokens,
            self.run_tokens,
        ]
        .contains(&0)
            || self.request_timeout_secs == 0
            || self.tool_timeout_secs == 0
            || self.run_timeout_secs == 0
        {
            bail!("Limits must be positive");
        }
        if !self.writing_reserve_ratio.is_finite()
            || !self.verification_reserve_ratio.is_finite()
            || !(0.0..1.0).contains(&self.verification_reserve_ratio)
            || self.verification_reserve_ratio == 0.0
            || !(self.verification_reserve_ratio..1.0).contains(&self.writing_reserve_ratio)
            || self.writing_reserve_ratio == self.verification_reserve_ratio
            || !self.closing_reserve_ratio.is_finite()
            || self.closing_reserve_ratio <= 0.0
            || self.closing_reserve_ratio >= self.verification_reserve_ratio
            || self.repeated_read_limit == 0
            || self.stall_round_limit == 0
        {
            bail!(
                "Require 0 < closing reserve < verification reserve < writing reserve < 1 and positive repetition limits"
            );
        }
        for seconds in [
            self.request_timeout_secs,
            self.tool_timeout_secs,
            self.run_timeout_secs,
        ] {
            if std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(seconds))
                .is_none()
            {
                bail!("Timeout exceeds supported clock range");
            }
        }
        self.completion_url()?;
        if !self.disable_proxy
            && let Some(proxy) = &self.proxy
        {
            reqwest::Proxy::all(proxy)?;
        }
        self.database.validate()?;
        Ok(())
    }
    pub fn runnable(&self) -> Result<()> {
        self.validate()?;
        if self.model.is_empty() || self.model_context.is_none() {
            bail!("Configure model and model_context before running");
        }
        Ok(())
    }
    pub fn load(path: &Path, overrides: &BTreeMap<String, serde_json::Value>) -> Result<Self> {
        let mut value = serde_json::to_value(Self::default())?;
        let mut env: BTreeMap<String, String> = dotenvy::from_path_iter(".env")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect();
        env.extend(std::env::vars().filter(|(k, _)| k.starts_with("MNEMOARC_")));
        for (key, raw) in env {
            if let Some(field) = key.strip_prefix("MNEMOARC_") {
                let field = field.to_lowercase();
                if value.get(&field).is_some() {
                    let parsed =
                        serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                    value[&field] = parsed;
                }
            }
        }
        if path.exists() {
            let saved: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
            let saved = serde_json::to_value(saved)?;
            for (k, v) in saved
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Expected configuration table"))?
            {
                value[k] = v.clone();
            }
        }
        for (k, v) in overrides {
            value[k] = v.clone();
        }
        let mut config: Self = serde_json::from_value(value)?;
        let credentials = path.with_extension("credentials.json");
        if credentials.exists() {
            let keys: BTreeMap<String, String> =
                serde_json::from_slice(&std::fs::read(credentials)?)?;
            config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
        }
        config.validate()?;
        Ok(config)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        use std::io::Write;
        temp.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    }
}
