//! Explicit paid evaluation; normal cargo test never calls a provider.
use mnemoarc::{
    agent::{self, AgentEvent},
    config::{Config, Secret},
    llm::OpenAiClient,
    session::Session,
    tools,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

fn env_bool(name: &str, fallback: bool) -> bool {
    match std::env::var(name).ok().as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        Some(value) => panic!("{name} must be a boolean, got {value:?}"),
        None => fallback,
    }
}

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
    config.run_timeout_secs = std::env::var("MNEMOARC_LIVE_TIMEOUT_SECS")
        .map(|v| {
            v.parse()
                .expect("MNEMOARC_LIVE_TIMEOUT_SECS must be an integer")
        })
        .unwrap_or_else(|_| config.run_timeout_secs.min(900));
    if let Ok(tokens) = std::env::var("MNEMOARC_LIVE_TOKENS") {
        config.run_tokens = tokens
            .parse()
            .expect("MNEMOARC_LIVE_TOKENS must be an integer");
    }
    config.source_document_review = env_bool("MNEMOARC_LIVE_SOURCE_DOCUMENT_REVIEW", true);
    config.completion_review_enabled = env_bool("MNEMOARC_LIVE_COMPLETION_REVIEW", true);
    eprintln!(
        "[live] budget_tokens={} budget_seconds={} source_document_review={} completion_review={}",
        config.run_tokens,
        config.run_timeout_secs,
        config.source_document_review,
        config.completion_review_enabled
    );
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
    // MNEMOARC_LIVE_PROMPT_FILE swaps in another request against the same project.
    let request = match std::env::var("MNEMOARC_LIVE_PROMPT_FILE") {
        Ok(path) => std::fs::read_to_string(&path).expect("MNEMOARC_LIVE_PROMPT_FILE"),
        Err(_) => include_str!("fixtures/llm-agent-document-request.txt").into(),
    };
    session.add_user(request.trim().into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut first_write = None;
        let mut reviews = Vec::new();
        let mut last_round = 0;
        let mut seen_call_ids = BTreeSet::new();
        let mut seen_result_ids = BTreeSet::new();
        let mut call_signatures = BTreeMap::<String, (String, String)>::new();
        let mut signature_counts = BTreeMap::<(String, String), usize>::new();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Tool { name, status, .. } => {
                    eprintln!("[live] tool={name} status={status}");
                }
                AgentEvent::Snapshot(s) => {
                    for message in s.history.bundles.iter().flat_map(|bundle| &bundle.messages) {
                        if let Some(calls) = message["tool_calls"].as_array() {
                            for call in calls {
                                let Some(id) = call["id"].as_str() else {
                                    continue;
                                };
                                if !seen_call_ids.insert(id.to_owned()) {
                                    continue;
                                }
                                let name = call["function"]["name"]
                                    .as_str()
                                    .unwrap_or("unknown")
                                    .to_owned();
                                let arguments = call["function"]["arguments"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_owned();
                                let signature = (name.clone(), arguments);
                                let count = signature_counts.entry(signature.clone()).or_default();
                                *count += 1;
                                call_signatures.insert(id.to_owned(), signature.clone());
                                if *count == 2 {
                                    eprintln!("[live] repeated_exact_call name={} count=2", name);
                                }
                            }
                        }
                        // A plan result can be status=ok yet change nothing;
                        // log it so bookkeeping loops are visible.
                        if message["role"] == "tool"
                            && let Some(id) = message["tool_call_id"].as_str()
                            && !seen_result_ids.contains(id)
                            && let Some((name, arguments)) = call_signatures.get(id)
                            && name == "task_plan"
                            && let Some(result) = message["content"]
                                .as_str()
                                .and_then(|content| serde_json::from_str::<Value>(content).ok())
                            && result["status"] == "ok"
                        {
                            let data = &result["data"];
                            eprintln!(
                                "[live] task_plan applied={} unchanged={} reason={} pending={} args={}",
                                data["applied"],
                                data["unchanged"],
                                data["reason"].as_str().unwrap_or(""),
                                data["plan"]["pending_count"],
                                arguments.chars().take(200).collect::<String>()
                            );
                        }
                        if message["role"] == "tool"
                            && let Some(id) = message["tool_call_id"].as_str()
                            && seen_result_ids.insert(id.to_owned())
                            && let Some((name, arguments)) = call_signatures.get(id)
                            && let Some(result) = message["content"]
                                .as_str()
                                .and_then(|content| serde_json::from_str::<Value>(content).ok())
                            && result["status"] != "ok"
                        {
                            let code = result["recovery"]["code"]
                                .as_str()
                                .or_else(|| result["code"].as_str())
                                .unwrap_or("tool_error");
                            let repeat_count = call_signatures
                                .get(id)
                                .and_then(|signature| signature_counts.get(signature))
                                .copied()
                                .unwrap_or(1);
                            // Arguments and the error text make a failure
                            // diagnosable while the run is still going.
                            let clip = |text: &str, max: usize| -> String {
                                text.chars()
                                    .map(|c| if c == '\n' { ' ' } else { c })
                                    .take(max)
                                    .collect()
                            };
                            eprintln!(
                                "[live] tool_failure name={} code={} same_call_count={} args={} error={}",
                                name,
                                code,
                                repeat_count,
                                clip(arguments, 200),
                                clip(result["error"].as_str().unwrap_or(""), 300)
                            );
                            // A partial batch hides each item's cause one level down.
                            for item in result["data"]["results"].as_array().into_iter().flatten() {
                                let nested = &item["result"];
                                if nested["status"] != "ok" {
                                    eprintln!(
                                        "[live] tool_failure_item name={} id={} code={} error={}",
                                        name,
                                        item["id"].as_str().unwrap_or("?"),
                                        nested["recovery"]["code"].as_str().unwrap_or("tool_error"),
                                        clip(nested["error"].as_str().unwrap_or(""), 300)
                                    );
                                }
                            }
                        }
                    }
                    if s.document_written && first_write.is_none() {
                        first_write = Some(
                            json!({"round":s.task_rounds,"input_tokens":s.input_tokens,"output_tokens":s.output_tokens}),
                        );
                    }
                    if s.document_review.attempts > reviews.len() {
                        reviews.push(json!(s.document_review));
                    }
                    if s.task_rounds > last_round {
                        last_round = s.task_rounds;
                        let verified = s
                            .investigations
                            .iter()
                            .filter(|item| item.status == "verified")
                            .count();
                        eprintln!(
                            "[live] round={} input={} output={} document_written={} investigations={}/{} document_reviews={} completion_reviews={} checkpoint={} ladder={}/{} best={} closing={} unrepaired_finals={}",
                            s.task_rounds,
                            s.input_tokens,
                            s.output_tokens,
                            s.document_written,
                            verified,
                            s.investigations.len(),
                            s.document_review.attempts,
                            s.completion_review.attempts,
                            s.checkpoint
                                .as_ref()
                                .map_or(0, |checkpoint| checkpoint.attempts),
                            // Progress ladder state: rounds without a better
                            // score / closing threshold, and why closing began.
                            s.progress_recovery.rounds_since_best,
                            s.config.stall_round_limit * 3,
                            s.progress_recovery.best_score,
                            s.progress_recovery.closing.as_ref().map_or(
                                "none".to_owned(),
                                |closing| format!("{}:{}", closing.reason, closing.rounds)
                            ),
                            s.progress_recovery.unrepaired_finals
                        );
                    }
                }
                _ => {}
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
        "source_document_review_enabled":result.config.source_document_review,
        "completion_review_enabled":result.config.completion_review_enabled,
        "completion_review":result.completion_review,
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
    if result.config.source_document_review {
        assert!(tools::document_review::approved(&result));
    } else {
        assert_eq!(result.document_review.attempts, 0);
    }
    if result.config.completion_review_enabled {
        assert!(result.completion_review.attempts > 0);
        assert!(result.completion_review.approved);
    }
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
