//! Explicit paid evaluation; normal cargo test never calls a provider.
use mnemoarc::{
    agent::{self, AgentEvent},
    config::{Config, Secret},
    llm::OpenAiClient,
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "paid provider; explicitly set MNEMOARC_LIVE_TEST=1"]
async fn registered_source_documentation() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    if path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(path.with_extension("credentials.json")).unwrap(),
        )
        .unwrap();
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    config.run_timeout_secs = config.run_timeout_secs.min(900);
    config.source_document_review = true;
    let mut project = config
        .projects
        .iter()
        .find(|p| p.name == "llm_agent")
        .expect("registered llm_agent project")
        .clone();
    let original_output = project.output.clone();
    let original_hash = std::fs::read(&original_output)
        .ok()
        .map(|b| tools::hash(&b));
    let before = tools::project_fingerprint(&project).unwrap();
    let dir = tempfile::tempdir().unwrap();
    project.output = dir.path().join("generated.md");
    let mut session = Session::new(project, config);
    session.add_user(
        include_str!("fixtures/llm-agent-document-request.txt")
            .trim()
            .into(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut first_write = None;
        let mut reviews = Vec::new();
        while let Some(event) = rx.recv().await {
            if let AgentEvent::Snapshot(s) = event {
                if s.document_written && first_write.is_none() {
                    first_write = Some(
                        json!({"round":s.task_rounds,"input_tokens":s.input_tokens,"output_tokens":s.output_tokens}),
                    );
                }
                if s.document_review.attempts > reviews.len() {
                    reviews.push(json!(s.document_review));
                }
            }
        }
        (first_write, reviews)
    });
    let start = Instant::now();
    let mut result = agent::run_session(
        session,
        Arc::new(OpenAiClient),
        CancellationToken::new(),
        tx,
    )
    .await;
    let (first_write, reviews) = drain.await.unwrap();
    let source_unchanged = tools::project_fingerprint(&result.project).unwrap() == before;
    let original_unchanged = std::fs::read(&original_output)
        .ok()
        .map(|b| tools::hash(&b))
        == original_hash;
    let audit =
        tools::audit_document(&mut result).unwrap_or_else(|e| json!({"error":e.to_string()}));
    let document = std::fs::read_to_string(&result.project.output).unwrap_or_default();
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for message in result.history.bundles.iter().flat_map(|b| &b.messages) {
        calls.extend(
            message["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
        if message["role"] == "tool"
            && let Some(value) = message["content"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
            && value["status"] != "ok"
        {
            errors.push(value);
        }
    }
    let report = json!({"status":result.status,"error":result.last_error,"model":result.config.model,
        "budget_tokens":result.config.run_tokens,"budget_seconds":result.config.run_timeout_secs,
        "elapsed_seconds":start.elapsed().as_secs_f64(),"input_tokens":result.input_tokens,"output_tokens":result.output_tokens,
        "usage_incomplete":result.usage_incomplete,"model_rounds":result.task_rounds,"first_write":first_write,"review_attempts":reviews,
        "tool_calls":calls,"tool_errors":errors,"document_review":result.document_review,"audit":audit,
        "investigations":result.investigations,"source_unchanged":source_unchanged,"configured_output_unchanged":original_unchanged,
        "document_lines":document.lines().count(),"document":document});
    if let Ok(path) = std::env::var("MNEMOARC_DOC_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    eprintln!(
        "documentation: status={} rounds={} tools={} input={} output={} seconds={:.1}",
        result.status,
        result.task_rounds,
        calls.len(),
        result.input_tokens,
        result.output_tokens,
        start.elapsed().as_secs_f64()
    );
    assert!(
        source_unchanged && original_unchanged,
        "source or original output changed"
    );
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(audit["structural_ok"], true);
    assert!(tools::document_review::approved(&result));
    assert!(
        result.investigations.len() >= 4
            && result.investigations.iter().all(|i| i.status == "verified")
    );
    if let Ok(limit) = std::env::var("MNEMOARC_DOC_MAX_INPUT") {
        assert!(
            result.input_tokens <= limit.parse::<usize>().unwrap(),
            "input token ceiling exceeded"
        );
    }
    // Structural checks and a model review are not proof. Inspect the saved
    // document against current source before claiming semantic correctness.
}
