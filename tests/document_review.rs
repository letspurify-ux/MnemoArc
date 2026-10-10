use crate::support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::AgentEvent,
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient},
    session::Session,
    tools::{self, document_review},
};
use serde_json::{Value, json};
use std::sync::Arc;
use support::document_review::run_session;
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
            ..support::compact_config()
        },
    );
    s.add_user(
        "Write a source document covering history normalization and all loop bounds.".into(),
    );
    s.select_workflow("source_document").unwrap();
    tools::execute(&mut s, "task_state", json!({"action":"update","patch":{"completion":["history normalization", "loop type and bounds"]}})).unwrap();
    tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Flow\nA while loop runs work. main.js:4-5\n"}),
    )
    .unwrap();
    (dir, s)
}

#[test]
fn review_uses_caller_criteria_without_promoting_agent_workflow_checks() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("manual.md");
    std::fs::write(dir.path().join("main.js"), "function openChat() {}\n").unwrap();
    std::fs::write(
        &output,
        "# Screen manual\nOpen the chat screen. main.js:1\n",
    )
    .unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output,
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            ..support::compact_config()
        },
    );
    s.task.completion = vec!["Include the named controls in the manual".into()];
    s.task.constraints = vec!["Use Korean".into()];
    s.task.deliverables = vec!["A Markdown manual".into()];
    s.add_user("Write a screen manual".into());
    s.select_workflow("source_document").unwrap();
    s.task
        .completion
        .push("Register and verify each investigation item".into());
    s.task
        .deliverables
        .push("Internal investigation ledger".into());

    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    // The request and the caller's criteria are sent once, in the catalog.
    assert_eq!(
        payload["requirement_catalog"],
        json!({"R0":"Write a screen manual","C1":"Include the named controls in the manual",
            "K1":"Use Korean","D1":"A Markdown manual"})
    );
    for key in [
        "request",
        "requirements",
        "constraints",
        "deliverables",
        "request_history",
    ] {
        assert!(payload.get(key).is_none(), "{key}");
    }
    assert_eq!(
        payload.to_string().matches("Write a screen manual").count(),
        1
    );
    assert!(!payload.to_string().contains("investigation"));

    s.add_user("continue".into());
    assert_eq!(
        s.request_review_criteria.completion,
        ["Include the named controls in the manual"]
    );
}

#[test]
fn a_review_lists_the_citations_into_test_code() {
    // A live reviewer approved a test's loop cited as how reviews repeat.
    let (dir, mut s) = fixture();
    std::fs::write(
        dir.path().join("main.test.js"),
        "it('runs', () => run([]));\n",
    )
    .unwrap();
    tools::execute(&mut s, "file_read", json!({"path":"main.test.js"})).unwrap();
    let current = tools::hash(&std::fs::read(dir.path().join("out.md")).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":current,"text":"# Flow\nWork runs in a loop. main.js:4-5\nRuns are checked. main.test.js:1\n"}),
    )
    .unwrap();
    let request = document_review::request(&mut s).unwrap();
    assert!(
        request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("test_code_citations, when present")
    );
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload["test_code_citations"],
        json!([{"line":3,"citation":"main.test.js:1"}])
    );
}

#[test]
fn review_of_an_unprepared_request_does_not_use_agent_completion_checks() {
    let (_dir, mut s) = fixture();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload["requirement_catalog"],
        json!({"R0":"Write a source document covering history normalization and all loop bounds."})
    );
    assert!(payload.get("request").is_none());
}

#[test]
fn source_workflow_activates_tools_and_schema_prevents_guessing() {
    let (_dir, mut s) = fixture();
    assert!(s.is_document_work());
    for name in ["document_edit", "document_audit"] {
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
    // Per-action unions are not offered to the model; execution enforces them.
    assert!(edit["function"]["parameters"].get("oneOf").is_none());
    // Without the model's own last write as a known version, a section
    // insertion needs the hash.
    s.last_document_write = None;
    assert!(
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"insert_after","section":"# Flow","text":"# Extra\n"})
        )
        .unwrap_err()
        .to_string()
        .contains("expected_hash")
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
    // The user switches the session to answers for the next request.
    s.workflow_mode = "answer".into();
    s.add_user("Now answer a question only.".into());
    assert_eq!(s.task.workflow, "answer");
    assert!(!s.is_document_work());
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
    assert!(support::document_review::finish(&mut s, "not json").is_err());
    support::document_review::finish(
        &mut s,
        r#"{"issues":["Flow: for loop, not while; history omitted"]}"#,
    )
    .unwrap();
    assert!(!document_review::approved(&s));
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, "```json\n{\"issues\":[]}\n```").unwrap();
    assert!(document_review::approved(&s));
    std::fs::write(dir.path().join("main.js"), "changed\n").unwrap();
    assert!(!document_review::approved(&s));
    assert!(
        support::document_review::finish(&mut s, r#"{"issues":[]}"#)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

#[test]
fn empty_model_verdict_does_not_approve_a_materially_short_document() {
    let (_dir, mut s) = fixture();
    s.answer_review_question = "사용자 매뉴얼을 120줄 내외로 작성해줘.".into();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(!document_review::approved(&s));
    assert!(
        s.document_review
            .issues
            .iter()
            .any(|issue| issue.contains("실제 2줄"))
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
        // A resumed repair can cross the context boundary. Complete the
        // requested maintenance before producing the scripted review/final.
        let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if content.starts_with("CHECKPOINT CONTROL") {
            let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1)?;
            let id = state["checkpoint"]["id"].as_str().unwrap();
            return Ok(Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: format!("reviewer-cleanup-{id}"),
                    name: "checkpoint_complete".into(),
                    arguments: json!({"id":id,"progress":"Continue reviewing the corrected document","no_save_reason":"The source and repair are already retained; no new findings"}).to_string(),
                }],
                ..Default::default()
            });
        }
        let review = request["messages"][1]["content"]
            .as_str()
            .is_some_and(|s| s.contains("\"source_document_review\":true"));
        let text = if review {
            assert!(request.get("tools").is_none());
            // A timed-out review is not retried as is; it is halved or skipped.
            assert!(!config.retry_timeouts);
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
async fn unchanged_review_is_reused_until_closing_reports_its_findings() {
    for (limit, issues) in [(1, false), (1, true), (2, true), (10, true)] {
        let (_dir, mut s) = fixture();
        s.config.review_limit = limit;
        s.config.run_tokens = 200_000;
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
        // An unchanged rejected document is never re-reviewed. Closing mode
        // finishes it and reports the unresolved findings instead of looping.
        assert_eq!(
            result.status,
            if issues {
                "complete_with_gaps"
            } else {
                "complete"
            },
            "{:?}",
            result.last_error
        );
        assert_eq!(result.document_review.attempts, 1);
        if issues {
            assert!(document_review::rejected_on_current_result(&result));
            assert!(
                result
                    .completion_gaps
                    .iter()
                    .any(|gap| gap.starts_with("문서 검토 지적"))
            );
        }
        let finals = result
            .history
            .bundles
            .iter()
            .flat_map(|b| &b.messages)
            .filter(|m| m["role"] == "assistant" && m.get("tool_calls").is_none())
            .count();
        assert_eq!(finals, 1);
        assert_eq!(deltas.len(), 1);
        // The final is the model's answer, or the runtime's report when the
        // budget ran out during closing; either way unresolved items are listed.
        assert_eq!(
            deltas[0].contains("확인하지 못한 항목"),
            issues,
            "{deltas:?}"
        );
        if !issues {
            assert_eq!(deltas[0], "Done");
        }
    }
}

#[tokio::test]
async fn required_document_before_any_write_is_not_forced_to_chat_answer() {
    let (_dir, mut s) = fixture();
    s.document_written = false;
    s.last_document_write = None;
    s.task_rounds = 6;
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        s,
        Arc::new(Reviewer {
            issues: false,
            // Neither a chat answer nor a drafting nudge: the model decides
            // when to write.
            phase: Some("investigate"),
        }),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    // No document was saved in this task, so closing mode cannot finish it.
    assert_eq!(result.status, "blocked");
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("closing_round_limit")
    );
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
fn broad_explicit_citations_preserve_comment_evidence() {
    let (dir, mut s) = fixture();
    let source = (1..=24)
        .map(|line| format!("// Contract clause {line}: preserve this documented behavior.\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("main.js"), &source).unwrap();
    std::fs::write(
        &s.project.output,
        "# Flow\nThe documented contract has 24 clauses. main.js:1-24\n",
    )
    .unwrap();
    let p: Value = serde_json::from_str(
        document_review::request(&mut s).unwrap()["messages"][1]["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let supplied = p["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["numbered_text"].as_str().unwrap())
        .collect::<String>();
    for line in 1..=24 {
        assert!(
            supplied.contains(&format!("{line}|// Contract clause {line}:")),
            "missing cited comment at line {line}"
        );
    }
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));
}

#[test]
fn wide_source_lines_are_split_across_review_pages_without_narrowing_citations() {
    let (dir, mut s) = fixture();
    let source = (1..=60)
        .map(|line| format!("const LINE_{line} = \"{}\";\n", "evidence ".repeat(900)))
        .collect::<String>();
    std::fs::write(dir.path().join("main.js"), &source).unwrap();
    std::fs::write(
        &s.project.output,
        "# Flow\nThere are sixty declarations. main.js:1-60\n",
    )
    .unwrap();
    let mut delivered = std::collections::BTreeSet::new();
    let mut pages = 0;
    let mut resized = false;
    loop {
        let request = document_review::request(&mut s).unwrap();
        assert!(
            mnemoarc::context::count(&request, &s.config.model)
                <= 24_000.min(ContextManager::input_budget(&s.config))
        );
        let p: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        for evidence in p["evidence"].as_array().unwrap() {
            for line in evidence["numbered_text"].as_str().unwrap().lines() {
                assert!(
                    delivered.insert(line.split_once('|').unwrap().0.parse::<usize>().unwrap())
                );
            }
        }
        pages += 1;
        support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
        if !s.document_review.pending {
            break;
        }
        if !resized {
            // A resumed run with a larger input budget repartitions chunks;
            // old offsets and partial verdicts cannot skip its new first page.
            s.config.context_tokens = 128_000;
            let restarted = document_review::request(&mut s).unwrap();
            let p: Value =
                serde_json::from_str(restarted["messages"][1]["content"].as_str().unwrap())
                    .unwrap();
            assert_eq!(p["first_evidence_chunk"], 0);
            assert_eq!(p["evidence_page"], 0);
            delivered.clear();
            resized = true;
        }
        assert!(pages < 60);
        assert!(!document_review::approved(&s));
    }
    assert!(pages > 1);
    assert!(resized);
    assert_eq!(delivered, (1..=60).collect());
    assert!(document_review::approved(&s));
}

#[test]
fn oversized_requirements_are_reported_before_document_paging() {
    let (_dir, mut s) = fixture();
    s.request_review_criteria.completion =
        vec!["review every branch and condition ".repeat(10_000)];
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
        support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
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
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();

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
    support::document_review::finish(&mut s, r#"{"issues":["Old revision issue"]}"#).unwrap();
    assert!(s.document_review.pending);
    std::fs::write(&s.project.output, format!("{doc}New line: main.js:1-6\n")).unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["document_line_start"], 1);
    assert_eq!(payload["evidence_page"], 0);
    assert_eq!(s.document_review.attempts, 0);
    while s.document_review.pending {
        support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
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
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
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
    let (_dir, mut s) = fixture();
    s.config.run_tokens = 200_000;
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, Arc::new(StallingRepair), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(
        result.status, "complete_with_gaps",
        "{:?}",
        result.last_error
    );
    assert_eq!(result.document_review.attempts, 1);
    assert_eq!(result.document_review.stalled_attempts, 0);
    assert!(
        result
            .completion_gaps
            .iter()
            .any(|gap| gap.starts_with("문서 검토 지적"))
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
                text: if self.always_bad || *calls <= 2 {
                    "invalid JSON".into()
                } else {
                    r#"{"issues":[]}"#.into()
                },
                usage: Some(mnemoarc::llm::Usage {
                    input: 5000,
                    output: 10,
                    cached: None,
                }),
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
        let (_dir, mut s) = fixture();
        s.config.run_tokens = 160_000;
        let model = Arc::new(MalformedReview {
            calls: std::sync::Mutex::new(0),
            always_bad,
        });
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = run_session(s, model.clone(), CancellationToken::new(), tx).await;
        drain.await.unwrap();
        // Three consecutive invalid verdicts abandon the review for this
        // unchanged document; two are recovered by the retry.
        assert_eq!(*model.calls.lock().unwrap(), 3);
        if always_bad {
            assert_eq!(result.status, "complete_with_gaps");
            assert!(
                result
                    .completion_gaps
                    .iter()
                    .any(|gap| gap.contains("검토를 마치지 못했습니다."))
            );
            assert!(document_review::unavailable_on_current(&result));
            assert_eq!(result.document_review.attempts, 0);
            assert!(!result.document_review.pending);
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
        // One manifest entry per cited file, however many chunks it has.
        let manifest = payload["evidence_manifest"].as_array().unwrap();
        assert_eq!(manifest.len(), 1, "{manifest:?}");
        let first = payload["first_evidence_chunk"].as_u64().unwrap();
        let file = &manifest[0];
        assert!(std::path::Path::new(file["path"].as_str().unwrap()).ends_with("main.js"));
        assert_eq!(file["first_line"], 1);
        assert_eq!(file["last_line"], lines);
        assert_eq!(file["chunks"], payload["evidence_chunks_total"]);
        assert!(file["chunks"].as_u64().unwrap() > 1);
        assert_eq!(file["reviewed_on_prior_pages"], first);
        assert_eq!(payload["evidence_manifest_omitted_files"], 0);
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
        support::document_review::finish(
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
fn the_writer_state_omits_review_bookkeeping_hashes() {
    // A review left an absolute path and a digest per cited file in the
    // writer's state, and edits kept them: 24.6K tokens per request for a
    // document citing 300 files.
    let (_dir, mut s) = fixture();
    s.document_review = Default::default();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":["Flow: name the loop type"]}"#).unwrap();
    assert!(!s.document_review.pending);
    assert!(document_review::rejected_on_current_result(&s));
    let state = ContextManager::state(&s).unwrap();
    let review = &state["document_review"];
    assert_eq!(review["issues"].as_array().unwrap().len(), 1, "{review}");
    for key in [
        "source_hashes",
        "target_hash",
        "target_requirements",
        "target_layout",
        "reviewed_requirements",
    ] {
        assert!(review.get(key).is_none(), "{key}: {review}");
    }
    assert!(!json!(s.document_review)["source_hashes"].is_null());
}

/// Requests one complete review of the saved document takes, with empty
/// verdicts.
fn review_requests(s: &mut Session) -> usize {
    s.document_review = Default::default();
    let mut requests = 0;
    loop {
        document_review::request(s).unwrap();
        requests += 1;
        support::document_review::finish(s, r#"{"issues":[]}"#).unwrap();
        if !s.document_review.pending {
            return requests;
        }
        assert!(requests < 500);
    }
}

#[test]
fn the_review_request_estimate_follows_document_length_and_cited_lines() {
    // The closing time reserve counts these requests: a fixed six requests
    // left the review of a long document to the deadline.
    let (dir, mut s) = fixture();
    for i in 0..20 {
        let source: String = (1..=200)
            .map(|line| format!("    let value_{line} = compute_state(&context, {line}, \"label-{i}-{line}\");\n"))
            .collect();
        std::fs::write(dir.path().join(format!("part_{i}.rs")), source).unwrap();
    }
    // A long document with short citations.
    let mut long = String::from("# Manual\n");
    for section in 0..60 {
        long.push_str(&format!("## Section {section}\n"));
        for line in 0..8 {
            let start = 1 + line * 20;
            long.push_str(&format!(
                "Step {line} computes the state. part_{}.rs:{start}-{}\n",
                section % 20,
                start + 9
            ));
        }
        long.push('\n');
    }
    // A short document whose few citations span whole files.
    let broad: String = std::iter::once("# Overview\n".to_owned())
        .chain((0..6).map(|i| format!("## Part {i}\nEvery value of part {i}. part_{i}.rs:1-200\n")))
        .collect();
    for doc in [long, broad] {
        std::fs::write(&s.project.output, &doc).unwrap();
        let estimate = document_review::estimated_requests(&s);
        let actual = review_requests(&mut s);
        assert!(
            actual / 2 <= estimate && estimate <= actual * 2,
            "{} lines: estimate {estimate}, actual {actual}",
            doc.lines().count()
        );
    }
    std::fs::remove_file(&s.project.output).unwrap();
    assert_eq!(document_review::estimated_requests(&s), 0);
}

#[test]
fn a_rereview_sends_evidence_only_where_the_last_verdict_may_no_longer_hold() {
    let (dir, mut s) = fixture();
    std::fs::write(
        dir.path().join("search.js"),
        "function search(query) {\n  return index.find(query);\n}\n",
    )
    .unwrap();
    let doc = "# Flow\n\n## Loop\n\nA for loop runs work five times. main.js:3-5\n\n## Search\n\nSearch looks the query up in the index. search.js:2\n";
    std::fs::write(&s.project.output, doc).unwrap();
    s.document_review = Default::default();
    let page = |s: &mut Session| -> (Vec<String>, Value) {
        let request = document_review::request(s).unwrap();
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        let mut paths: Vec<String> = payload["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chunk| {
                let path = std::path::Path::new(chunk["path"].as_str().unwrap());
                path.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        paths.dedup();
        (paths, payload)
    };
    let rereview = |payload: &Value| {
        payload["page_scope"]
            .as_str()
            .unwrap()
            .contains("Re-review")
    };
    // The first review checks every citation.
    let (paths, payload) = page(&mut s);
    assert_eq!(paths, ["main.js", "search.js"]);
    assert!(!rereview(&payload));
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));

    // A repair of one section sends that section's evidence only.
    let doc = doc.replace(
        "Search looks the query up in the index.",
        "Search returns the first index match.",
    );
    std::fs::write(&s.project.output, &doc).unwrap();
    let (paths, payload) = page(&mut s);
    assert_eq!(paths, ["search.js"]);
    assert_eq!(payload["changed_sections"], json!(["# Flow > ## Search"]));
    assert!(rereview(&payload));
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));

    // A changed source is sent again for the unchanged section citing it.
    let main = std::fs::read_to_string(dir.path().join("main.js")).unwrap();
    std::fs::write(dir.path().join("main.js"), format!("{main}// end\n")).unwrap();
    assert!(!document_review::approved(&s));
    let (paths, payload) = page(&mut s);
    assert_eq!(paths, ["main.js"]);
    assert_eq!(payload["changed_sections"], json!([]));
    let finding = json!({"previous_id":null,"kind":"factual",
        "document":{"start_line":5,"end_line":5,"quote":"A for loop runs work five times."},
        "requirement_id":null,
        "sources":[{"path":"main.js","start_line":3,"end_line":3,"quote":"for (let i = 0; i < 5; i++) {"}],
        "problem":"The loop bound is not stated as i < 5.","correction":"State the bound i < 5.","ui_labels":[]});
    support::document_review::finish(&mut s, &json!({"issues":[finding]}).to_string()).unwrap();
    assert_eq!(s.document_review.findings.len(), 1);

    // The passage of an open finding keeps its evidence although neither
    // its section nor its source changed.
    std::fs::write(
        &s.project.output,
        doc.replace("first index match", "first match"),
    )
    .unwrap();
    let (paths, payload) = page(&mut s);
    assert_eq!(paths, ["main.js", "search.js"]);
    assert_eq!(payload["previous_findings"][0]["id"], "F1");
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();

    // Lines a review could not judge make the next one complete again.
    s.document_review.unavailable_ranges = vec![(5, 5)];
    std::fs::write(&s.project.output, &doc).unwrap();
    let (paths, payload) = page(&mut s);
    assert_eq!(paths, ["main.js", "search.js"]);
    assert!(!rereview(&payload));
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
    support::document_review::finish(&mut s, r#"{"issues":["Old revision issue"]}"#).unwrap();
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
    s.config.run_tokens = 200_000;
    let mut s = run_repair_test(s, Arc::new(StallingRepair)).await;
    assert_eq!(s.status, "complete_with_gaps");
    assert!(
        s.completion_gaps
            .iter()
            .any(|gap| gap.starts_with("문서 검토 지적"))
    );
    assert_eq!(s.document_review.attempts, 1);
    assert_eq!(s.document_review.stalled_attempts, 0);
    assert!(!document_review::approved(&s));
    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop runs work. main.js:4-5","text":"History is normalized before a bounded for loop runs work. main.js:2-5"}),
    )
    .unwrap();
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
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
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
        support::document_review::finish(&mut s, &json!({"issues":issues}).to_string()).unwrap();
        assert_eq!(s.document_review.stalled_attempts, 0);
    }
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert_eq!(s.document_review.attempts, 4);
    assert!(document_review::approved(&s));
}

#[test]
fn additive_sections_keep_document_review_open_for_remaining_work() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 2;
    for n in 1..=3 {
        document_review::request(&mut s).unwrap();
        support::document_review::finish(&mut s, r#"{"issues":["Finish the requested overview"]}"#)
            .unwrap();
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
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));
    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(&mut s, "document_edit", json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop","text":"A for loop"})).unwrap();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":["Finish history normalization"]}"#)
        .unwrap();
    assert_eq!(s.document_review.stalled_attempts, 0);
    assert!(!document_review::stalled_on_current_result(&s));
}

struct ProgressiveDocumentRepair {
    calls: std::sync::Mutex<usize>,
    path: std::path::PathBuf,
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
            1 | 6..=8 => Completion {
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
            4 => Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: format!("correct-{calls}"),
                    name: "document_edit".into(),
                    arguments: json!({"action":"replace_text","expected_hash":tools::hash(&std::fs::read(&self.path)?),"old_text":"A while loop runs work. main.js:4-5","text":"History is normalized before a bounded for loop runs work. main.js:2-5"}).to_string(),
                }],
                ..Default::default()
            },
            5 => Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: "verify-flow".into(),
                    name: "document_audit".into(),
                    arguments: json!({}).to_string(),
                }],
                ..Default::default()
            },
            _ => anyhow::bail!("test_limit: progressive document repair did not finish at call {calls}"),
        };
        Ok(response)
    }
}

#[tokio::test]
async fn progressive_document_repairs_reach_final_approval() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 2;
    s.config.run_tokens = 5_000_000;
    // This test counts repair requests, not checkpoint cleanup; the default
    // budget sits close enough to the cleanup threshold that a slightly
    // larger tool schema would insert a cleanup request.
    s.config.context_tokens = 128_000;
    let client = Arc::new(ProgressiveDocumentRepair {
        calls: std::sync::Mutex::new(0),
        path: s.project.output.clone(),
    });
    let result = run_repair_test(s, client.clone()).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    // Edits between final answers are not reviewed on their own: the first
    // final is rejected and the second, after the correction, approved.
    assert_eq!(result.document_review.attempts, 2);
    assert!(document_review::approved(&result));
    // The final answer that started the approving review is resumed; the
    // model is not asked to answer a seventh time.
    assert_eq!(*client.calls.lock().unwrap(), 6);
}

#[test]
fn rejected_review_cache_and_approval_follow_requirements_and_source_versions() {
    let (dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":["Correct the loop"]}"#).unwrap();
    assert!(document_review::rejected_on_current_result(&s));
    // Agent-authored working checks do not change what the user requested.
    s.task
        .completion
        .push("Verify investigation bookkeeping".into());
    assert!(document_review::rejected_on_current_result(&s));
    s.request_review_criteria
        .completion
        .push("Also document cancellation".into());
    assert!(!document_review::rejected_on_current_result(&s));
    document_review::request(&mut s).unwrap();
    s.request_review_criteria
        .constraints
        .push("Use Korean".into());
    assert!(
        support::document_review::finish(&mut s, r#"{"issues":[]}"#)
            .unwrap_err()
            .to_string()
            .starts_with("document_review_stale")
    );
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));
    s.request_review_criteria
        .deliverables
        .push("API appendix".into());
    assert!(!document_review::approved(&s));
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":["Add the appendix"]}"#).unwrap();
    std::fs::write(dir.path().join("main.js"), "function changed() {}\n").unwrap();
    assert!(!document_review::rejected_on_current_result(&s));
}

#[test]
fn current_verdict_does_not_promote_findings_from_an_earlier_document() {
    let (dir, mut s) = fixture();
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Unreviewed
    );
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":["Correct the loop"]}"#).unwrap();
    assert!(matches!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Rejected(issues) if issues == ["Correct the loop"]
    ));

    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":expected,"old_text":"A while loop","text":"A for loop"}),
    )
    .unwrap();
    assert_eq!(s.document_review.issues, ["Correct the loop"]);
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Unreviewed
    );

    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Approved
    );
    std::fs::write(dir.path().join("main.js"), "function changed() {}\n").unwrap();
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Unreviewed
    );
}

#[test]
fn unavailable_verdict_stays_bound_to_the_failed_request_snapshot() {
    let (dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    let original = std::fs::read(dir.path().join("out.md")).unwrap();
    s.last_error = Some("document_review_invalid: rejected response".into());
    std::fs::write(
        dir.path().join("out.md"),
        "# Changed\nDifferent document.\n",
    )
    .unwrap();
    s.task.constraints.push("New requirement".into());
    document_review::mark_unavailable(&mut s);
    // Diagnostics identify the document supplied to the failed request,
    // even when the output file has since changed externally.
    let failure = s.document_review.unavailable_failure.as_ref().unwrap();
    assert_eq!(failure.document_hash, Some(tools::hash(&original)));
    assert_eq!(failure.error.as_deref(), s.last_error.as_deref());
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Unreviewed
    );
    std::fs::write(dir.path().join("out.md"), original).unwrap();
    s.task.constraints.pop();
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Unavailable
    );
}

#[test]
fn rewording_review_findings_does_not_count_as_progress() {
    let (_dir, mut s) = fixture();
    for issue in [
        "Correct the loop",
        "The loop type is wrong",
        "Fix the loop description",
    ] {
        document_review::request(&mut s).unwrap();
        support::document_review::finish(&mut s, &json!({"issues":[issue]}).to_string()).unwrap();
    }
    assert_eq!(s.document_review.stalled_attempts, 2);
}

#[test]
fn long_document_review_advances_past_thirty_two_pages() {
    let (_dir, mut s) = fixture();
    let document = format!(
        "# Flow\nBounded loop. main.js:1-6\n{}",
        "More supported detail.\n".repeat(3300)
    );
    std::fs::write(&s.project.output, document).unwrap();
    let mut next_line = 1;
    let mut pages = 0;
    loop {
        let request = document_review::request(&mut s).unwrap();
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["document_line_start"], next_line);
        next_line = payload["document_line_end"].as_u64().unwrap() + 1;
        assert!(mnemoarc::context::count(&request, &s.config.model) <= 24_000);
        support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
        pages += 1;
        assert!(pages < 40, "page offsets stopped advancing");
        if !s.document_review.pending {
            break;
        }
    }
    assert!(pages > 32);
    assert_eq!(next_line, 3303);
    assert_eq!(s.document_review.attempts, 1);
    assert!(document_review::approved(&s));
}

#[test]
fn structural_preflight_remains_available_after_review_limit() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 1;
    for _ in 0..12 {
        let result = tools::execute(&mut s, "document_audit", json!({})).unwrap();
        assert_eq!(result["structural_ok"], true, "{result}");
    }
    assert!(!document_review::approved(&s));
}

#[test]
fn review_retry_preserves_document_range_and_evidence_continuation() {
    let (dir, mut s) = fixture();
    let source: String = (1..=1800)
        .map(|i| format!("const value_{i} = normalize(history, {i}, 'input-{i}');\n"))
        .collect();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    let mut document = "# Flow\nAll processing: main.js:1-1800\n".to_owned();
    for i in 0..100 {
        document.push_str(&format!(
            "Detail {i}: {}\n",
            "Concrete supported behavior. ".repeat(80)
        ));
    }
    std::fs::write(&s.project.output, document).unwrap();
    s.document_review = Default::default();
    let payload = |request: Value| -> Value {
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap()
    };
    let first = payload(document_review::request(&mut s).unwrap());
    assert_eq!(first["more_document_pages"], true);
    assert_eq!(first["more_evidence_pages"], true);
    s.last_error = Some(format!(
        "document_review_invalid: {}",
        "Malformed review response. ".repeat(100)
    ));
    let retry = payload(document_review::request(&mut s).unwrap());
    assert_eq!(retry["document_line_end"], first["document_line_end"]);
    assert_eq!(retry["document"], first["document"]);
    assert_eq!(retry["evidence"], first["evidence"]);
    assert!(
        retry["previous_response_error"]
            .as_str()
            .unwrap()
            .contains("document_review_invalid")
    );
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    let next = payload(document_review::request(&mut s).unwrap());
    assert_eq!(next["document"], first["document"]);
    assert_eq!(
        next["first_evidence_chunk"],
        first["evidence"].as_array().unwrap().len()
    );
    assert!(!document_review::approved(&s));
}

#[tokio::test]
async fn pending_document_review_setup_error_returns_to_repair_and_finishes() {
    struct RepairMissingCitation {
        path: std::path::PathBuf,
        calls: std::sync::Mutex<usize>,
        reviews: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl LlmClient for RepairMissingCitation {
        async fn complete(
            &self,
            request: Value,
            _: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
            let payload: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                    .unwrap_or(Value::Null);
            if payload["source_document_review"] == true {
                assert!(!std::fs::read_to_string(&self.path)?.contains("missing.js"));
                if self
                    .reviews
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    == 0
                {
                    return Ok(Completion {
                        calls: (0..33)
                            .map(|i| mnemoarc::llm::ToolCall {
                                id: format!("forbidden-document-review-tool-{i}"),
                                name: "document_edit".into(),
                                arguments: json!({"action":"append","text":"must not execute"})
                                    .to_string(),
                            })
                            .collect(),
                        ..Default::default()
                    });
                }
                assert!(
                    payload["previous_response_error"]
                        .as_str()
                        .unwrap()
                        .starts_with("document_review_incomplete:")
                );
                return Ok(Completion {
                    text: r#"{"issues":[]}"#.into(),
                    ..Default::default()
                });
            }
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            assert!(
                *calls <= 4,
                "document setup failure did not return to final acceptance"
            );
            let action = match *calls {
                1 => {
                    let content =
                        request["messages"].as_array().unwrap().last().unwrap()["content"]
                            .as_str()
                            .unwrap();
                    let state: Value = serde_json::from_str(content.split_once('\n').unwrap().1)?;
                    assert!(!state["document_review"]["pending"].as_bool().unwrap());
                    assert!(
                        state["run_guidance"]["recovery_reason"]
                            .as_str()
                            .unwrap()
                            .contains("file_not_found")
                    );
                    (
                        "document_edit",
                        json!({"action":"write","expected_hash":tools::hash(&std::fs::read(&self.path)?),
                        "text":"# Flow\nHistory is normalized before a bounded for loop runs work. main.js:2-5\n"}),
                    )
                }
                2 => ("document_audit", json!({})),
                _ => {
                    return Ok(Completion {
                        text: "Saved out.md".into(),
                        ..Default::default()
                    });
                }
            };
            Ok(Completion {
                calls: vec![mnemoarc::llm::ToolCall {
                    id: format!("repair-{calls}"),
                    name: action.0.into(),
                    arguments: action.1.to_string(),
                }],
                ..Default::default()
            })
        }
    }
    let (_dir, mut s) = fixture();
    let hash = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,
        "text":"\nMissing evidence: missing.js:1\n"}),
    )
    .unwrap();
    s.document_review.pending = true;
    let client = Arc::new(RepairMissingCitation {
        path: s.project.output.clone(),
        calls: std::sync::Mutex::new(0),
        reviews: std::sync::atomic::AtomicUsize::new(0),
    });
    let s = run_repair_test(s, client).await;
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert!(document_review::approved(&s));
    assert!(
        !s.ledger
            .keys()
            .any(|id| id.starts_with("forbidden-document-review-tool-"))
    );
}

struct SlowCorrection {
    calls: std::sync::Mutex<usize>,
    path: std::path::PathBuf,
    premature_finals: bool,
}

#[async_trait]
impl LlmClient for SlowCorrection {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if request["messages"][1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("\"source_document_review\":true"))
        {
            let issues = if std::fs::read_to_string(&self.path)?.contains("A while loop") {
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
        assert!(*calls < 22, "correction did not finish");
        if *calls == 1 || *calls > 16 || (self.premature_finals && *calls < 16) {
            if self.premature_finals && *calls > 1 && *calls < 16 {
                assert_eq!(request["tool_choice"], "required");
            }
            return Ok(Completion {
                text: "Saved and verified out.md".into(),
                ..Default::default()
            });
        }
        let text = if *calls == 16 {
            "# Flow\nHistory is normalized before a bounded for loop runs work. main.js:2-5\n"
                .into()
        } else {
            format!("# Flow\nA while loop runs work (draft {calls}). main.js:4-5\n")
        };
        let mut tool_calls = vec![mnemoarc::llm::ToolCall {
            id: format!("edit-{calls}"), name: "document_edit".into(),
            arguments: json!({"action":"write","expected_hash":tools::hash(&std::fs::read(&self.path)?),"text":text}).to_string(),
        }];
        // The verification sibling executes with the final correction.
        if *calls == 16 {
            tool_calls.push(mnemoarc::llm::ToolCall {
                id: "audit-final-correction".into(),
                name: "document_audit".into(),
                arguments: json!({}).to_string(),
            });
        }
        Ok(Completion {
            calls: tool_calls,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn source_document_can_finish_after_many_repair_edits_or_rejected_finals() {
    for premature_finals in [false, true] {
        let (_dir, mut s) = fixture();
        s.config.review_limit = 1;
        s.config.context_tokens = 128_000;
        s.config.run_tokens = 2_000_000;
        let client = Arc::new(SlowCorrection {
            calls: std::sync::Mutex::new(0),
            path: s.project.output.clone(),
            premature_finals,
        });
        let s = run_repair_test(s, client).await;
        if premature_finals {
            // Final answers that never edit the rejected document are not
            // repair: after three unchanged rejections the run closes and
            // reports the open finding instead of waiting for a late fix.
            assert_eq!(s.status, "complete_with_gaps", "{:?}", s.last_error);
            assert_eq!(
                s.progress_recovery.closing.as_ref().unwrap().reason,
                "review_unrepaired"
            );
            assert!(
                s.completion_gaps
                    .iter()
                    .any(|gap| gap.contains("correct the loop and history statement"))
            );
            assert_eq!(s.document_review.attempts, 1);
            continue;
        }
        assert_eq!(s.status, "complete", "{:?}", s.last_error);
        assert!(document_review::approved(&s));
        // Fourteen draft edits run without a review of their own; only the
        // first and the last final answer are reviewed.
        assert_eq!(s.document_review.attempts, 2);
    }
}

#[test]
fn a_repeated_finding_about_rewritten_text_is_invalid_not_an_approval() {
    let (_dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":["Flow: correct the loop"]}"#).unwrap();
    let expected = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":expected,
        "old_text":"A while loop runs work.","text":"A bounded for loop runs work."}),
    )
    .unwrap();
    document_review::request(&mut s).unwrap();
    let stale = json!({"issues":[{"previous_id":null,"kind":"scope","document":{"start_line":2,"end_line":2,"quote":"A while loop runs work."},
        "requirement_id":"R0","sources":[],"problem":"Old loop finding","correction":"Fix the old passage","ui_labels":[]}]});
    assert!(
        document_review::finish(&mut s, &stale.to_string())
            .unwrap_err()
            .to_string()
            .starts_with("document_review_invalid:")
    );
    assert!(!document_review::approved(&s));
    assert_eq!(s.document_review.attempts, 1);
    document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));
}

#[test]
fn a_page_cannot_report_a_later_section_as_missing() {
    let (_dir, mut s) = fixture();
    let mut doc = String::from("# Intro\n");
    doc.push_str(&"Intro context. main.js:1-6\n".repeat(59));
    doc.push_str("## Diagram\n```mermaid\n");
    doc.push_str(&"A --> B\n".repeat(60));
    doc.push_str("```\n## 4-3. Binding editor basics\n");
    doc.push_str(&"Conclusion. main.js:1-6\n".repeat(100));
    std::fs::write(&s.project.output, doc).unwrap();
    s.document_review = Default::default();
    let first = document_review::request(&mut s).unwrap();
    let first: Value =
        serde_json::from_str(first["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(first["document_line_end"], 60);
    // Every page sees the whole outline.
    assert!(
        first["document_outline"]
            .to_string()
            .contains("## 4-3. Binding editor basics")
    );
    let outside = json!({"issues":[{"previous_id":null,"kind":"scope","document":{"start_line":124,"end_line":124,"quote":"## 4-3. Binding editor basics"},
        "requirement_id":"R0","sources":[],"problem":"Section is missing","correction":"Add the section","ui_labels":[]}]});
    assert!(
        document_review::finish(&mut s, &outside.to_string())
            .unwrap_err()
            .to_string()
            .contains("lines 124-124 are not on this page, which supplies only lines 1-60.")
    );
    assert!(!document_review::approved(&s));
    assert_eq!(s.document_review.attempts, 0);
    loop {
        document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
        if !s.document_review.pending {
            break;
        }
        document_review::request(&mut s).unwrap();
    }
    assert!(s.document_review.issues.is_empty());
    assert!(document_review::approved(&s));
}

#[test]
fn reviewer_judges_detail_for_the_reader_the_request_names() {
    // The reader and purpose come from the request, not project settings.
    let (_dir, mut s) = fixture();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(payload.get("audience").is_none(), "{payload}");
    assert!(payload.get("purpose").is_none(), "{payload}");
    assert!(
        !payload["requirement_catalog"]
            .as_object()
            .unwrap()
            .contains_key("audience"),
        "{payload}"
    );
    let instruction = request["messages"][0]["content"].as_str().unwrap();
    assert!(instruction.contains("The reader and purpose are whatever the user request states"));
    assert!(instruction.contains("non-developer audience"));
}

#[test]
fn user_manual_citations_retain_source_evidence_and_validation() {
    let (_dir, mut s) = fixture();
    let source_path = s.project.root.join("main.js").canonicalize().unwrap();
    let request = document_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(
        payload["document"]
            .as_str()
            .unwrap()
            .contains("main.js:4-5")
    );
    assert!(payload["evidence"].as_array().unwrap().iter().any(|chunk| {
        s.project
            .root
            .join(chunk["path"].as_str().unwrap())
            .canonicalize()
            .unwrap()
            == source_path
            && chunk["numbered_text"]
                .as_str()
                .unwrap()
                .contains("for (let i = 0; i < 5; i++)")
    }));
    assert_eq!(
        tools::audit_document(&mut s).unwrap()["citations_checked"],
        1
    );

    std::fs::write(&s.project.output, "# Flow\nWork runs. main.js:999\n").unwrap();
    let audit = tools::audit_document(&mut s).unwrap();
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "citation_range" })
    );

    std::fs::write(&s.project.output, "# Flow\nWork runs.\n").unwrap();
    let audit = tools::audit_document(&mut s).unwrap();
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "no_machine_readable_citations" })
    );
    assert!(
        document_review::request(&mut s)
            .unwrap_err()
            .to_string()
            .contains("no source citations")
    );
}

#[test]
fn longer_repairs_do_not_reset_a_stalled_review_without_a_length_finding() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 2;
    let issues =
        r#"{"issues":["Flow: name the loop bound","Flow: describe history normalization"]}"#;
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, issues).unwrap();
    for n in 1..=2 {
        // Each repair adds lines to the same section but resolves nothing.
        let expected = s.last_document_write.as_ref().unwrap().1.clone();
        tools::execute(&mut s, "document_edit", json!({"action":"append","expected_hash":expected,"text":format!("More detail {n}.\nAnd more {n}.\n")})).unwrap();
        document_review::request(&mut s).unwrap();
        support::document_review::finish(&mut s, issues).unwrap();
        assert_eq!(s.document_review.stalled_attempts, n);
    }
    assert!(document_review::stalled_on_current_result(&s));
}

#[test]
fn longer_repairs_count_as_progress_after_a_length_finding() {
    let (_dir, mut s) = fixture();
    s.config.review_limit = 1;
    s.answer_review_question = "history normalization 문서를 40줄 내외로 작성해줘.".into();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(s.document_review.issues[0].starts_with("문서 길이:"));
    let expected = s.last_document_write.as_ref().unwrap().1.clone();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":expected,"text":"More detail.\nAnd more.\n"}),
    )
    .unwrap();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert_eq!(s.document_review.stalled_attempts, 0);
}

/// The first review response is cut by the output limit mid-JSON, as when a
/// reasoning model spends the output allowance on reasoning.
struct TruncatedFirstReview {
    reviews: std::sync::Mutex<Vec<(usize, Value)>>,
}
#[async_trait]
impl LlmClient for TruncatedFirstReview {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        _: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let payload: Value = request["messages"][1]["content"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        if payload["source_document_review"] == true {
            let mut reviews = self.reviews.lock().unwrap();
            reviews.push((
                config.output_tokens,
                payload["previous_response_error"].clone(),
            ));
            if reviews.len() == 1 {
                return Ok(Completion {
                    text: r#"{"issues":[{"previous_id":null,"kind":"factual","document":{"start_line":2"#
                        .into(),
                    length_limited: true,
                    ..Default::default()
                });
            }
            return Ok(Completion {
                text: r#"{"issues":[]}"#.into(),
                ..Default::default()
            });
        }
        tx.send("Done".into()).await.ok();
        Ok(Completion {
            text: "Done".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn truncated_review_retry_names_the_output_limit_not_tools() {
    let (_dir, mut s) = fixture();
    s.config.output_tokens = 8192;
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,"text":"# Flow\nHistory is normalized, then a for loop runs work five times. main.js:2-5\n"}),
    )
    .unwrap();
    tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
    let client = Arc::new(TruncatedFirstReview {
        reviews: Default::default(),
    });
    let (tx, mut rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(s, client.clone(), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    let reviews = client.reviews.lock().unwrap();
    assert!(reviews.len() >= 2, "{reviews:?} {:?}", result.last_error);
    // Both requests get the full allowance; the retry, on a halved page, is
    // told why the JSON broke off.
    assert_eq!(reviews[0].0, 8192);
    assert_eq!(reviews[0].1, Value::Null);
    assert_eq!(reviews[1].0, 8192);
    let error = reviews[1].1.as_str().unwrap();
    assert!(error.starts_with("document_review_incomplete:"), "{error}");
    assert!(error.contains("output token limit"), "{error}");
    assert!(!error.contains("without tools"), "{error}");
}

/// Reviews a two-page document; the second page always answers invalid JSON.
struct FailingSecondPage {
    first_page_issue: bool,
}
#[async_trait]
impl LlmClient for FailingSecondPage {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        tx: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let payload: Value = request["messages"][1]["content"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        let text = if payload["source_document_review"] == true {
            if payload["document_line_start"] != 1 {
                "{\"issues\":[{\"problem\":\"unterminated"
            } else if self.first_page_issue {
                r#"{"issues":["Flow: incorrect loop type"]}"#
            } else {
                r#"{"issues":[]}"#
            }
        } else {
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
async fn a_persistently_invalid_page_is_skipped_without_losing_other_findings() {
    for first_page_issue in [true, false] {
        let (_dir, mut s) = fixture();
        s.config.run_tokens = 400_000;
        let doc = (1..=220)
            .map(|i| format!("Line {i}: main.js:1-6\n"))
            .collect::<String>();
        let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"write","expected_hash":hash,"text":format!("# Flow\n{doc}")}),
        )
        .unwrap();
        tools::execute(&mut s, "file_read", json!({"path":"main.js"})).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        let drain = tokio::spawn(async move {
            let mut notices = Vec::new();
            while let Some(e) = rx.recv().await {
                if let AgentEvent::Notice { text, .. } = e {
                    notices.push(text);
                }
            }
            notices
        });
        let result = run_session(
            s,
            Arc::new(FailingSecondPage { first_page_issue }),
            CancellationToken::new(),
            tx,
        )
        .await;
        let notices = drain.await.unwrap();
        assert_eq!(
            result.status, "complete_with_gaps",
            "{:?}",
            result.last_error
        );
        assert!(
            notices
                .iter()
                .any(|n| n.contains("이 부분의 검토를 건너뛰고")),
            "{notices:?}"
        );
        assert!(!document_review::approved(&result));
        if first_page_issue {
            // The first page's finding survived the failing page and was
            // validated into a rejected verdict instead of being discarded.
            assert!(document_review::rejected_on_current_result(&result));
            assert!(
                result
                    .completion_gaps
                    .iter()
                    .any(|gap| gap.starts_with("문서 검토 지적") && gap.contains("loop type")),
                "{:?}",
                result.completion_gaps
            );
        } else {
            assert!(document_review::unavailable_on_current(&result));
            let ranges = document_review::unavailable_ranges(&result);
            // Pages 101-200 and 201-221 both failed and merge into one range.
            assert_eq!(ranges, [(101, 221)]);
            assert!(
                result
                    .completion_gaps
                    .iter()
                    .any(|gap| gap == "문서 검토 — 101–221줄은 검토를 마치지 못했습니다."),
                "{:?}",
                result.completion_gaps
            );
        }
    }
}

#[test]
fn a_skipped_page_blocks_approval_even_when_other_pages_are_clean() {
    let (_dir, mut s) = fixture();
    let doc = (1..=220)
        .map(|i| format!("Line {i}: main.js:1-6\n"))
        .collect::<String>();
    std::fs::write(&s.project.output, &doc).unwrap();
    s.document_review = Default::default();
    document_review::request(&mut s).unwrap();
    s.document_review.pending = true;
    s.last_error = Some("document_review_invalid: bad label".into());
    // The first page fails; later pages review cleanly.
    assert_eq!(
        document_review::skip_failing_page(&mut s),
        document_review::PageSkip::Continued
    );
    let first_end = s.document_review.skipped_ranges[0].1;
    assert_eq!(s.document_review.skipped_ranges, [(1, first_end)]);
    // The skip ends the page's retries, so its last rejection is kept here.
    assert_eq!(
        s.document_review.skip_log,
        [
            json!({"lines":[1, first_end],"evidence_page":0,"error":"document_review_invalid: bad label"})
        ]
    );
    assert!(document_review::guidance(&s).get("skip_log").is_none());
    while s.document_review.pending {
        document_review::request(&mut s).unwrap();
        support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    }
    assert!(!document_review::approved(&s));
    assert!(document_review::unavailable_on_current(&s));
    assert_eq!(document_review::unavailable_ranges(&s), [(1, first_end)]);
    // Validation that never built a request has no candidate batch to skip.
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"More: main.js:1-6\n"}),
    )
    .unwrap();
    document_review::request(&mut s).unwrap();
    s.document_review.pending = true;
    s.document_review.validating = true;
    assert_eq!(
        document_review::skip_failing_page(&mut s),
        document_review::PageSkip::NotApplicable
    );
}

#[test]
fn a_bare_issue_array_is_read_as_the_issue_list() {
    let (_dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    // Models without structured output sent the list itself; serde read the
    // array as the verdict's fields ("invalid type: map, expected a
    // sequence") and the page was skipped on its last try.
    support::document_review::finish(
        &mut s,
        "```json\n[\"Flow: for loop, not while; history omitted\"]\n```",
    )
    .unwrap();
    assert!(!document_review::approved(&s));
    assert!(
        s.document_review
            .issues
            .iter()
            .any(|issue| issue.contains("for loop, not while")),
        "{:?}",
        s.document_review.issues
    );
    document_review::request(&mut s).unwrap();
    let error = support::document_review::finish(&mut s, "\"no issues\"")
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "document_review_invalid: expected one JSON object {\"issues\":[...]}"
    );
    support::document_review::finish(&mut s, "[]").unwrap();
    assert!(document_review::approved(&s));
}

#[test]
fn a_reply_that_opens_its_wrapper_twice_is_read_as_the_inner_answer() {
    // Live runs 2026-10-08: a reviewer answered {"issues":{ "issues": [] }
    // and {"issues":[{"issues":[...]}; in closing mode the format error
    // dropped the whole document review.
    for reply in [
        r#"{"issues":{ "issues": [] }"#,
        r#"{"issues":[{"issues":[]}"#,
        r#"{"issues":[{"issues":[]}]}"#,
        r#"{"issues":{"issues":[]}}"#,
    ] {
        let (_dir, mut s) = fixture();
        document_review::request(&mut s).unwrap();
        support::document_review::finish(&mut s, reply).unwrap();
        assert!(document_review::approved(&s), "{reply}");
    }
    let (_dir, mut s) = fixture();
    document_review::request(&mut s).unwrap();
    support::document_review::finish(
        &mut s,
        r##"{"issues":[{"issues":[{"previous_id":null,"kind":"scope","document":{"start_line":1,"end_line":1,"quote":"# Flow"},"requirement_id":"R0","sources":[],"problem":"Flow: for loop, not while; history omitted","correction":"Describe the for loop and history normalization.","ui_labels":[]}]}"##,
    )
    .unwrap();
    assert!(!document_review::approved(&s));
    assert!(
        s.document_review
            .issues
            .iter()
            .any(|issue| issue.contains("for loop, not while")),
        "{:?}",
        s.document_review.issues
    );
    // Anything else that does not parse is still rejected.
    document_review::request(&mut s).unwrap();
    let error = support::document_review::finish(&mut s, r#"{"issues":[{"issues":["#)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("document_review_invalid: EOF"), "{error}");
}

/// Answers once; counts model requests apart from the two reviews.
struct OneFinalAnswer {
    model_calls: std::sync::atomic::AtomicUsize,
    issues: bool,
}

#[async_trait]
impl LlmClient for OneFinalAnswer {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if request["messages"][1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("\"source_document_review\":true"))
        {
            let issues: Vec<&str> = if self.issues {
                vec!["Flow: the loop is a for loop, not a while loop"]
            } else {
                vec![]
            };
            return Ok(Completion {
                text: json!({"issues":issues}).to_string(),
                ..Default::default()
            });
        }
        let call = self
            .model_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Completion {
            text: format!("Saved out.md (answer {call})."),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn an_approved_review_resumes_the_final_answer_that_started_it() {
    let (_dir, s) = fixture();
    let client = Arc::new(OneFinalAnswer {
        model_calls: Default::default(),
        issues: false,
    });
    let result = run_repair_test(s, client.clone()).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(document_review::approved(&result));
    // One model answer: the review approves it, and that same answer is
    // the one published.
    assert_eq!(
        client.model_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let published = result
        .history
        .bundles
        .iter()
        .flat_map(|bundle| bundle.messages.iter())
        .rev()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert!(
        published["content"].as_str().unwrap().contains("answer 0"),
        "{published}"
    );
}

#[tokio::test]
async fn a_rejecting_review_still_returns_to_the_model() {
    let (_dir, mut s) = fixture();
    s.config.run_tokens = 300_000;
    let client = Arc::new(OneFinalAnswer {
        model_calls: Default::default(),
        issues: true,
    });
    let result = run_repair_test(s, client.clone()).await;
    // The unchanged rejected document is never approved, so every later
    // answer comes from the model, not from the held one.
    assert!(!document_review::approved(&result));
    assert!(client.model_calls.load(std::sync::atomic::Ordering::SeqCst) > 1);
}

/// Answers once and returns no issue for any review page. With
/// `fail_first_page`, every response for the first document page is
/// invalid, so that page is skipped and the review ends unavailable.
struct PagedReview {
    model_calls: std::sync::atomic::AtomicUsize,
    review_calls: std::sync::atomic::AtomicUsize,
    fail_first_page: bool,
}

#[async_trait]
impl LlmClient for PagedReview {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let payload: Value = request["messages"][1]["content"]
            .as_str()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        if payload["source_document_review"] == true {
            self.review_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = if self.fail_first_page && payload["document_line_start"] == 1 {
                "The page looks fine.".to_owned()
            } else {
                json!({"issues":[]}).to_string()
            };
            return Ok(Completion {
                text,
                ..Default::default()
            });
        }
        let call = self
            .model_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Completion {
            text: format!("Saved out.md (answer {call})."),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_review_of_several_requests_resumes_the_final_answer_that_started_it() {
    // A live review of two evidence pages and a finding validation lost the
    // held answer at its first page, and the model was asked for its final
    // answer again after the approval (3.9% of the run's input).
    for fail_first_page in [false, true] {
        let (_dir, mut s) = fixture();
        // More lines than one review page holds.
        let doc = std::iter::once("# Flow\n".to_owned())
            .chain((1..=150).map(|i| format!("Step {i} runs work in a for loop. main.js:3-5\n")))
            .collect::<String>();
        let expected = s.last_document_write.as_ref().unwrap().1.clone();
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"write","expected_hash":expected,"text":doc}),
        )
        .unwrap();
        let client = Arc::new(PagedReview {
            model_calls: Default::default(),
            review_calls: Default::default(),
            fail_first_page,
        });
        let result = run_repair_test(s, client.clone()).await;
        let review_calls = client
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(review_calls >= 2, "{review_calls} review requests");
        if fail_first_page {
            // Every page was answered, but the skipped one blocks approval:
            // the review ends unreviewed, and so does the held answer.
            assert!(document_review::unavailable_on_current(&result));
        } else {
            assert!(document_review::approved(&result));
        }
        assert_eq!(
            client.model_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fail_first_page={fail_first_page}: {:?} {:?}",
            result.last_error,
            result.completion_gaps
        );
        let published = result
            .history
            .bundles
            .iter()
            .flat_map(|bundle| bundle.messages.iter())
            .rev()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert!(
            published["content"].as_str().unwrap().contains("answer 0"),
            "{published}"
        );
    }
}

#[test]
fn a_halved_review_caps_every_later_page_at_half_the_unanswered_evidence() {
    // Live run 2026-10-08: the halved count was taken from what each later
    // page had left, so pages far below the input ceiling took half, then a
    // quarter, then one chunk of the remaining evidence: 13 requests for
    // 137 lines, and the review ran out of time. The cap is half of the
    // unanswered page's evidence and holds for every later page.
    let (dir, mut s) = fixture();
    let files = 8;
    for file in 0..files {
        let source = (1..=20)
            .map(|i| format!("export const value_{file}_{i} = {i};\n"))
            .collect::<String>();
        std::fs::write(dir.path().join(format!("mod{file}.js")), source).unwrap();
    }
    let doc = (0..files)
        .flat_map(|file| {
            (0..5).map(move |i| {
                format!(
                    "Claim {file}-{i} shows the `value_{file}_{i}` setting. mod{file}.js:1-20\n"
                )
            })
        })
        .collect::<String>();
    std::fs::write(&s.project.output, &doc).unwrap();
    let page = |s: &mut Session| {
        let request = document_review::request(s).unwrap();
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        payload
    };
    let full = page(&mut s);
    assert_eq!(full["evidence"].as_array().unwrap().len(), files);
    assert!(document_review::shrink_page(&mut s));
    let first = page(&mut s);
    let first_chunks = first["evidence"].as_array().unwrap().len();
    assert!((3..=5).contains(&first_chunks), "{first_chunks}");
    assert_eq!(first["more_evidence_pages"], true);
    document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    // The next evidence page of the same range takes as much again, not
    // half of what is left, and judges only its evidence.
    let second = page(&mut s);
    let second_chunks = second["evidence"].as_array().unwrap().len();
    assert!(
        second_chunks + 1 >= first_chunks,
        "{second_chunks} after {first_chunks}"
    );
    assert!(first_chunks + second_chunks >= files - 1);
    assert!(
        second["page_scope"]
            .as_str()
            .unwrap()
            .contains("continues the same document range with further evidence"),
        "{second}"
    );
    assert!(
        !first["page_scope"]
            .as_str()
            .unwrap()
            .contains("continues the same document range"),
        "{first}"
    );
    // Another model starts from full pages again.
    s.config.model = "gpt-4o-mini".into();
    let other = page(&mut s);
    assert_eq!(other["evidence"].as_array().unwrap().len(), files);
    assert!(s.document_review.evidence_cap.is_none());
}

#[test]
fn a_shrunk_review_page_covers_fewer_lines_and_evidence_chunks() {
    // A reasoning model ran out of its full output allowance on a page; an
    // identical retry tends to fail the same way, so the page is halved.
    let (dir, mut s) = fixture();
    let source = (1..=400)
        .map(|i| format!("const LINE_{i} = {i};\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    let doc = (1..=150)
        .map(|i| format!("Claim {i}. main.js:{}-{}\n", i * 2, i * 2 + 1))
        .collect::<String>();
    std::fs::write(&s.project.output, &doc).unwrap();
    let page = |s: &mut Session| {
        let request = document_review::request(s).unwrap();
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        (
            payload["document_line_end"].as_u64().unwrap(),
            payload["evidence"].as_array().unwrap().len(),
        )
    };
    let (full_end, full_evidence) = page(&mut s);
    assert!(full_end > 50, "{full_end}");
    assert!(document_review::shrink_page(&mut s));
    let (half_end, half_evidence) = page(&mut s);
    assert!(
        half_end <= 50 && half_end < full_end,
        "{half_end} vs {full_end}"
    );
    assert!(half_evidence >= 1 && half_evidence <= full_evidence);
    assert!(document_review::shrink_page(&mut s));
    let (quarter_end, quarter_evidence) = page(&mut s);
    assert!(quarter_end <= 25, "{quarter_end}");
    assert!(quarter_evidence >= 1 && quarter_evidence <= half_evidence);
    assert!(
        !document_review::shrink_page(&mut s),
        "at most two halvings"
    );
    assert_eq!(s.document_review.page_shrink, 2);
    // Another model has its own capacity: it starts from full pages again.
    s.config.model = "gpt-4o-mini".into();
    let (other_end, _) = page(&mut s);
    assert_eq!(s.document_review.page_shrink, 0);
    assert_eq!(other_end, full_end);
}

#[test]
fn a_long_outline_on_a_review_page_keeps_the_page_and_the_top_levels() {
    // A page's outline is bounded to the headings near the page and the top
    // levels, with heading_count and document_outline_omitted saying what
    // is left out; it used to list every heading, which shrank the room for
    // the document text and evidence as documents grew.
    let (_dir, mut s) = fixture();
    let mut doc = String::from("# Intro\n");
    for i in 1..=150 {
        doc.push_str(&format!("## S{i}\nText. main.js:1-6\n"));
    }
    std::fs::write(&s.project.output, doc).unwrap();
    s.document_review = Default::default();
    let first = document_review::request(&mut s).unwrap();
    let first: Value =
        serde_json::from_str(first["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(first["heading_count"], 151);
    assert_eq!(first["document_outline_omitted"], 31);
    let headings: Vec<&str> = first["document_outline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["heading"].as_str().unwrap())
        .collect();
    assert_eq!(headings.len(), 120, "{headings:?}");
    // Lines 1-100 hold S1..S50, four neighbours follow, then level-2
    // sections from the top while they fit; the last sections are left out.
    assert_eq!(headings[0], "# Intro");
    assert_eq!(headings[50], "## S50");
    assert_eq!(headings[54], "## S54");
    assert_eq!(headings[119], "## S119");
    assert!(!headings.contains(&"## S120"));
    assert_eq!(first["document_outline"][1]["start_line"], 2);
}

#[test]
fn a_rereview_page_lists_only_its_own_changed_sections() {
    // changed_sections went into every page of a re-review as the whole
    // document's list (every section after a full rewrite). A page judges
    // only its own range, so it lists the changed sections it overlaps and
    // changed_section_count counts them all.
    let (_dir, mut s) = fixture();
    let mut doc = String::from("# Guide\n");
    for i in 0..60 {
        doc.push_str(&format!("## S{i}\nText {i}. main.js:1-6\n\n"));
    }
    std::fs::write(&s.project.output, &doc).unwrap();
    s.document_review = Default::default();
    let mut payloads = Vec::new();
    let review = |s: &mut Session, payloads: &mut Vec<Value>| loop {
        let request = document_review::request(s).unwrap();
        payloads.push(
            serde_json::from_str::<Value>(request["messages"][1]["content"].as_str().unwrap())
                .unwrap(),
        );
        support::document_review::finish(s, r#"{"issues":[]}"#).unwrap();
        if !s.document_review.pending {
            break;
        }
        assert!(payloads.len() < 32);
    };
    review(&mut s, &mut payloads);
    assert!(payloads.iter().all(|p| p["changed_sections"].is_null()));
    assert!(document_review::approved(&s));

    // S1 starts on line 5 and S50 on line 152: the re-review requests one
    // range from each and nothing between them.
    let doc = doc
        .replace("Text 1. ", "Text one. ")
        .replace("Text 50. ", "Text fifty. ");
    std::fs::write(&s.project.output, &doc).unwrap();
    payloads.clear();
    review(&mut s, &mut payloads);
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0]["document_line_start"], 5);
    assert_eq!(payloads[0]["changed_sections"], json!(["# Guide > ## S1"]));
    assert_eq!(payloads[1]["document_line_start"], 152);
    assert_eq!(payloads[1]["changed_sections"], json!(["# Guide > ## S50"]));
    for payload in &payloads {
        assert_eq!(payload["changed_section_count"], 2, "{payload}");
    }
}

/// The payload of the next review request.
fn next_payload(s: &mut Session) -> Value {
    let request = document_review::request(s).unwrap();
    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap()
}

/// A factual issue about the document line `line` that reads `quote`.
fn line_issue(line: usize, quote: &str) -> Value {
    json!({"previous_id":null,"kind":"factual",
        "document":{"start_line":line,"end_line":line,"quote":quote},
        "requirement_id":null,
        "sources":[{"path":"main.js","start_line":4,"end_line":4,"quote":"work(turns);"}],
        "problem":format!("{quote} misstates the loop."),"correction":"State the loop bound.","ui_labels":[]})
}

/// "# Guide" and `sections` sections of three lines: "## S{i}" on line
/// 2 + 3i, then "Text {i}." with a citation, then a blank line.
fn guide(sections: usize) -> String {
    let mut doc = String::from("# Guide\n");
    for i in 0..sections {
        doc.push_str(&format!("## S{i}\nText {i}. main.js:1-6\n\n"));
    }
    doc
}

#[test]
fn a_review_stops_at_its_finding_limit_and_the_next_one_covers_the_rest() {
    // Live: once twelve candidates were collected, later pages kept being
    // requested, their findings were dropped, and every section was marked
    // reviewed, so a re-review never looked at them again.
    let (_dir, mut s) = fixture();
    std::fs::write(&s.project.output, guide(60)).unwrap();
    s.document_review = Default::default();
    let first = next_payload(&mut s);
    assert_eq!(first["document_line_start"], 1);
    let end = first["document_line_end"].as_u64().unwrap() as usize;
    let issues: Vec<Value> = (0..12)
        .map(|i| line_issue(3 + 3 * i, &format!("Text {i}.")))
        .collect();
    support::document_review::finish(&mut s, &json!({"issues":issues}).to_string()).unwrap();
    // No further range was requested: the twelve findings went to validation
    // and the verdict asks for their repair.
    assert!(!s.document_review.pending);
    assert_eq!(s.document_review.issues.len(), 12);
    assert_eq!(document_review::unreached_ranges(&s), [(end + 1, 181)]);

    // The repair review re-checks the repaired passages and covers the
    // unreached sections in full: they are listed as changed and their
    // citations are sent.
    let doc = guide(60).replace("Text 0. ", "Text zero. ");
    std::fs::write(&s.project.output, &doc).unwrap();
    let mut late = Vec::new();
    loop {
        let payload = next_payload(&mut s);
        if payload["document_line_start"].as_u64().unwrap() as usize > end {
            late.push(payload);
        }
        support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
        if !s.document_review.pending {
            break;
        }
    }
    assert!(!late.is_empty());
    for payload in &late {
        assert!(!payload["changed_sections"].as_array().unwrap().is_empty());
        assert!(!payload["evidence"].as_array().unwrap().is_empty());
    }
    assert!(document_review::approved(&s));
    assert!(document_review::unreached_ranges(&s).is_empty());
}

/// Dismiss every candidate of the pending finding validation.
fn dismiss_all(s: &mut Session) {
    while s.document_review.validating {
        let payload = next_payload(s);
        let decisions: Vec<Value> = payload["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| json!({"id":c["id"],"status":"dismissed","reason":"The source supports the passage as written.","duplicate_of":null}))
            .collect();
        document_review::finish(s, &json!({"decisions":decisions}).to_string()).unwrap();
    }
}

#[test]
fn a_full_cycle_of_dismissed_findings_continues_the_review_once_per_position() {
    let (dir, mut s) = fixture();
    // The second range cites a long file, so it spans several evidence pages.
    let big: String = (1..=600)
        .map(|n| {
            format!(
                "const value_{n} = compute_value({n}, \"{}\");\n",
                "x".repeat(40)
            )
        })
        .collect();
    std::fs::write(dir.path().join("big.js"), big).unwrap();
    let mut doc = guide(40);
    let second = doc.lines().count() + 1;
    for i in 0..12 {
        doc.push_str(&format!("## B{i}\nBig {i}. big.js:1-600\n\n"));
    }
    std::fs::write(&s.project.output, &doc).unwrap();
    s.document_review = Default::default();
    s.document_review.pending = true;
    let first = next_payload(&mut s);
    assert!((first["document_line_end"].as_u64().unwrap() as usize) < second);
    let issues: Vec<Value> = (0..12)
        .map(|i| line_issue(3 + 3 * i, &format!("Text {i}.")))
        .collect();
    document_review::finish(&mut s, &json!({"issues":issues}).to_string()).unwrap();
    dismiss_all(&mut s);
    // Nothing to repair: the review goes on with the unreached lines instead
    // of approving them unseen.
    assert!(s.document_review.pending);
    assert!(!document_review::approved(&s));
    let resumed = next_payload(&mut s);
    let start = resumed["document_line_start"].as_u64().unwrap() as usize;
    assert!(start > first["document_line_end"].as_u64().unwrap() as usize);
    assert!(resumed["more_evidence_pages"].as_bool().unwrap());

    // Filling the findings again before passing that position would repeat
    // the same lines: they are reported unreviewed instead.
    let issues: Vec<Value> = (0..12)
        .map(|i| {
            let line = second + 1 + 3 * i;
            let mut issue = line_issue(line, &format!("Big {i}."));
            issue["sources"] = json!([{"path":"big.js","start_line":1,"end_line":1,
                "quote":format!("const value_1 = compute_value(1, \"{}\");", "x".repeat(40))}]);
            issue
        })
        .collect();
    document_review::finish(&mut s, &json!({"issues":issues}).to_string()).unwrap();
    dismiss_all(&mut s);
    assert!(!s.document_review.pending);
    assert!(document_review::unavailable_on_current(&s));
    assert_eq!(
        document_review::unavailable_ranges(&s),
        [(start, doc.lines().count())]
    );
}

#[test]
fn a_review_cut_off_by_the_deadline_continues_where_it_stopped() {
    let (_dir, mut s) = fixture();
    let doc = guide(60);
    std::fs::write(&s.project.output, &doc).unwrap();
    s.document_review = Default::default();
    s.document_review.pending = true;
    let first = next_payload(&mut s);
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    let second = next_payload(&mut s);
    assert!(second["document_line_start"].as_u64() > first["document_line_start"].as_u64());

    // The deadline ends the run while the second page is pending.
    document_review::pause(&mut s);
    assert!(!s.document_review.pending);
    assert_eq!(document_review::paused_pages(&s), Some(1));
    assert_eq!(
        document_review::current_verdict(&s),
        document_review::CurrentVerdict::Unreviewed
    );
    // The next final answer of the unchanged document resumes that page.
    s.document_review.pending = true;
    let resumed = next_payload(&mut s);
    assert_eq!(
        resumed["document_line_start"],
        second["document_line_start"]
    );
    assert_eq!(resumed["evidence_page"], second["evidence_page"]);
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(!s.document_review.pending);
    assert!(document_review::approved(&s));
    assert_eq!(s.document_review.attempts, 1);

    // An edit after the pause starts the review over.
    std::fs::write(&s.project.output, guide(61)).unwrap();
    s.document_review = Default::default();
    s.document_review.pending = true;
    next_payload(&mut s);
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    document_review::pause(&mut s);
    std::fs::write(&s.project.output, &doc).unwrap();
    assert_eq!(document_review::paused_pages(&s), None);
    s.document_review.pending = true;
    assert_eq!(next_payload(&mut s)["document_line_start"], 1);
}

#[test]
fn renumbered_headings_and_unchanged_ranges_are_not_reviewed_again() {
    let (_dir, mut s) = fixture();
    let numbered = |inserted: bool| {
        let mut doc = String::from("# Guide\n");
        let mut number = 0;
        for i in 0..60 {
            number += 1;
            doc.push_str(&format!("## {number}. Part {i}\nText {i}. main.js:1-6\n\n"));
            if inserted && i == 29 {
                number += 1;
                doc.push_str(&format!("## {number}. New part\nNew text. main.js:3-5\n\n"));
            }
        }
        doc
    };
    std::fs::write(&s.project.output, numbered(false)).unwrap();
    assert!(review_requests(&mut s) > 1);
    assert!(document_review::approved(&s));

    // A section inserted in the middle renumbers every later heading; only
    // the new section is reviewed again, in one request.
    std::fs::write(&s.project.output, numbered(true)).unwrap();
    assert_eq!(document_review::estimated_requests(&s), 2);
    let payload = next_payload(&mut s);
    assert_eq!(payload["changed_section_count"], 1);
    assert_eq!(
        payload["changed_sections"],
        json!(["# Guide > ## 31. New part"])
    );
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(!s.document_review.pending);
    assert!(document_review::approved(&s));
}

#[test]
fn a_cited_line_too_long_for_review_evidence_is_reported_for_repair() {
    // A cited line of generated text failed review setup with "narrow
    // citations", which a single line cannot do.
    let (dir, mut s) = fixture();
    let bundle = format!(
        "// bundle\nvar data=\"{}\";\nrun(data);\n",
        "a1b2".repeat(5000)
    );
    std::fs::write(dir.path().join("bundle.js"), bundle).unwrap();
    tools::execute(&mut s, "file_read", json!({"path":"bundle.js"})).unwrap();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":hash,
            "text":"# Flow\nThe bundle runs its data. bundle.js:1-3\n"}),
    )
    .unwrap();
    let check = &saved["citation_check"];
    assert_eq!(check["long_line_citation_count"], 1, "{saved}");
    assert_eq!(check["long_line_citations"][0]["source_line"], 2);

    s.document_review = Default::default();
    let payload = next_payload(&mut s);
    let evidence = payload["evidence"].to_string();
    assert!(
        evidence.contains("2|[line not shown: 20012 bytes"),
        "{evidence}"
    );
    assert!(evidence.contains("3|run(data);"));
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    let document_review::CurrentVerdict::Rejected(issues) = document_review::current_verdict(&s)
    else {
        panic!("a citation the review cannot check is not approved");
    };
    assert!(issues[0].starts_with("인용 근거:"), "{issues:?}");
    assert!(issues[0].contains("bundle.js:1-3"));

    // Leaving the long line out of the citation repairs it.
    std::fs::write(
        &s.project.output,
        "# Flow\nThe bundle runs its data. bundle.js:3\n",
    )
    .unwrap();
    next_payload(&mut s);
    support::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(document_review::approved(&s));
}
