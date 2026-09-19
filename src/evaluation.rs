//! Evaluation saves generated documents and aggregate measurements, never session transcripts.
use crate::{
    agent,
    config::{Config, Project},
    session::Session,
    tools,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    #[serde(default = "three")]
    pub repetitions: usize,
    pub cases: Vec<Case>,
}
fn three() -> usize {
    3
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub root: PathBuf,
    pub prompt: String,
    #[serde(default)]
    pub expected_topics: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}
#[derive(Serialize)]
pub struct RunReport {
    pub case: String,
    pub variant: String,
    pub repetition: usize,
    pub model: String,
    pub source_sha256: String,
    pub source_unchanged: bool,
    pub status: String,
    pub output: PathBuf,
    pub elapsed_secs: f64,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_tokens: Option<usize>,
    pub usage_estimated_or_incomplete: bool,
    pub checkpoints: usize,
    pub memory_loads: usize,
    pub history_loads: usize,
    pub investigation_count: usize,
    pub verified_count: usize,
    pub topics_found: Vec<String>,
    pub topics_missing: Vec<String>,
    pub factual_errors: Option<usize>,
    pub evidence_accuracy: Option<f64>,
    pub reviewer_notes: String,
}
pub async fn run(config: Config, suite_path: &Path, output: &Path) -> Result<()> {
    config.runnable()?;
    let suite: Suite = toml::from_str(&std::fs::read_to_string(suite_path)?)?;
    if suite.repetitions < 3 {
        bail!("Evaluation requires at least three repetitions");
    }
    std::fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    let parent = suite_path.canonicalize()?.parent().unwrap().to_path_buf();
    for case in suite.cases {
        if case.name.is_empty()
            || !case
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("Case names must be ASCII letters, digits, hyphens or underscores");
        }
        let root = parent.join(case.root).canonicalize()?;
        let mut project = Project {
            name: case.name.clone(),
            root: root.clone(),
            purpose: case.prompt.clone(),
            exclude: case.exclude.clone(),
            ..Default::default()
        };
        if let Ok(relative) = output.strip_prefix(&root) {
            project.exclude.push(format!("{}/**", relative.display()));
        }
        let fingerprint = tools::project_fingerprint(&project)?;
        for variant in ["full", "no-memory-reuse"] {
            for repetition in 1..=suite.repetitions {
                if tools::project_fingerprint(&project)? != fingerprint {
                    bail!("Source changed during evaluation; restore it before continuing");
                }
                let stem = format!("{}-{variant}-{repetition}", case.name);
                let doc = output.join(format!("{stem}.md"));
                let metrics = output.join(format!("{stem}.json"));
                if doc.exists() || metrics.exists() {
                    bail!("Evaluation output already exists: {stem}; use a new output directory");
                }
                project.output = doc.clone();
                let mut config = config.clone();
                config.memory_reuse = variant == "full";
                let started = std::time::Instant::now();
                let session =
                    agent::headless(Session::new(project.clone(), config), case.prompt.clone())
                        .await?;
                let text = std::fs::read_to_string(&doc)
                    .unwrap_or_default()
                    .to_lowercase();
                let (topics_found, topics_missing) = case
                    .expected_topics
                    .iter()
                    .cloned()
                    .partition(|topic| text.contains(&topic.to_lowercase()));
                let report=RunReport{case:case.name.clone(),variant:variant.into(),repetition,model:session.config.model.clone(),source_sha256:fingerprint.clone(),source_unchanged:tools::project_fingerprint(&project)?==fingerprint,status:session.status,output:doc,elapsed_secs:started.elapsed().as_secs_f64(),input_tokens:session.input_tokens,output_tokens:session.output_tokens,cached_tokens:session.cached_tokens,usage_estimated_or_incomplete:session.usage_incomplete,checkpoints:session.checkpoints_completed,memory_loads:session.memory_loads,history_loads:session.history_loads,investigation_count:session.investigations.len(),verified_count:session.investigations.iter().filter(|i|i.status=="verified").count(),topics_found,topics_missing,factual_errors:None,evidence_accuracy:None,reviewer_notes:"Keyword coverage is only a screening metric. Independently review factual errors and source citations; verified_count is the agent's own recorded verification, not a ground-truth score.".into()};
                std::fs::write(&metrics, serde_json::to_string_pretty(&report)?)?;
                if report.status == "cancelled" {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}
