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
        json!(["expected_hash"])
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
            if issues { limit } else { 1 }
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
fn explicitly_cited_comments_survive_compaction_and_oversize_review_is_refused() {
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
        let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if content.contains("[Current program state") {
            let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1).unwrap();
            if state["document_review"]["attempts"] == 1 {
                return Ok(Completion {
                    calls: vec![mnemoarc::llm::ToolCall {
                        id: format!("stall-{}", state["run_guidance"]["task_rounds"]),
                        name: "task_state".into(),
                        arguments: json!({"action":"read"}).to_string(),
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
    assert_eq!(result.status, "partial");
    assert_eq!(result.document_review.attempts, 1);
    assert!(result.last_error.unwrap().contains("document_repair_limit"));
    assert_eq!(result.task_rounds, 10); // initial final + review + eight repair requests
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
    assert_eq!(result.document_review.repair_requests, 1);
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
        assert_eq!(*model.calls.lock().unwrap(), if always_bad { 3 } else { 2 });
        if always_bad {
            assert_eq!(result.status, "blocked");
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
