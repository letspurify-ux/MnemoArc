mod support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient},
    session::Session,
    tools::{self, document_review},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn fixture() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), "function run(history) {\n  const turns = normalize(history);\n  for (let i = 0; i < 5; i++) {\n    work(turns);\n  }\n}\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            source_answer_review: false,
            ..Default::default()
        },
    );
    s.add_user(
        "Write a source document covering history normalization and all loop bounds.".into(),
    );
    tools::execute(&mut s, "task_state", json!({"action":"update","patch":{"workflow":"source_document","completion":["history normalization", "loop type and bounds"]}})).unwrap();
    let read = tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Flow\nA while loop runs work. main.js:4-5\n"}),
    )
    .unwrap();
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"flow","title":"Flow","section":"# Flow","status":"written"}),
    )
    .unwrap();
    tools::execute(&mut s,"investigation",json!({"action":"verify","id":"flow","source_ids":[read["source"]["id"]],"verification_note":"Compared body"})).unwrap();
    (dir, s)
}

#[test]
fn source_workflow_activates_tools_and_schema_prevents_guessing() {
    let (_dir, mut s) = fixture();
    assert!(s.task.require_investigation);
    for name in ["investigation", "document_edit", "document_audit"] {
        assert!(s.active_tools.contains(name));
    }
    let specs = tools::ToolRegistry::specs();
    let patch = &specs
        .iter()
        .find(|t| t.name == "task_state")
        .unwrap()
        .parameters["properties"]["patch"];
    assert_eq!(patch["additionalProperties"], false);
    assert_eq!(patch["properties"]["details"]["type"], "array");
    assert!(patch["properties"].get("investigations").is_none());
    let definitions = tools::ToolRegistry::definitions(&s);
    let edit = definitions
        .iter()
        .find(|t| t["function"]["name"] == "document_edit")
        .unwrap();
    assert_eq!(
        edit["function"]["parameters"]["oneOf"][1]["required"],
        json!(["text", "expected_hash"])
    );
    let revision = s.task.revision;
    tools::execute(&mut s, "task_state", json!({"action":"update","patch":{}})).unwrap();
    assert_eq!(s.task.revision, revision);
    assert!(
        tools::execute(
            &mut s,
            "task_state",
            json!({"action":"update","patch":{"workflow":"answer"}})
        )
        .is_err()
    );
    s.add_user("continue".into());
    assert_eq!(s.task.workflow, "source_document");
    s.add_user("Now answer a question only.".into());
    assert_eq!(s.task.workflow, "");
    assert!(!s.task.require_investigation);
}

#[test]
fn review_reads_declarations_is_bounded_and_invalidates_on_change() {
    let (dir, mut s) = fixture();
    let request = document_review::request(&mut s).unwrap();
    assert!(request.get("tools").is_none());
    let review_instruction = request["messages"][0]["content"].as_str().unwrap();
    assert!(review_instruction.contains("Inspect the document headings"));
    assert!(review_instruction.contains("edits in the relevant original sections"));
    assert!(review_instruction.contains("Preserve a user-requested follow-up section"));
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(payload["evidence"].to_string().contains("for (let i = 0"));
    assert!(
        payload["evidence"]
            .to_string()
            .contains("normalize(history)")
    );
    assert!(mnemoarc::context::count(&request, &s.config.model) <= 24000);
    assert!(!document_review::approved(&s));
    assert!(document_review::finish(&mut s, "not json").is_err());
    document_review::finish(
        &mut s,
        r#"{"issues":["Flow: for loop, not while; history omitted"]}"#,
    )
    .unwrap();
    assert!(!document_review::approved(&s));
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, "```json\n{\"issues\":[]}\n```").unwrap();
    assert!(document_review::approved(&s));
    std::fs::write(dir.path().join("main.js"), "changed\n").unwrap();
    assert!(!document_review::approved(&s));
    assert!(
        document_review::finish(&mut s, r#"{"issues":[]}"#)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

#[test]
fn write_reports_bad_mermaid_citations_without_rejecting_partial_draft() {
    let (_dir, mut s) = fixture();
    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    let result = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":expected,"text":"# Flow\nmain.js:1-6\n```mermaid\nA[missing.js:1-2]\n```\n```js\nexample.js:99\n```\n"}),
    );
    let result = result.unwrap();
    assert_eq!(result["citation_check"]["issue_count"], 1);
    assert_eq!(
        result["citation_check"]["issues"][0]["citation"],
        "missing.js:1-2"
    );
    assert!(s.document_written);
}

struct Reviewer {
    issues: bool,
    phase: Option<&'static str>,
}
#[async_trait]
impl LlmClient for Reviewer {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        _: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(review) = support::acceptance(&request) {
            return Ok(review);
        }
        let review = request["messages"][1]["content"]
            .as_str()
            .is_some_and(|s| s.contains("\"source_document_review\":true"));
        let text = if review {
            assert!(request.get("tools").is_none());
            assert!(config.output_tokens <= 4096);
            if self.issues {
                r#"{"issues":["Flow: incorrect loop type and missing history normalization"]}"#
            } else {
                r#"{"issues":[]}"#
            }
        } else {
            if let Some(phase) = self.phase {
                let state = request["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                let state: Value = serde_json::from_str(state.split_once('\n').unwrap().1).unwrap();
                if state["run_guidance"]["finalization_attempts"] == 0 {
                    assert_eq!(state["run_guidance"]["phase"], phase);
                } else {
                    assert_eq!(state["run_guidance"]["phase"], "verify");
                }
            }
            "Done"
        };
        tx.send(text.into()).await.ok();
        Ok(Completion {
            text: text.into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn review_gate_honors_configured_attempt_limit() {
    for (limit, issues) in [(1, false), (1, true), (2, true), (10, true)] {
        let (_dir, mut s) = fixture();
        s.config.review_limit = limit;
        s.config.run_tokens = 5_000_000;
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move {
            let mut deltas = Vec::new();
            while let Some(e) = rx.recv().await {
                if let AgentEvent::Delta { text, .. } = e {
                    deltas.push(text);
                }
            }
            deltas
        });
        let result = run_session(
            s,
            Arc::new(Reviewer {
                issues,
                phase: None,
            }),
            CancellationToken::new(),
            tx,
        )
        .await;
        let deltas = drain.await.unwrap();
        assert_eq!(
            result.status,
            if issues { "partial" } else { "complete" },
            "{:?}",
            result.last_error
        );
        assert_eq!(
            result.document_review.attempts,
            if issues { limit + 1 } else { 1 }
        );
        let finals = result
            .history
            .bundles
            .iter()
            .flat_map(|b| &b.messages)
            .filter(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
            .count();
        assert_eq!(finals, if issues { 0 } else { 1 });
        assert_eq!(
            deltas,
            if issues {
                vec![]
            } else {
                vec!["Done".to_string()]
            }
        );
    }
}

#[tokio::test]
async fn required_document_before_investigations_is_not_forced_to_chat_answer() {
    let (_dir, mut s) = fixture();
    s.investigations.clear();
    s.document_written = false;
    s.last_document_write = None;
    s.task_rounds = 6;
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        s,
        Arc::new(Reviewer {
            issues: false,
            phase: Some("draft"),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "partial");
    assert!(ContextManager::state(&result).unwrap()["document_review"].is_object());
}

#[test]
fn explicitly_cited_comments_survive_compaction_and_oversize_line_is_refused() {
    let (dir, mut s) = fixture();
    std::fs::write(
        dir.path().join("main.js"),
        "function run() {\n// Does not cancel the underlying operation.\n}\n",
    )
    .unwrap();
    std::fs::write(
        &s.project.output,
        "# Flow\nDoes not cancel the operation. main.js:2\n",
    )
    .unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(
        payload["evidence"]
            .to_string()
            .contains("2|// Does not cancel")
    );
    std::fs::write(&s.project.output, "huge source document ".repeat(30000)).unwrap();
    assert!(
        document_review::request(&mut s)
            .unwrap_err()
            .to_string()
            .contains("document_review_budget")
    );
}

#[test]
fn oversized_requirements_are_reported_before_document_paging() {
    let (_dir, mut s) = fixture();
    s.task.completion = vec!["review every branch and condition ".repeat(10_000)];
    let error = document_review::request(&mut s).unwrap_err().to_string();
    assert!(
        error.contains("requirements and review instructions"),
        "{error}"
    );
}

#[test]
fn oversized_multiline_document_is_reviewed_in_complete_bounded_ranges() {
    let (_dir, mut s) = fixture();
    let doc = (1..=900)
        .map(|i| {
            format!(
                "Line {i}: {} main.js:1-6\n",
                "The backend must preserve source evidence and compare every branch before approval. ".repeat(3)
            )
        })
        .collect::<String>();
    assert!(mnemoarc::context::tokens(&doc, &s.config.model) > 24_000);
    std::fs::write(&s.project.output, &doc).unwrap();
    s.document_review = Default::default();
    let mut expected_start = 1;
    let mut pages = 0;
    loop {
        let request = document_review::request(&mut s).unwrap();
        assert!(mnemoarc::context::count(&request, &s.config.model) <= 24_000);
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        let start = payload["document_line_start"].as_u64().unwrap() as usize;
        let end = payload["document_line_end"].as_u64().unwrap() as usize;
        assert_eq!(start, expected_start);
        assert!(end >= start && end <= 900);
        assert_eq!(
            payload["document"].as_str().unwrap().lines().count(),
            end - start + 1
        );
        assert!(!payload["evidence"].as_array().unwrap().is_empty());
        expected_start = end + 1;
        pages += 1;
        document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
        if !s.document_review.pending {
            break;
        }
        assert!(!document_review::approved(&s));
        assert_eq!(s.document_review.attempts, 0);
        assert!(pages < 32);
    }
    assert_eq!(expected_start, 901);
    assert!(pages > 1);
    assert_eq!(s.document_review.attempts, 1);
    assert!(document_review::approved(&s));
}

#[test]
fn review_keeps_the_last_line_citation_on_its_document_page() {
    let (dir, mut s) = fixture();
    let source = (1..=200)
        .map(|i| format!("const LINE_{i} = {i};\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    let mut doc = String::from("# Intro\n");
    for i in 2..=59 {
        doc.push_str(&format!(
            "Uncited context {i}: this is reviewed before the cited boundary.\n"
        ));
    }
    doc.push_str("Claim at the page boundary: main.js:100\n");
    doc.push_str("## Later\nClaim after the boundary: main.js:200\n");
    doc.push_str(&"Later context without another boundary.\n".repeat(200));
    std::fs::write(&s.project.output, doc).unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let end = payload["document_line_end"].as_u64().unwrap() as usize;
    let evidence = payload["evidence"].to_string();
    assert_eq!(end, 60);
    assert!(
        evidence.contains("100|const LINE_100"),
        "last document line {end} was not included in evidence"
    );
}

#[test]
fn review_keeps_citations_on_the_first_line_of_each_document_page() {
    let (dir, mut s) = fixture();
    let source = (1..=200)
        .map(|i| format!("const LINE_{i} = {i};\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    let mut doc = String::from("First page context. main.js:1\n");
    doc.push_str(&"Uncited context that fills the first review page.\n".repeat(120));
    doc.push_str("Second page boundary claim: main.js:200\n");
    std::fs::write(&s.project.output, doc).unwrap();

    let first = document_review::request(&mut s).unwrap();
    let first_payload: Value =
        serde_json::from_str(first["messages"][1]["content"].as_str().unwrap()).unwrap();
    let first_end = first_payload["document_line_end"].as_u64().unwrap() as usize;
    assert!(
        first_payload["evidence"]
            .to_string()
            .contains("1|const LINE_1")
    );
    document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();

    let second = document_review::request(&mut s).unwrap();
    let second_payload: Value =
        serde_json::from_str(second["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(second_payload["document_line_start"], first_end + 1);
    assert!(
        second_payload["evidence"]
            .to_string()
            .contains("200|const LINE_200"),
        "citation at the next page start was omitted"
    );
}

#[test]
fn review_allows_an_uncited_page_before_later_citations() {
    let (dir, mut s) = fixture();
    std::fs::write(dir.path().join("main.js"), "const VALUE = 1;\n").unwrap();
    let mut doc = String::new();
    for i in 1..=500 {
        doc.push_str(&format!(
            "Uncited context {i}: {}\n",
            "This page has no source citation but must still be reviewed. ".repeat(8)
        ));
    }
    doc.push_str("\nCited conclusion: main.js:1\n");
    std::fs::write(&s.project.output, doc).unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(payload["document_line_end"].as_u64().unwrap() < 500);
    assert!(payload["evidence"].as_array().unwrap().is_empty());
    assert_eq!(payload["more_document_pages"], true);
}

#[test]
fn editing_document_between_ranges_restarts_review_and_discards_old_findings() {
    let (_dir, mut s) = fixture();
    let doc = (1..=220)
        .map(|i| format!("Line {i}: main.js:1-6\n"))
        .collect::<String>();
    std::fs::write(&s.project.output, &doc).unwrap();
    s.document_review = Default::default();
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":["Old revision issue"]}"#).unwrap();
    assert!(s.document_review.pending);
    std::fs::write(&s.project.output, format!("{doc}New line: main.js:1-6\n")).unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["document_line_start"], 1);
    assert_eq!(payload["evidence_page"], 0);
    assert_eq!(s.document_review.attempts, 0);
    while s.document_review.pending {
        document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
        if s.document_review.pending {
            document_review::request(&mut s).unwrap();
        }
    }
    assert!(s.document_review.issues.is_empty());
    assert!(document_review::approved(&s));
}

#[test]
fn document_ranges_keep_a_mermaid_section_together_when_it_fits() {
    let (_dir, mut s) = fixture();
    let mut doc = String::from("# Intro\n");
    doc.push_str(&"Intro context. main.js:1-6\n".repeat(59));
    doc.push_str("## Diagram\n```mermaid\n");
    doc.push_str(&"A --> B\n".repeat(60));
    doc.push_str("```\n## End\n");
    doc.push_str(&"Conclusion. main.js:1-6\n".repeat(100));
    std::fs::write(&s.project.output, doc).unwrap();
    s.document_review = Default::default();
    let first = document_review::request(&mut s).unwrap();
    let first: Value =
        serde_json::from_str(first["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(first["document_line_end"], 60);
    document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    let second = document_review::request(&mut s).unwrap();
    let second: Value =
        serde_json::from_str(second["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(second["document_line_start"], 61);
    assert_eq!(second["document_line_end"], 123);
    assert!(second["document"].as_str().unwrap().contains("```mermaid"));
    assert!(second["document"].as_str().unwrap().ends_with("```"));
}

struct StallingRepair;
#[async_trait]
impl LlmClient for StallingRepair {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(review) = support::acceptance(&request) {
            return Ok(review);
        }
        let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if content.contains("[Current program state") || content.starts_with("CHECKPOINT CONTROL") {
            let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1).unwrap();
            if let Some(id) = state["checkpoint"]["id"].as_str() {
                return Ok(Completion {
                    calls: vec![mnemoarc::llm::ToolCall {
                        id: format!("cleanup-{id}"),
                        name: "checkpoint_complete".into(),
                        arguments: json!({"id":id,"progress":"Document repair remains pending","no_save_reason":"Unchanged edits produced no new findings"}).to_string(),
                    }],
                    ..Default::default()
                });
            }
            if state["document_review"]["attempts"] == 1 {
                return Ok(Completion {
                    calls: vec![mnemoarc::llm::ToolCall {
                        id: format!("stall-{}", state["run_guidance"]["task_rounds"]),
                        name: "document_edit".into(),
                        arguments: json!({"action":"patch","expected_hash":state["document_review"]["target_hash"],"old_text":"A while loop","text":"A while loop"}).to_string(),
                    }],
                    ..Default::default()
                });
            }
        }
        Reviewer {
            issues: true,
            phase: None,
        }
        .complete(request, config, cancel, tx)
        .await
    }
}

#[tokio::test]
async fn failed_review_cannot_open_an_unbounded_repair_loop() {
    let (_dir, s) = fixture();
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, Arc::new(StallingRepair), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert_eq!(result.document_review.attempts, 4);
    assert_eq!(result.document_review.stalled_attempts, 3);
    assert!(
        result
            .last_error
            .unwrap()
            .contains("document_review_no_progress")
    );
    assert!(result.task_rounds < 40 + result.checkpoints_completed);
}

#[test]
fn grouped_citation_ranges_are_all_audited_and_supplied_to_reviewer() {
    let (dir, mut s) = fixture();
    let source = (1..=50)
        .map(|i| format!("const VALUE_{i} = {i};\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    std::fs::write(&s.project.output, "# Flow\nmain.js:1, 40-41\n").unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(
        payload["evidence"]
            .to_string()
            .contains("40|const VALUE_40")
    );
    let audit = tools::audit_document(&mut s).unwrap();
    assert_eq!(audit["citations_checked"], 2);
    std::fs::write(&s.project.output, "# Flow\nmain.js:1, 999-1000\n").unwrap();
    let audit = tools::audit_document(&mut s).unwrap();
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "citation_range")
    );
}

#[tokio::test]
async fn checkpoint_maintenance_does_not_consume_document_repair_requests() {
    let (_dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":["Correct the loop type"]}"#).unwrap();
    s.task_rounds = 20; // Time spent in other control requests is not document repair.
    s.document_review.repair_requests = 0;
    ContextManager::prepare(&mut s, 60000).unwrap();
    struct Cleanup;
    #[async_trait]
    impl LlmClient for Cleanup {
        async fn complete(
            &self,
            request: Value,
            _: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
            if let Some(review) = support::acceptance(&request) {
                return Ok(review);
            }
            let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1)?;
            if state["checkpoint"].is_null() {
                anyhow::bail!("stop_after_cleanup");
            }
            assert_eq!(state["document_review"]["repair_requests"], 0);
            Ok(Completion { calls:vec![mnemoarc::llm::ToolCall {
                id:"ack".into(), name:"checkpoint_complete".into(),
                arguments:json!({"id":state["checkpoint"]["id"],"progress":"Preserve unresolved loop issue", "no_save_reason":"No new findings"}).to_string()
            }], ..Default::default() })
        }
    }
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, Arc::new(Cleanup), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(result.checkpoints_completed, 1);
    assert_eq!(result.document_review.repair_requests, 0);
    assert_eq!(result.last_error.as_deref(), Some("stop_after_cleanup"));
}

struct MalformedReview {
    calls: std::sync::Mutex<usize>,
    always_bad: bool,
}
#[async_trait]
impl LlmClient for MalformedReview {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(review) = support::acceptance(&request) {
            return Ok(review);
        }
        let payload = request["messages"][1]["content"].as_str().unwrap();
        if payload.contains("\"source_document_review\":true") {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls > 1 {
                assert!(payload.contains("document_review_invalid"));
            }
            return Ok(Completion {
                text: if self.always_bad || *calls == 1 {
                    "invalid JSON".into()
                } else {
                    r#"{"issues":[]}"#.into()
                },
                ..Default::default()
            });
        }
        Ok(Completion {
            text: "Done".into(),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn malformed_review_has_bounded_recovery_without_consuming_valid_review_budget() {
    for always_bad in [false, true] {
        let (_dir, s) = fixture();
        let model = Arc::new(MalformedReview {
            calls: std::sync::Mutex::new(0),
            always_bad,
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(s, model.clone(), CancellationToken::new(), tx).await;
        drain.await.unwrap();
        assert_eq!(*model.calls.lock().unwrap(), if always_bad { 8 } else { 2 });
        if always_bad {
            assert_eq!(result.status, "partial");
            assert!(
                result
                    .last_error
                    .unwrap()
                    .starts_with("document_review_invalid:")
            );
            assert_eq!(result.document_review.attempts, 0);
            assert!(result.document_review.pending);
        } else {
            assert_eq!(result.status, "complete", "{:?}", result.last_error);
            assert_eq!(result.document_review.attempts, 1);
        }
    }
}

#[test]
fn review_pages_cover_all_evidence_before_approval_and_accumulate_findings() {
    let (dir, mut s) = fixture();
    let lines = 1800;
    let source: String = (1..=lines)
        .map(|i| format!("const value_{i} = normalize(history, {i}, 'input-{i}');\n"))
        .collect();
    std::fs::write(dir.path().join("main.js"), &source).unwrap();
    std::fs::write(
        &s.project.output,
        format!("# Flow\nAll processing: main.js:1-{lines}\n"),
    )
    .unwrap();
    s.document_review = Default::default();
    let mut seen = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let req = document_review::request(&mut s).unwrap();
        assert!(mnemoarc::context::count(&req, &s.config.model) <= 24000);
        let payload: Value =
            serde_json::from_str(req["messages"][1]["content"].as_str().unwrap()).unwrap();
        let manifest = payload["evidence_manifest"].as_array().unwrap();
        assert!(!manifest.is_empty());
        let first = payload["first_evidence_chunk"].as_u64().unwrap() as usize;
        for (index, chunk) in manifest.iter().enumerate() {
            assert_eq!(chunk["reviewed_on_prior_page"], index < first);
            assert!(std::path::Path::new(chunk["path"].as_str().unwrap()).ends_with("main.js"));
            assert!(chunk["first_line"].as_u64().unwrap() <= chunk["last_line"].as_u64().unwrap());
        }
        assert_eq!(first > 0, pages > 0);
        assert!(
            payload["page_scope"]
                .as_str()
                .unwrap()
                .contains("independent request")
        );
        for chunk in payload["evidence"].as_array().unwrap() {
            for line in chunk["numbered_text"].as_str().unwrap().lines() {
                seen.insert(line.split_once('|').unwrap().0.parse::<usize>().unwrap());
            }
        }
        pages += 1;
        document_review::finish(
            &mut s,
            if pages == 1 {
                r#"{"issues":["Flow: preserve numeric cap"]}"#
            } else {
                r#"{"issues":[]}"#
            },
        )
        .unwrap();
        if !s.document_review.pending {
            break;
        }
        assert_eq!(s.document_review.attempts, 0);
        assert!(!document_review::approved(&s));
        assert!(pages < 32);
    }
    assert!(pages > 1);
    assert_eq!(seen.len(), lines);
    assert_eq!(s.document_review.attempts, 1);
    assert!(!s.document_review.evidence_omitted);
    assert_eq!(s.document_review.issues, vec!["Flow: preserve numeric cap"]);
    assert!(!document_review::approved(&s));
}

#[test]
fn changing_evidence_between_pages_restarts_review_without_old_findings() {
    let (dir, mut s) = fixture();
    let source: String = (1..=1800)
        .map(|i| format!("const value_{i} = normalize(history, {i}, 'input-{i}');\n"))
        .collect();
    std::fs::write(dir.path().join("main.js"), &source).unwrap();
    std::fs::write(&s.project.output, "# Flow\nmain.js:1-1800\n").unwrap();
    s.document_review = Default::default();
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":["Old revision issue"]}"#).unwrap();
    assert!(s.document_review.pending);
    assert_eq!(s.document_review.evidence_page, 1);
    std::fs::write(
        dir.path().join("main.js"),
        format!("{source}// new revision\n"),
    )
    .unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["evidence_page"], 0);
    assert!(!document_review::approved(&s));
    assert_eq!(s.document_review.attempts, 0);
}

#[test]
fn repair_limit_defaults_and_validates() {
    let mut config: Config = serde_json::from_value(json!({})).unwrap();
    assert_eq!(config.document_repair_limit, 8);
    config.document_repair_limit = 0;
    assert!(config.validate().is_err());
    config.document_repair_limit = 16;
    let encoded = toml::to_string(&config).unwrap();
    assert_eq!(
        toml::from_str::<Config>(&encoded)
            .unwrap()
            .document_repair_limit,
        16
    );
}

async fn run_repair_test(s: Session, client: Arc<dyn LlmClient>) -> Session {
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    drain.await.unwrap();
    result
}

#[tokio::test]
async fn repair_interval_rechecks_and_a_changed_document_can_resume() {
    let (_dir, mut s) = fixture();
    s.config.document_repair_limit = 2;
    let mut s = run_repair_test(s, Arc::new(StallingRepair)).await;
    assert_eq!(s.status, "partial");
    assert_eq!(s.document_review.attempts, 4);
    assert_eq!(s.document_review.stalled_attempts, 3);
    assert!(!document_review::approved(&s));
    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop runs work. main.js:4-5","text":"History is normalized before a bounded for loop runs work. main.js:2-5"}),
    )
    .unwrap();
    let source = s
        .sources
        .values()
        .find(|source| source.origin == "file")
        .unwrap()
        .id
        .clone();
    tools::execute(&mut s, "investigation", json!({"action":"verify","id":"flow","source_ids":[source],"verification_note":"Compared the corrected loop and history statement with the source"})).unwrap();
    assert!(!document_review::stalled_on_current_result(&s));
    s.add_user("계속".into());
    let s = run_repair_test(
        s,
        Arc::new(Reviewer {
            issues: false,
            phase: None,
        }),
    )
    .await;
    assert_eq!(s.status, "complete");
    assert!(document_review::approved(&s));
}

#[test]
fn resolving_review_issues_allows_more_than_the_total_review_count() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 2;
    for issues in [
        vec!["loop", "history", "bounds"],
        vec!["history", "bounds"],
        vec!["bounds"],
    ] {
        document_review::request(&mut s).unwrap();
        document_review::finish(&mut s, &json!({"issues":issues}).to_string()).unwrap();
        assert_eq!(s.document_review.stalled_attempts, 0);
    }
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert_eq!(s.document_review.attempts, 4);
    assert!(document_review::approved(&s));
}

#[test]
fn additive_sections_keep_document_review_open_for_remaining_work() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 2;
    for n in 1..=3 {
        document_review::request(&mut s).unwrap();
        document_review::finish(&mut s, r#"{"issues":["Finish the requested overview"]}"#).unwrap();
        assert_eq!(s.document_review.stalled_attempts, 0);
        if n < 3 {
            let expected = s.last_document_write.as_ref().unwrap().1.clone();
            tools::execute(&mut s, "document_edit", json!({"action":"append","expected_hash":expected,"text":format!("\n## Extra {n}\nDocumented part {n}.\n")})).unwrap();
        }
    }
    assert_eq!(s.document_review.attempts, 3);
    assert!(!document_review::stalled_on_current_result(&s));
}

#[test]
fn first_issue_after_an_approved_document_gets_a_repair_chance() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 1;
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));
    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(&mut s, "document_edit", json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop","text":"A for loop"})).unwrap();
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":["Finish history normalization"]}"#).unwrap();
    assert_eq!(s.document_review.stalled_attempts, 0);
    assert!(!document_review::stalled_on_current_result(&s));
}

struct ProgressiveDocumentRepair {
    calls: std::sync::Mutex<usize>,
    path: std::path::PathBuf,
    source_id: String,
}

#[async_trait]
impl LlmClient for ProgressiveDocumentRepair {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(review) = support::acceptance(&request) {
            return Ok(review);
        }
        if request["messages"][1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("\"source_document_review\":true"))
        {
            let doc = std::fs::read_to_string(&self.path)?;
            let issues = if doc.contains("A while loop") {
                vec!["Flow: correct the loop and history statement"]
            } else {
                vec![]
            };
            return Ok(Completion {
                text: json!({"issues":issues}).to_string(),
                ..Default::default()
            });
        }
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let response = match *calls {
            1 | 7..=9 => Completion {
                text: "The document is complete.".into(),
                ..Default::default()
            },
            2 | 3 => Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: format!("add-{calls}"),
                    name: "document_edit".into(),
                    arguments: json!({"action":"append","expected_hash":tools::hash(&std::fs::read(&self.path)?),"text":format!("\n## Detail {calls}\nAdded detail.\n")}).to_string(),
                }],
                ..Default::default()
            },
            4 | 5 => Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: format!("correct-{calls}"),
                    name: "document_edit".into(),
                    arguments: json!({"action":"replace_text","expected_hash":tools::hash(&std::fs::read(&self.path)?),"old_text":"A while loop runs work. main.js:4-5","text":"History is normalized before a bounded for loop runs work. main.js:2-5"}).to_string(),
                }],
                ..Default::default()
            },
            6 => Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: "verify-flow".into(),
                    name: "investigation".into(),
                    arguments: json!({"action":"verify","id":"flow","source_ids":[self.source_id],"verification_note":"Compared the corrected history and loop statement with the source"}).to_string(),
                }],
                ..Default::default()
            },
            _ => anyhow::bail!("test_limit: progressive document repair did not finish at call {calls}"),
        };
        Ok(response)
    }
}

#[tokio::test]
async fn progressive_document_repairs_reach_final_approval_past_both_intervals() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 2;
    s.config.document_repair_limit = 2;
    s.config.run_tokens = 5_000_000;
    let client = Arc::new(ProgressiveDocumentRepair {
        calls: std::sync::Mutex::new(0),
        path: s.project.output.clone(),
        source_id: s
            .sources
            .values()
            .find(|source| source.origin == "file")
            .unwrap()
            .id
            .clone(),
    });
    let result = run_repair_test(s, client.clone()).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.document_review.attempts, 3);
    assert!(document_review::approved(&result));
    assert_eq!(*client.calls.lock().unwrap(), 8);
}

#[tokio::test]
async fn reads_and_verification_can_finish_even_at_the_edit_limit() {
    struct ReadAndVerify(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl LlmClient for ReadAndVerify {
        async fn complete(
            &self,
            request: Value,
            config: &Config,
            cancel: CancellationToken,
            tx: mpsc::Sender<String>,
        ) -> Result<Completion> {
            if let Some(review) = support::acceptance(&request) {
                return Ok(review);
            }
            let round = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if round < 9 {
                let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1)?;
                assert_eq!(state["document_review"]["repair_requests"], 2);
                let name = ["file_read", "document_inspect", "investigation"][round % 3];
                let args = match name {
                    "file_read" => json!({"path":"main.js","force_read":true}),
                    "document_inspect" => json!({}),
                    _ => {
                        json!({"action":"verify_batch","items":{"flow":{"source_ids":[],"verification_note":"Reuse unchanged attestation"}}})
                    }
                };
                return Ok(Completion {
                    calls: vec![mnemoarc::llm::ToolCall {
                        id: format!("read-{round}"),
                        name: name.into(),
                        arguments: args.to_string(),
                    }],
                    ..Default::default()
                });
            }
            Reviewer {
                issues: false,
                phase: None,
            }
            .complete(request, config, cancel, tx)
            .await
        }
    }
    let (_dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    document_review::finish(&mut s, r#"{"issues":["Check the existing section"]}"#).unwrap();
    // This test exercises the edit cap across many reads, not checkpoint cleanup.
    s.config.context_tokens = 128_000;
    s.config.document_repair_limit = 2;
    s.document_review.repair_requests = 2;
    let result = run_repair_test(
        s,
        Arc::new(ReadAndVerify(std::sync::atomic::AtomicUsize::new(0))),
    )
    .await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(document_review::approved(&result));
    assert!(result.task_rounds >= 11); // nine non-edit requests + final + paged review
}
