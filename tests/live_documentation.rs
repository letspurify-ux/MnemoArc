//! Explicit paid evaluation; normal cargo test never calls a provider.
mod support;
use mnemoarc::{
    agent::{self, AgentEvent},
    config::{Config, Project, Secret},
    llm::{Completion, CompletionError, LlmClient, OpenAiClient},
    session::{RunRecord, Session},
    tools,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Keep review inputs, protocol retries and validation decisions observable in
/// the paid run report, including calls that never produced a complete verdict.
struct ReviewTrace(Arc<Mutex<Vec<Value>>>);

async fn finish_live_review(
    session: &mut Session,
    first: &str,
    started: Instant,
    limit: std::time::Duration,
) -> anyhow::Result<Vec<Value>> {
    tools::document_review::finish(session, first)?;
    let mut followups = Vec::new();
    while session.document_review.pending {
        let remaining = limit
            .checked_sub(started.elapsed())
            .ok_or_else(|| anyhow::anyhow!("live review deadline exhausted"))?;
        let request = tools::document_review::request(session)?;
        let (tx, mut rx) = mpsc::channel(32);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let response = tokio::time::timeout(
            remaining,
            OpenAiClient.complete(
                request.clone(),
                &session.config,
                CancellationToken::new(),
                tx,
            ),
        )
        .await??;
        drain.await?;
        tools::document_review::finish(session, &response.text)?;
        followups.push(json!({"request":request,"response":response.text,"usage":response.usage,
            "provider_attempts":response.attempts,"attempt_diagnostics":response.attempt_diagnostics,
            "provider":response.provider,"first_event_seconds":response.first_event_seconds}));
    }
    Ok(followups)
}

#[async_trait::async_trait]
impl LlmClient for ReviewTrace {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let payload = request["messages"][1]["content"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        // Completion reviews too: their unmet checks explain the repair that
        // follows them, and a live report had no record of one.
        let review = payload
            .as_ref()
            .is_some_and(|p| p["source_document_review"] == true || p["completion_review"] == true);
        let started = Instant::now();
        let result = OpenAiClient.complete(request, config, cancel, delta).await;
        if review {
            let record = match &result {
                Ok(response) => json!({"input":payload,"response":response.text,
                    "usage":response.usage,"provider_attempts":response.attempts,
                    "attempt_diagnostics":response.attempt_diagnostics,
                    "provider":response.provider,
                    "first_event_seconds":response.first_event_seconds,
                    "elapsed_seconds":started.elapsed().as_secs_f64()}),
                Err(error) => {
                    let provider = error.downcast_ref::<CompletionError>();
                    json!({"input":payload,"error":error.to_string(),
                        "provider_attempts":provider.map(CompletionError::attempts),
                        "attempt_diagnostics":provider.map(CompletionError::attempt_diagnostics).unwrap_or_default(),
                        "elapsed_seconds":started.elapsed().as_secs_f64()})
                }
            };
            self.0.lock().unwrap().push(record);
        }
        result
    }
}

#[tokio::test]
async fn live_review_trace_preserves_retries_after_success_and_failure() {
    use axum::{Router, http::header, response::IntoResponse, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if call == 1 {
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices":[{"delta":{"content":"{\"issues\":[]}"},"finish_reason":"stop"}]})),
                    ).into_response()
                } else {
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable").into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        retries: 1,
        ..support::compact_config()
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let records = Arc::new(Mutex::new(Vec::new()));
    let client = ReviewTrace(records.clone());
    let request = json!({"messages":[
        {"role":"system","content":"Test review"},
        {"role":"user","content":json!({"source_document_review":true}).to_string()}
    ]});
    let (tx, _rx) = mpsc::channel(8);
    let result = client
        .complete(request.clone(), &config, CancellationToken::new(), tx)
        .await
        .unwrap();
    assert_eq!(result.text, "{\"issues\":[]}");
    let (tx, _rx) = mpsc::channel(8);
    assert!(
        client
            .complete(request, &config, CancellationToken::new(), tx)
            .await
            .is_err()
    );
    // A completion review is recorded as well; an ordinary model request is
    // not a review.
    for (content, review) in [
        (json!({"completion_review":true}).to_string(), true),
        ("Write the manual.".to_owned(), false),
    ] {
        let request = json!({"messages":[
            {"role":"system","content":"Test"},
            {"role":"user","content":content}
        ]});
        let before = records.lock().unwrap().len();
        let (tx, _rx) = mpsc::channel(8);
        assert!(
            client
                .complete(request, &config, CancellationToken::new(), tx)
                .await
                .is_err()
        );
        assert_eq!(records.lock().unwrap().len(), before + usize::from(review));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[2]["input"]["completion_review"], true);
    assert_eq!(records[0]["provider_attempts"], 2);
    assert_eq!(records[1]["provider_attempts"], 2);
    assert_eq!(
        records[0]["attempt_diagnostics"],
        json!([
            {"attempt":1,"code":"http_503","reason":"http_503: temporarily unavailable","action":"retry"}
        ])
    );
    assert_eq!(
        records[1]["attempt_diagnostics"],
        json!([
            {"attempt":1,"code":"http_503","reason":"http_503: temporarily unavailable","action":"retry"},
            {"attempt":2,"code":"http_503","reason":"http_503: temporarily unavailable","action":"stop"}
        ])
    );
    server.abort();
}

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

fn env_bool(name: &str, fallback: bool) -> bool {
    match std::env::var(name).ok().as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        Some(value) => panic!("{name} must be a boolean, got {value:?}"),
        None => fallback,
    }
}

fn report_diagnostics(result: &Session, audit: &Value) -> Value {
    let final_document_hash = audit["hash"].as_str().map(str::to_owned).or_else(|| {
        std::fs::read(&result.project.output)
            .ok()
            .map(|bytes| tools::hash(&bytes))
    });
    let review_verdict = match tools::document_review::current_verdict(result) {
        tools::document_review::CurrentVerdict::Approved => "approved",
        tools::document_review::CurrentVerdict::Rejected(_) => "rejected_current",
        tools::document_review::CurrentVerdict::Unavailable => "unavailable_current",
        tools::document_review::CurrentVerdict::Unreviewed => "unreviewed",
    };
    let completion_verdict = match tools::completion_review::current_verdict(result) {
        tools::completion_review::CurrentVerdict::Approved => "approved",
        tools::completion_review::CurrentVerdict::Rejected(_) => "rejected_current",
        tools::completion_review::CurrentVerdict::Unavailable => "unavailable_current",
        // An earlier version was rejected and nothing has been reviewed since.
        tools::completion_review::CurrentVerdict::Unreviewed
            if tools::completion_review::prior_rejection(result).is_some() =>
        {
            "unreviewed_after_rejection"
        }
        tools::completion_review::CurrentVerdict::Unreviewed => "unreviewed",
    };
    json!({
        "completion_gaps":result.completion_gaps,
        "run_stop_reason":result.run_history.back().map(|run| run.reason.as_str()),
        "last_run":result.run_history.back(),
        "final_document_hash":final_document_hash,
        "review_target_hash":tools::document_review::review_target_hash(result),
        "review_verdict":review_verdict,
        "completion_verdict":completion_verdict,
    })
}

#[test]
fn live_report_records_completion_gaps_and_stop_reason() {
    let mut result = Session::new(Project::default(), support::compact_config());
    result.status = "complete_with_gaps".into();
    result.completion_gaps = vec!["문서 검토 — 현재 문서를 마감 전에 검토하지 못했습니다.".into()];
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
        document_review_failure: None,
        input_tokens: 10,
        output_tokens: 5,
        usage_estimated: false,
        rounds: 1,
        last_stage: "review".into(),
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
    assert_eq!(diagnostics["completion_verdict"], "unreviewed");
    assert_eq!(diagnostics["final_document_hash"], "current-hash");
    assert_eq!(diagnostics["review_target_hash"], Value::Null);
    assert_eq!(diagnostics["review_verdict"], "unreviewed");
}

#[test]
fn live_report_distinguishes_the_edited_document_from_its_last_review_target() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("source.rs"),
        "fn run() { for _ in 0..5 {} }\n",
    )
    .unwrap();
    let mut result = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("manual.md"),
            ..Default::default()
        },
        support::compact_config(),
    );
    result.add_user("Document the loop.".into());
    result.select_workflow("source_document").unwrap();
    tools::execute(
        &mut result,
        "document_edit",
        json!({"action":"create","text":"# Flow\nA while loop runs five times. source.rs:1\n"}),
    )
    .unwrap();
    tools::document_review::request(&mut result).unwrap();
    tools::document_review::finish(&mut result, &json!({"issues":[{
        "previous_id":null,"kind":"factual","document":{"start_line":2,"end_line":2,"quote":"A while loop runs five times."},
        "requirement_id":null,"sources":[{"path":"source.rs","start_line":1,"end_line":1,"quote":"fn run() { for _ in 0..5 {} }"}],
        "problem":"Flow: wrong loop type","correction":"Describe the for loop.","ui_labels":[]
    }]}).to_string()).unwrap();
    tools::document_review::request(&mut result).unwrap();
    tools::document_review::finish(
        &mut result,
        &json!({"decisions":[{"id":"F1","status":"confirmed",
        "reason":"The document says while; the source declares for.","duplicate_of":null}]})
        .to_string(),
    )
    .unwrap();
    let reviewed_hash = tools::document_review::review_target_hash(&result)
        .unwrap()
        .to_owned();
    let expected = result.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(
        &mut result,
        "document_edit",
        json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop","text":"A for loop"}),
    )
    .unwrap();
    let audit = tools::audit_document(&mut result).unwrap();
    let diagnostics = report_diagnostics(&result, &audit);
    assert_eq!(diagnostics["review_target_hash"], reviewed_hash);
    assert_ne!(diagnostics["final_document_hash"], reviewed_hash);
    assert_eq!(diagnostics["review_verdict"], "unreviewed");
}

#[tokio::test]
#[ignore = "paid provider; explicitly set MNEMOARC_LIVE_TEST=1"]
async fn glm_rejects_missing_required_ui_steps() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    assert_eq!(
        config.model, "z-ai/glm-5.3-flash",
        "use the existing GLM model"
    );
    if path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(path.with_extension("credentials.json")).unwrap(),
        )
        .unwrap();
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    config.output_tokens = config.output_tokens.min(1024);
    config.request_timeout_secs = config.request_timeout_secs.min(90);
    config.retries = 0;
    let mut project = config
        .projects
        .iter()
        .find(|project| project.name == "MnemoArc")
        .expect("registered MnemoArc project")
        .clone();
    let dir = tempfile::tempdir().unwrap();
    project.output = dir.path().join("incomplete-manual.md");
    std::fs::write(
        &project.output,
        "# MnemoArc 사용자 매뉴얼\n## 처음 설정\n모델 연결 정보는 입력합니다. frontend/src/fields.js:10-34\n연결 확인과 설정 저장 버튼은 미확인입니다. frontend/src/Settings.jsx:239-289 frontend/src/Settings.jsx:483-492\nAPI 키 보관 선택도 미확인입니다. frontend/src/Settings.jsx:374-417\n",
    )
    .unwrap();
    let mut session = Session::new(project, config);
    session.select_workflow("source_document").unwrap();
    session.add_user("frontend/src 브라우저 UI를 근거로 처음 설정 절차를 작성해줘. 모델 연결 정보 입력, 연결 확인, 설정 저장, API 키 보관 선택의 실제 버튼과 순서를 모두 설명하고, 확인 가능한 항목을 미확인으로 대체하지 마.".into());
    let request = tools::document_review::request(&mut session).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["more_document_pages"], false);
    assert_eq!(payload["more_evidence_pages"], false);
    let (tx, mut rx) = mpsc::channel(32);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let start = Instant::now();
    let completion = tokio::time::timeout(
        std::time::Duration::from_secs(150),
        OpenAiClient.complete(request, &session.config, CancellationToken::new(), tx),
    )
    .await
    .expect("review timeout")
    .expect("GLM review request");
    drain.await.unwrap();
    let followups = finish_live_review(
        &mut session,
        &completion.text,
        start,
        std::time::Duration::from_secs(150),
    )
    .await
    .unwrap();
    let report = json!({
        "model":session.config.model,"followup_reviews":followups,
        "elapsed_seconds":start.elapsed().as_secs_f64(),
        "attempts":completion.attempts,
        "usage":completion.usage,
        "review_text":completion.text,
        "review_issues":session.document_review.issues,
        "approved":tools::document_review::approved(&session),
    });
    if let Ok(path) = std::env::var("MNEMOARC_REVIEW_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    eprintln!("[live] targeted GLM review: {}", report);
    assert!(!tools::document_review::approved(&session));
    assert!(!session.document_review.issues.is_empty());
    let issues = session.document_review.issues.join(" ");
    for required in ["연결 확인", "설정 저장", "API 키"] {
        assert!(
            issues.contains(required),
            "review omitted {required}: {issues}"
        );
    }
    assert!(
        !issues.contains("프로젝트 폴더"),
        "unrelated scope: {issues}"
    );
}

#[tokio::test]
#[ignore = "paid provider; explicitly set MNEMOARC_LIVE_TEST=1"]
async fn registered_user_manual_review_preserves_citations() {
    assert_eq!(std::env::var("MNEMOARC_LIVE_TEST").as_deref(), Ok("1"));
    let path = PathBuf::from(std::env::var("MNEMOARC_LIVE_CONFIG").unwrap_or("config.toml".into()));
    let mut config = Config::load(&path, &BTreeMap::new()).unwrap();
    if let Ok(model) = std::env::var("MNEMOARC_LIVE_MODEL") {
        config.model = model;
    }
    config.output_tokens = config.output_tokens.min(8192);
    config.request_timeout_secs = config.request_timeout_secs.min(180);
    config.retries = 0;
    let project_name = std::env::var("MNEMOARC_LIVE_PROJECT").unwrap_or("MnemoArc".into());
    let mut project = config
        .projects
        .iter()
        .find(|project| project.name == project_name)
        .expect("registered MnemoArc project")
        .clone();
    project.audience = "일반 유저".into();
    project.purpose = "화면 사용법 안내".into();
    let original_output = project.output.clone();
    let original_hash = std::fs::read(&original_output)
        .ok()
        .map(|bytes| tools::hash(&bytes));
    let source_path = project.root.join("frontend/src/App.jsx");
    let source = std::fs::read_to_string(&source_path).unwrap();
    let source_lines: Vec<_> = source.lines().collect();
    let button = source_lines
        .iter()
        .position(|line| line.contains("className=\"new-session\""))
        .expect("new-session button");
    let button_end = button
        + source_lines[button..]
            .iter()
            .position(|line| line.contains("</button>"))
            .unwrap();
    assert!(
        source_lines[button..=button_end]
            .iter()
            .any(|line| line.contains("!state?.config.projects.length"))
    );
    let citation = format!("frontend/src/App.jsx:{}-{}", button, button_end + 1);
    let source_hash = tools::hash(source.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    let mut reports = vec![];
    for (case, claim, citation, expected_approved) in [
        (
            "valid_citation",
            "등록된 프로젝트가 없으면 사이드바의 **새 세션** 버튼을 사용할 수 없습니다.",
            citation.as_str(),
            true,
        ),
        (
            "false_claim",
            "등록된 프로젝트가 없어도 사이드바의 **새 세션** 버튼을 사용할 수 있습니다.",
            citation.as_str(),
            false,
        ),
        (
            "unrelated_citation",
            "등록된 프로젝트가 없으면 사이드바의 **새 세션** 버튼을 사용할 수 없습니다.",
            "frontend/src/App.jsx:1-4",
            false,
        ),
    ] {
        project.output = dir.path().join(format!("{case}.md"));
        let document =
            format!("# MnemoArc UI 사용자 매뉴얼\n## 새 세션 버튼\n{claim} {citation}\n");
        std::fs::write(&project.output, &document).unwrap();
        let mut session = Session::new(project.clone(), config.clone());
        session.select_workflow("source_document").unwrap();
        session.add_user("UI 사용자 매뉴얼의 '새 세션 버튼' 절만 작성해줘. 프로젝트가 하나도 등록되지 않았을 때 버튼을 사용할 수 있는지만 설명해줘. 다른 동작은 범위에 포함하지 마.".into());
        let request = tools::document_review::request(&mut session).unwrap();
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["more_document_pages"], false);
        assert_eq!(payload["more_evidence_pages"], false);
        let (tx, mut rx) = mpsc::channel(32);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let started = Instant::now();
        let completion = tokio::time::timeout(
            std::time::Duration::from_secs(210),
            OpenAiClient.complete(
                request.clone(),
                &session.config,
                CancellationToken::new(),
                tx,
            ),
        )
        .await
        .expect("review timeout")
        .expect("manual review request");
        drain.await.unwrap();
        assert!(completion.calls.is_empty(), "review must not call tools");
        let verdict = finish_live_review(
            &mut session,
            &completion.text,
            started,
            std::time::Duration::from_secs(210),
        )
        .await;
        let approved = tools::document_review::approved(&session);
        reports.push(json!({
            "case":case,"document":document,"request":request,
            "expected_approved":expected_approved,"approved":approved,
            "elapsed_seconds":started.elapsed().as_secs_f64(),
            "usage":completion.usage,"provider_attempts":completion.attempts,
            "review_text":completion.text,"followup_reviews":verdict.as_ref().ok(),"review_issues":session.document_review.issues,
            "review_state":session.document_review,
            "error":verdict.as_ref().err().map(ToString::to_string),
        }));
        if let Ok(path) = std::env::var("MNEMOARC_REVIEW_REPORT") {
            let report = json!({"model":config.model,"source_hash":source_hash,"cases":reports});
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
        eprintln!(
            "[live] {case}: approved={approved} expected={expected_approved} usage={:?} issues={:?}",
            completion.usage, session.document_review.issues
        );
        verdict.unwrap();
    }
    assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);
    assert_eq!(
        std::fs::read(original_output)
            .ok()
            .map(|bytes| tools::hash(&bytes)),
        original_hash
    );
    for report in reports {
        assert_eq!(
            report["approved"], report["expected_approved"],
            "{}: {}",
            report["case"], report["review_text"]
        );
    }
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
    config.source_document_review = env_bool("MNEMOARC_LIVE_SOURCE_DOCUMENT_REVIEW", true);
    config.completion_review_enabled = env_bool("MNEMOARC_LIVE_COMPLETION_REVIEW", true);
    eprintln!(
        "[live] budget_tokens={} budget_seconds={} source_document_review={} completion_review={}",
        config.run_tokens,
        config.run_timeout_secs,
        config.source_document_review,
        config.completion_review_enabled
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
        let mut reviews = Vec::new();
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
                    if s.document_review.attempts > reviews.len() {
                        reviews.push(json!(s.document_review));
                    }
                    if s.task_rounds > last_round {
                        last_round = s.task_rounds;
                        let unread = tools::unread_citations(&s).map_or(0, |unread| unread.len());
                        eprintln!(
                            "[live] round={} input={} output={} document_written={} unread_citations={} document_reviews={} finding_validations={} dismissed={} merged={} completion_reviews={} checkpoint={} ladder={}/{} best={} closing={} unrepaired_finals={}",
                            s.task_rounds,
                            s.input_tokens,
                            s.output_tokens,
                            s.document_written,
                            unread,
                            s.document_review.attempts,
                            s.document_review.validation_rounds,
                            s.document_review.dismissed_findings,
                            s.document_review.merged_findings,
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
        (first_write, reviews)
    });
    let review_calls = Arc::new(Mutex::new(Vec::new()));
    let start = Instant::now();
    let mut result = agent::run_session(
        session,
        Arc::new(ReviewTrace(review_calls.clone())),
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
        "source_document_review_enabled":result.config.source_document_review,
        "completion_review_enabled":result.config.completion_review_enabled,
        "completion_review":result.completion_review,
        "elapsed_seconds":start.elapsed().as_secs_f64(),"input_tokens":result.input_tokens,"output_tokens":result.output_tokens,
        "usage_incomplete":result.usage_incomplete,"model_rounds":result.task_rounds,"first_write":first_write,"review_attempts":reviews,
        "tool_calls":calls,"tool_errors":errors,"document_review":result.document_review,"audit":audit,
        "unread_citations":tools::unread_citations(&result).unwrap_or_default(),"source_unchanged":source_unchanged,"configured_output_unchanged":original_unchanged,
        "document_lines":document.lines().count(),"document":document,"review_calls":*review_calls.lock().unwrap()});
    report.as_object_mut().unwrap().extend(
        report_diagnostics(&result, &audit)
            .as_object()
            .unwrap()
            .clone(),
    );
    if let Ok(path) = std::env::var("MNEMOARC_DOC_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    for skip in &result.document_review.skip_log {
        eprintln!(
            "[live] review_page_skipped lines={} evidence_page={} error={}",
            skip["lines"], skip["evidence_page"], skip["error"]
        );
    }
    for drop in &result.document_review.issue_drop_log {
        eprintln!(
            "[live] review_issue_dropped lines={} evidence_page={} error={}",
            drop["lines"], drop["evidence_page"], drop["error"]
        );
    }
    for release in &result.document_review.released_id_log {
        eprintln!(
            "[live] review_id_released lines={} evidence_page={} previous_id={} reason={}",
            release["lines"], release["evidence_page"], release["previous_id"], release["reason"]
        );
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
        "error={:?}; stop_reason={:?}; gaps={:?}; final_hash={:?}; review_target_hash={:?}",
        result.last_error,
        result.run_history.back().map(|run| run.reason.as_str()),
        result.completion_gaps,
        report["final_document_hash"],
        report["review_target_hash"]
    );
    assert_eq!(audit["structural_ok"], true);
    if result.config.source_document_review {
        assert!(tools::document_review::approved(&result));
    } else {
        assert_eq!(result.document_review.attempts, 0);
    }
    if result.config.completion_review_enabled {
        assert!(result.completion_review.attempts > 0);
        assert_eq!(
            tools::completion_review::current_verdict(&result),
            tools::completion_review::CurrentVerdict::Approved
        );
    }
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
    // Structural checks and a model review are not proof. Inspect the saved
    // document against current source before claiming semantic correctness.
}
