//! Explicit paid evaluation; normal cargo test never calls a provider.
mod support;
use mnemoarc::{
    agent::{self, AgentEvent},
    config::{Config, Project, Secret},
    llm::OpenAiClient,
    session::{RunRecord, Session},
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

/// Resolve the original result by call ID before reading its bounded preview.
/// Archives belong to the producing call, not to a later history-read call.
fn tool_result_for_report(session: &Session, message: &Value) -> Option<Value> {
    if message["role"] != "tool" {
        return None;
    }
    let id = message["tool_call_id"].as_str()?;
    session
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .find(|archive| {
            archive["role"] == "tool_archive"
                && archive["call_id"] == id
                && archive["result"].is_object()
        })
        .map(|archive| archive["result"].clone())
        .or_else(|| serde_json::from_str(message["content"].as_str()?).ok())
}

fn tool_errors_for_report(session: &Session) -> Vec<Value> {
    session
        .history
        .bundles
        .iter()
        .flat_map(|bundle| &bundle.messages)
        .filter_map(|message| tool_result_for_report(session, message))
        .filter(|result| result["status"] != "ok")
        .collect()
}

#[test]
fn live_report_and_log_resolve_archived_batch_failure_details() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        support::compact_config(),
    );
    let call = mnemoarc::llm::ToolCall {
        id: "large-edit-batch".into(),
        name: "document_edit_batch".into(),
        arguments: json!({"expected_hash":"h","edits":[]}).to_string(),
    };
    let original = tools::envelope(Ok(json!({"results":(0..6).map(|id| {
        json!({"id":format!("item-{id}"),"result":tools::envelope(Err(anyhow::anyhow!(
            "patch_target_must_match_once: item-{id} {}", "missing anchor text ".repeat(100)
        )))})
    }).collect::<Vec<_>>()})));
    let bounded = tools::limit_result(&mut session, &call, original.clone(), 256);
    assert_eq!(bounded["truncated"], true);
    assert!(!bounded["data"]["results"].is_array());
    let message = json!({"role":"tool","tool_call_id":call.id,"content":bounded.to_string()});
    session.history.push(vec![message.clone()], true);

    // Both the live logger and final report read the same complete result.
    let logged = tool_result_for_report(&session, &message).unwrap();
    assert_eq!(logged, original);
    assert_eq!(logged["data"]["results"].as_array().unwrap().len(), 6);
    assert_eq!(tool_errors_for_report(&session), vec![original]);
}

#[test]
fn live_report_does_not_substitute_another_calls_archive() {
    let mut session = Session::new(Project::default(), support::compact_config());
    session.history.push(
        vec![json!({"role":"tool_archive","call_id":"original","result":{
            "status":"error","error":"original failure"
        }})],
        true,
    );
    let result = json!({"status":"error","error":"history read failed","truncated":true});
    let message = json!({"role":"tool","tool_call_id":"history-read","content":result.to_string()});
    session.history.push(vec![message.clone()], true);
    assert_eq!(
        tool_result_for_report(&session, &message),
        Some(result.clone())
    );
    assert_eq!(tool_errors_for_report(&session), vec![result]);
}

fn report_diagnostics(result: &Session, audit: &Value) -> Value {
    let final_document_hash = audit["hash"].as_str().map(str::to_owned).or_else(|| {
        std::fs::read(&result.project.output)
            .ok()
            .map(|bytes| tools::hash(&bytes))
    });
    json!({
        "completion_gaps":result.completion_gaps,
        "run_stop_reason":result.run_history.back().map(|run| run.reason.as_str()),
        "last_run":result.run_history.back(),
        "final_document_hash":final_document_hash,
    })
}

#[test]
fn live_report_records_completion_gaps_and_stop_reason() {
    let mut result = Session::new(Project::default(), support::compact_config());
    result.status = "complete_with_gaps".into();
    result.completion_gaps = vec!["미완료 할 일 — 첫 장 검증".into()];
    let now = chrono::Utc::now();
    result.run_history.push_back(RunRecord {
        id: "run".into(),
        request: "화면 사용자 매뉴얼 만들어줘".into(),
        workflow: "source_document".into(),
        started_at: now,
        ended_at: now,
        elapsed_ms: 100,
        status: result.status.clone(),
        reason: "closing_round_limit".into(),
        error: None,
        input_tokens: 10,
        output_tokens: 5,
        usage_estimated: false,
        rounds: 1,
        last_stage: "model".into(),
        checkpoint_pending: false,
        token_limit: 100,
        timeout_secs: 60,
    });
    let diagnostics = report_diagnostics(&result, &json!({"hash":"current-hash"}));
    assert_eq!(
        diagnostics["completion_gaps"],
        json!(result.completion_gaps)
    );
    assert_eq!(diagnostics["run_stop_reason"], "closing_round_limit");
    assert_eq!(diagnostics["final_document_hash"], "current-hash");
}

#[tokio::test]
#[ignore = "paid provider; explicitly set MNEMOARC_LIVE_TEST=1"]
async fn registered_source_documentation() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    if let Ok(model) = std::env::var("MNEMOARC_LIVE_MODEL") {
        config.model = model;
    }
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
    eprintln!(
        "[live] budget_tokens={} budget_seconds={}",
        config.run_tokens, config.run_timeout_secs,
    );
    // MNEMOARC_LIVE_PROJECT picks another registered project (default llm_agent).
    let project_name = std::env::var("MNEMOARC_LIVE_PROJECT").unwrap_or("llm_agent".into());
    let mut project = config
        .projects
        .iter()
        .find(|p| p.name == project_name)
        .unwrap_or_else(|| panic!("registered {project_name} project"))
        .clone();
    let original_output = project.output.clone();
    let original_hash = std::fs::read(&original_output)
        .ok()
        .map(|b| tools::hash(&b));
    let before = tools::project_fingerprint(&project).unwrap();
    let dir = tempfile::tempdir().unwrap();
    project.output = dir.path().join("generated.md");
    let mut session = Session::new(project, config);
    // The workflow a user selects in the session window; MNEMOARC_LIVE_WORKFLOW
    // overrides the documentation default.
    let workflow = std::env::var("MNEMOARC_LIVE_WORKFLOW").unwrap_or("source_document".into());
    session.select_workflow(&workflow).unwrap();
    eprintln!("[live] workflow_mode={workflow}");
    // MNEMOARC_LIVE_PROMPT_FILE swaps in another request against the same project.
    let request = match std::env::var("MNEMOARC_LIVE_PROMPT_FILE") {
        Ok(path) => std::fs::read_to_string(&path).expect("MNEMOARC_LIVE_PROMPT_FILE"),
        Err(_) => include_str!("fixtures/llm-agent-document-request.txt").into(),
    };
    session.add_user(request.trim().into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut first_write = None;
        let mut last_round = 0;
        let mut last_plan_check = String::new();
        let mut seen_call_ids = BTreeSet::new();
        let mut seen_result_ids = BTreeSet::new();
        let mut call_signatures = BTreeMap::<String, (String, String)>::new();
        let mut signature_counts = BTreeMap::<(String, String), usize>::new();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Tool { name, status, .. } => {
                    eprintln!("[live] tool={name} status={status}");
                }
                AgentEvent::Notice { text, .. } => {
                    eprintln!("[live] notice={text}");
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
                            && let Some(result) = tool_result_for_report(&s, message)
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
                    if s.task_rounds > last_round {
                        last_round = s.task_rounds;
                        let unread = tools::unread_citations(&s).map_or(0, |unread| unread.len());
                        eprintln!(
                            "[live] round={} input={} output={} document_written={} unread_citations={} checkpoint={} ladder={}/{} best={} closing={}",
                            s.task_rounds,
                            s.input_tokens,
                            s.output_tokens,
                            s.document_written,
                            unread,
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
                        );
                    }
                    // Plan checks are pointed out once per plan state.
                    if let Some(check) = s.run_guidance["plan_check"].as_str()
                        && check != last_plan_check
                    {
                        eprintln!("[live] plan_check={check}");
                        check.clone_into(&mut last_plan_check);
                    }
                }
                _ => {}
            }
        }
        first_write
    });
    let start = Instant::now();
    let mut result = agent::run_session(
        session,
        Arc::new(OpenAiClient),
        CancellationToken::new(),
        tx,
    )
    .await;
    let first_write = drain.await.unwrap();
    let source_unchanged = tools::project_fingerprint(&result.project).unwrap() == before;
    let original_unchanged = std::fs::read(&original_output)
        .ok()
        .map(|b| tools::hash(&b))
        == original_hash;
    let audit =
        tools::audit_document(&mut result).unwrap_or_else(|e| json!({"error":e.to_string()}));
    let document = std::fs::read_to_string(&result.project.output).unwrap_or_default();
    let mut calls = Vec::new();
    let errors = tool_errors_for_report(&result);
    for message in result.history.bundles.iter().flat_map(|b| &b.messages) {
        calls.extend(
            message["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    let mut report = json!({"status":result.status,"error":result.last_error,"model":result.config.model,
        "budget_tokens":result.config.run_tokens,"budget_seconds":result.config.run_timeout_secs,
        "elapsed_seconds":start.elapsed().as_secs_f64(),"input_tokens":result.input_tokens,"output_tokens":result.output_tokens,
        "usage_incomplete":result.usage_incomplete,"model_rounds":result.task_rounds,"first_write":first_write,
        "tool_calls":calls,"tool_errors":errors,"audit":audit,
        "unread_citations":tools::unread_citations(&result).unwrap_or_default(),"source_unchanged":source_unchanged,"configured_output_unchanged":original_unchanged,
        "document_lines":document.lines().count(),"document":document});
    report.as_object_mut().unwrap().extend(
        report_diagnostics(&result, &audit)
            .as_object()
            .unwrap()
            .clone(),
    );
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
    assert_eq!(
        result.status,
        "complete",
        "error={:?}; stop_reason={:?}; gaps={:?}; final_hash={:?}",
        result.last_error,
        result.run_history.back().map(|run| run.reason.as_str()),
        result.completion_gaps,
        report["final_document_hash"]
    );
    assert_eq!(audit["structural_ok"], true);
    assert!(
        tools::unread_citations(&result)
            .unwrap_or_default()
            .is_empty(),
        "cited ranges remain unread"
    );
    if let Ok(limit) = std::env::var("MNEMOARC_DOC_MAX_INPUT") {
        assert!(
            result.input_tokens <= limit.parse::<usize>().unwrap(),
            "input token ceiling exceeded"
        );
    }
    // Structural checks are not proof. Inspect the saved document against
    // current source before claiming semantic correctness.
}
