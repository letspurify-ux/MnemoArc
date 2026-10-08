use crate::support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::{AgentEvent, run_session},
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
    tools::{
        self,
        completion_review::{self as review, Gate},
    },
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn session(root: &std::path::Path) -> Session {
    let mut s = Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.task.completion = vec!["Conclusion is saved".into(), "An example is saved".into()];
    s.add_user("Save result.txt with a conclusion and an example".into());
    s
}

#[test]
fn disabled_completion_review_does_not_gate_artifact_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.completion_review_enabled = false;
    s.task.workflow = "source_document".into();
    s.document_written = true;

    assert!(!review::required(&s));
    assert_eq!(
        review::begin_final(&mut s, "The requested artifact is complete.", false).unwrap(),
        Gate::Accepted
    );
    assert!(!s.completion_review.pending);
    assert!(!s.completion_review.approved);
}

#[test]
fn disabled_completion_review_is_not_shown_as_required_after_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.completion_review_enabled = false;
    let call = ToolCall {
        id: "write-with-review-disabled".into(),
        name: "document_edit".into(),
        arguments: json!({"action":"create","text":"# Manual"}).to_string(),
    };
    review::observe(&mut s, &call, &json!({"status":"ok","data":{}}));
    assert!(s.completion_review.artifact_work);
    assert!(!s.completion_review.required);

    let task_call = ToolCall {
        id: "completion-with-review-disabled".into(),
        name: "task_state".into(),
        arguments: json!({"action":"update","patch":{"completion":["Manual saved"]}}).to_string(),
    };
    review::observe(&mut s, &task_call, &json!({"status":"ok","data":{}}));
    assert!(!s.completion_review.required);
}

#[test]
fn disabled_completion_review_is_not_required_by_first_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            completion_review_enabled: false,
            ..support::compact_config()
        },
    );
    s.task.completion = vec!["Manual saved".into()];
    s.add_user("Write a manual".into());
    assert!(!s.completion_review.required);
}

fn write(s: &mut Session, content: &str) {
    let path = s.project.root.join("result.txt");
    let mut args = json!({"path":"result.txt","content":content});
    if path.exists() {
        args["expected_hash"] = json!(tools::hash(&std::fs::read(path).unwrap()));
    }
    let call = ToolCall {
        id: format!("write-{}", tools::hash(content.as_bytes())),
        name: "file_write".into(),
        arguments: args.to_string(),
    };
    let result = tools::run_call(s, &call);
    assert_eq!(result["status"], "ok", "{result}");
    review::observe(s, &call, &result);
}

#[test]
fn patch_write_log_tracks_committed_paths_before_result_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.active_tools = tools::ToolRegistry::optional_names();
    for path in ["update.rs", "move.rs", "delete.rs", "unchanged.rs"] {
        std::fs::write(dir.path().join(path), "original\n").unwrap();
    }
    let digest = tools::hash(b"original\n");
    let mut operations = vec![
        json!({"action":"update","path":"update.rs","expected_hash":digest,"old_text":"original","new_text":"changed"}),
        json!({"action":"move","path":"move.rs","to_path":"moved.rs","expected_hash":digest}),
        json!({"action":"delete","path":"delete.rs","expected_hash":digest}),
        json!({"action":"replace","path":"unchanged.rs","expected_hash":digest,"content":"original\n"}),
    ];
    let mut expected: std::collections::BTreeSet<String> =
        ["update.rs", "move.rs", "moved.rs", "delete.rs"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    for i in 0..16 {
        let path = format!("added-{i}-{}.rs", "long-name-".repeat(6));
        operations.push(json!({"action":"add","path":path,"content":"new\n"}));
        expected.insert(path);
    }
    let call = ToolCall {
        id: "patch".into(),
        name: "file_patch".into(),
        arguments: json!({"operations":operations}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["status"], "ok", "{result}");
    let limited = tools::limit_result(&mut s, &call, result, 200);
    assert_eq!(limited["truncated"], true, "{limited}");
    review::observe(&mut s, &call, &limited);
    assert!(!dir.path().join("move.rs").exists());
    assert!(!dir.path().join("delete.rs").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("moved.rs")).unwrap(),
        "original\n"
    );

    let failed = ToolCall {
        id: "failed-patch".into(),
        name: "file_patch".into(),
        arguments: json!({"operations":[
            {"action":"add","path":"uncommitted.rs","content":"not committed"},
            {"action":"delete","path":"unchanged.rs","expected_hash":"wrong"}
        ]})
        .to_string(),
    };
    let result = tools::run_call(&mut s, &failed);
    assert_eq!(result["status"], "error");
    review::observe(&mut s, &failed, &result);
    assert!(!dir.path().join("uncommitted.rs").exists());

    review::begin(&mut s, "Applied the patch").unwrap();
    let data = payload(&review::request(&mut s).unwrap());
    let log = data["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "runtime_write_log")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let actual: std::collections::BTreeSet<String> = log["written_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| {
            std::path::Path::new(path.as_str().unwrap())
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(actual, expected);
}

fn plan(s: &mut Session, operations: Value) {
    let result = tools::execute(
        s,
        "task_plan",
        json!({"action":"apply","expected_revision":s.task.plan_revision,"operations":operations}),
    )
    .unwrap();
    assert_eq!(result["applied"], true, "{result}");
}

fn payload(request: &Value) -> Value {
    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap()
}

fn existing_document(root: &std::path::Path, before: &str) -> Session {
    let output = root.join("manual.md");
    std::fs::write(&output, before).unwrap();
    let mut s = Session::new(
        Project {
            root: root.into(),
            output,
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.add_user("Fix three passages in the existing document; preserve every other passage.".into());
    s.select_workflow("source_document").unwrap();
    s
}

fn document_step(s: &mut Session, name: &str, args: Value) -> Value {
    let call = ToolCall {
        id: format!("{name}-{}", tools::hash(args.to_string().as_bytes())),
        name: name.into(),
        arguments: args.to_string(),
    };
    let result = tools::run_call(s, &call);
    review::observe(s, &call, &result);
    result
}

fn comparison(p: &Value) -> &Value {
    p["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "runtime_document_changes")
        .expect("runtime preservation evidence")
}

#[test]
fn partial_document_edits_keep_original_net_changes_after_receipt_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let before = "# Manual\n## A\nOld condition.\n## B\nKeep this example.\n## C\nOld term.\n## D\nKeep this explanation.\n## E\nOld command.\n";
    let after = before
        .replace("Old condition.", "Correct condition.")
        .replace("Old term.", "Correct term.")
        .replace("Old command.", "Correct command.");
    let mut s = existing_document(dir.path(), before);
    let inspected = document_step(&mut s, "document_inspect", json!({"section":"# Manual"}));
    assert_eq!(inspected["status"], "ok");
    let edited = document_step(
        &mut s,
        "document_edit_batch",
        json!({
        "expected_hash":tools::hash(before.as_bytes()),"edits":[
            {"action":"replace_text","old_text":"Old condition.","text":"Correct condition."},
            {"action":"replace_text","old_text":"Old term.","text":"Correct term."},
            {"action":"replace_text","old_text":"Old command.","text":"Correct command."}
        ]}),
    );
    assert_eq!(edited["status"], "ok", "{edited}");
    // A later successful edit must not replace the original preimage.
    let edited = document_step(
        &mut s,
        "document_edit",
        json!({"action":"replace_text",
        "expected_hash":tools::hash(after.as_bytes()),"old_text":"Correct term.","text":"Final term."}),
    );
    assert_eq!(edited["status"], "ok", "{edited}");
    let after = after.replace("Correct term.", "Final term.");
    for i in 0..20 {
        let name = format!("source{i}.rs");
        std::fs::write(dir.path().join(&name), format!("// evidence {i}\n")).unwrap();
        let read = document_step(&mut s, "file_read", json!({"path":name}));
        assert_eq!(read["status"], "ok", "{read}");
    }
    s.history.bundles.clear(); // Checkpoint cleanup cannot erase the baseline.
    review::begin(&mut s, "Saved the three fixes.").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let diff = comparison(&p);
    assert_eq!(diff["baseline_hash"], tools::hash(before.as_bytes()));
    assert_eq!(diff["current_hash"], tools::hash(after.as_bytes()));
    assert_eq!(diff["total_changes"], 3);
    assert_eq!(diff["truncated"], false);
    assert_eq!(diff["unchanged_outside_reported_ranges"], true);
    assert_eq!(diff["changes"][1]["before"]["text"], "Old term.\n");
    assert_eq!(diff["changes"][1]["after"]["text"], "Final term.\n");
    assert!(
        !p["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "tool_observation" && e["data"]["tool"] == "document_inspect")
    );
    assert_eq!(p["evidence"][3]["kind"], "current_file");
    assert_eq!(p["evidence"][4]["kind"], "runtime_document_changes");
    review::finish(&mut s, &verdict(&p, true)).unwrap();
    // Recompute from the actual current bytes, including external changes.
    std::fs::write(
        &s.project.output,
        after.replace("Keep this example.", "Unexpected change."),
    )
    .unwrap();
    assert_eq!(
        review::begin(&mut s, "Saved the three fixes.").unwrap(),
        Gate::Review
    );
    let p = payload(&review::request(&mut s).unwrap());
    assert_eq!(comparison(&p)["total_changes"], 4);
    assert!(comparison(&p).to_string().contains("Unexpected change."));
}

#[test]
fn failed_document_edits_do_not_capture_a_stale_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = existing_document(dir.path(), "# Manual\nOld.\n");
    let failed = document_step(
        &mut s,
        "document_edit",
        json!({"action":"replace_text",
        "expected_hash":tools::hash(b"wrong version"),"old_text":"Old.","text":"Correct."}),
    );
    assert_eq!(failed["status"], "error");
    let before = "# Manual\nExternal version.\n";
    std::fs::write(&s.project.output, before).unwrap();
    let edited = document_step(
        &mut s,
        "document_edit",
        json!({"action":"replace_text",
        "expected_hash":tools::hash(before.as_bytes()),"old_text":"External version.","text":"Correct."}),
    );
    assert_eq!(edited["status"], "ok", "{edited}");
    review::begin(&mut s, "Saved.").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    assert_eq!(
        comparison(&p)["baseline_hash"],
        tools::hash(before.as_bytes())
    );
    assert_eq!(
        comparison(&p)["changes"][0]["before"]["text"],
        "External version.\n"
    );
}

#[tokio::test]
async fn routed_amendments_preserve_unfinished_changes_and_rebase_completed_documents() {
    const CHANGE: &str = "Also change First. to Second.; preserve all other original passages.";
    const GOAL: &str = "Change the target to Second.; preserve all other original passages.";
    struct AmendmentThenStop;
    #[async_trait]
    impl LlmClient for AmendmentThenStop {
        async fn complete(
            &self,
            request: Value,
            _: &Config,
            _: CancellationToken,
            _: mpsc::Sender<String>,
        ) -> Result<Completion> {
            if request["response_format"]["json_schema"]["name"] == "session_message_routing" {
                return Ok(Completion {
                    text: json!({"intent":"work","authorization_quote":CHANGE,
                        "changes":{"goal":GOAL}})
                    .to_string(),
                    ..Default::default()
                });
            }
            anyhow::bail!("offline amendment test: stop after routing")
        }
    }

    for (status, preexisting) in [
        ("blocked", true),
        ("partial", true),
        ("cancelled", true),
        ("complete_with_gaps", true),
        ("complete", true),
        ("blocked", false),
        ("complete", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let before = "# Manual\n## Target\nOld.\n## Keep\nProtected original.\n";
        let mut s = existing_document(dir.path(), before);
        let first = before.replace("Old.", "First.");
        let new_comparison = status == "complete" || !preexisting;
        let first = if new_comparison {
            first
        } else {
            first.replace("Protected original.", "Accidentally corrupted.")
        };
        let edit = if preexisting {
            json!({"action":"write","text":first,"expected_hash":tools::hash(before.as_bytes())})
        } else {
            // No document existed before this task created its first draft.
            std::fs::remove_file(&s.project.output).unwrap();
            json!({"action":"create","text":first})
        };
        let edited = document_step(&mut s, "document_edit", edit);
        assert_eq!(edited["status"], "ok", "{edited}");
        s.status = status.into();
        s.receive_message(CHANGE.into()).unwrap();
        let (events, mut rx) = mpsc::channel::<AgentEvent>(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut s = run_session(
            s,
            Arc::new(AmendmentThenStop),
            CancellationToken::new(),
            events,
        )
        .await;
        drain.await.unwrap();
        assert_eq!(s.latest_request, GOAL, "{status}: {:?}", s.last_error);
        // Existing originals survive an unfinished task's amendment. A
        // completed result or a newly created draft can start a new comparison.
        review::begin(&mut s, "Saved.").unwrap();
        let p = payload(&review::request(&mut s).unwrap());
        if new_comparison {
            assert!(
                !p["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "runtime_document_changes")
            );
        } else {
            assert_eq!(
                comparison(&p)["baseline_hash"],
                tools::hash(before.as_bytes()),
                "{status}"
            );
            assert!(
                comparison(&p).to_string().contains("Protected original."),
                "{status}"
            );
        }
        let edited = document_step(
            &mut s,
            "document_edit",
            json!({"action":"replace_text","old_text":"First.","text":"Second.",
                "expected_hash":tools::hash(first.as_bytes())}),
        );
        assert_eq!(edited["status"], "ok", "{edited}");
        review::begin(&mut s, "Saved.").unwrap();
        let p = payload(&review::request(&mut s).unwrap());
        let diff = comparison(&p);
        if new_comparison {
            assert_eq!(diff["baseline_hash"], tools::hash(first.as_bytes()));
            assert_eq!(diff["total_changes"], 1);
            assert_eq!(diff["changes"][0]["before"]["text"], "First.\n");
        } else {
            assert_eq!(
                diff["baseline_hash"],
                tools::hash(before.as_bytes()),
                "{status}"
            );
            assert_eq!(diff["total_changes"], 2, "{status}");
            assert_eq!(
                diff["changes"][1]["before"]["text"],
                "Protected original.\n"
            );
            assert_eq!(
                diff["changes"][1]["after"]["text"],
                "Accidentally corrupted.\n"
            );
        }
    }
}

fn verdict(payload: &Value, met: bool) -> String {
    let evidence = payload["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "current_file")
        .map(|e| e["id"].clone())
        .unwrap_or(json!("answer"));
    json!({"checks":payload["criteria"].as_array().unwrap().iter().map(|c|json!({"id":c["id"],"status":if met {"met"} else {"unmet"},"reason":if met {"Saved content meets this condition"} else {"Saved file has no example"},"evidence":[evidence],"next_action":if met {""} else {"Add the requested example to result.txt"}})).collect::<Vec<_>>()}).to_string()
}

fn finish(s: &mut Session, met: bool) -> Option<String> {
    let p = payload(&review::request(s).unwrap());
    review::finish(s, &verdict(&p, met)).unwrap()
}

#[test]
fn working_checks_cannot_add_requirements_or_weaken_caller_requirements() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            ..support::compact_config()
        },
    );
    s.task.completion = vec!["Include a conclusion and an example".into()];
    s.task.constraints = vec!["Use Korean".into()];
    s.task.deliverables = vec!["result.txt".into()];
    s.add_user("Save result.txt with a conclusion and an example".into());
    write(&mut s, "결론과 예시\n");
    s.task.completion = vec!["Every investigation must be exactly written".into()];
    s.task.constraints.clear();
    s.task
        .deliverables
        .push("Internal investigation ledger".into());

    assert_eq!(
        review::begin(&mut s, "Saved result.txt").unwrap(),
        Gate::Review
    );
    let p = payload(&review::request(&mut s).unwrap());
    assert_eq!(p["original_request"], s.answer_review_question);
    assert_eq!(p["criteria"].as_array().unwrap().len(), 2);
    assert_eq!(
        p["criteria"][1]["text"],
        "Include a conclusion and an example"
    );
    assert_eq!(p["constraints"], json!(["Use Korean"]));
    assert_eq!(p["deliverables"], json!(["result.txt"]));
    assert!(!p.to_string().contains("exactly written"));
    assert!(!p.to_string().contains("Internal investigation ledger"));

    // Refining a working checklist during a review neither weakens the
    // caller's requirements nor makes an otherwise current verdict stale.
    s.task.completion = vec!["Conclusion only is enough".into()];
    assert_eq!(
        review::finish(&mut s, &verdict(&p, true))
            .unwrap()
            .as_deref(),
        Some("Saved result.txt")
    );
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Approved
    );
    assert_eq!(
        review::begin(&mut s, "Saved result.txt").unwrap(),
        Gate::Accepted
    );
    s.add_user("continue".into());
    assert_eq!(
        s.request_review_criteria.completion,
        ["Include a conclusion and an example"]
    );
}

#[test]
fn reading_an_unread_citation_gets_a_fresh_review_without_a_document_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ui.js"), "export function openChat() {}\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("manual.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            ..support::compact_config()
        },
    );
    s.add_user("ui 사용자 매뉴얼 만들어줘".into());
    s.select_workflow("source_document").unwrap();
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{
            "completion":["모든 화면 절이 저장되고 감사로 구조/인용 오류가 없다"]
        }}),
    )
    .unwrap();
    let body = (1..=7)
        .map(|i| format!("# Screen {i}\nOpen the chat screen. ui.js:1\n\n"))
        .collect::<String>();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":body}),
    )
    .unwrap();
    let saved_hash = s.last_document_write.as_ref().unwrap().1.clone();
    // Match the live run: both review options are on, and document review has
    // already approved the unchanged manual before completion is retried.
    assert!(s.config.source_document_review && s.config.completion_review_enabled);
    tools::document_review::request(&mut s).unwrap();
    tools::document_review::finish(&mut s, r#"{"issues":[]}"#).unwrap();
    assert!(tools::document_review::approved(&s));
    assert_eq!(
        review::begin(&mut s, "매뉴얼을 저장했습니다.").unwrap(),
        Gate::Review
    );
    let p = payload(&review::request(&mut s).unwrap());
    // Only the actual user request and the runtime scope check are criteria.
    let ids: Vec<_> = p["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["R0", "S1"]);
    assert_eq!(p["evidence"][2]["kind"], "runtime_citation_coverage", "{p}");
    assert_eq!(p["evidence"][2]["unread_count"], 1, "{p}");
    assert_eq!(p["evidence"][2]["unread_citations"][0]["path"], "ui.js");
    let mut response: Value = serde_json::from_str(&verdict(&p, false)).unwrap();
    response["checks"][0]["reason"] = json!("ui.js:1 is cited but was never read");
    response["checks"][0]["next_action"] = json!("Read ui.js:1");
    response["checks"][1] = json!({"id":"S1","status":"met","reason":"The manual concerns the read screen only","evidence":[p["evidence"][1]["id"]],"next_action":""});
    assert!(
        review::finish(&mut s, &response.to_string())
            .unwrap()
            .is_none()
    );
    assert!(review::rejected_on_current_result(&s));

    // Reading the cited range changes the runtime evidence, not the file.
    tools::execute(&mut s, "file_read", json!({"path":"ui.js"})).unwrap();
    assert_eq!(s.last_document_write.as_ref().unwrap().1, saved_hash);
    assert!(tools::unread_citations(&s).unwrap().is_empty());
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Unreviewed
    );
    assert_eq!(review::guidance(&s)["checks"], json!([]));
    assert_eq!(review::view(&s)["approved"], false);
    assert_eq!(review::view(&s)["needs_review"], true);
    assert_eq!(
        review::begin(&mut s, "매뉴얼을 저장했습니다.").unwrap(),
        Gate::Review
    );
    let p = payload(&review::request(&mut s).unwrap());
    assert_eq!(p["evidence"][2]["unread_count"], 0, "{p}");
    assert_eq!(
        review::finish(&mut s, &verdict(&p, true))
            .unwrap()
            .as_deref(),
        Some("매뉴얼을 저장했습니다.")
    );
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Approved
    );
    assert_eq!(review::view(&s)["approved"], true);
    assert_eq!(review::view(&s)["needs_review"], false);
}

#[test]
fn targeted_read_keeps_evidence_beyond_the_old_receipt_character_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let marker = "Example: the requested tail is present";
    write(
        &mut s,
        &format!(
            "{}\n{}\n{marker}\n",
            "Introduction ".repeat(600),
            "Evidence ".repeat(450)
        ),
    );
    let call = ToolCall {
        id: "read-tail".into(),
        name: "file_read".into(),
        arguments: json!({"path":"result.txt","start_line":2,"max_lines":2}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["status"], "ok", "{result}");
    assert!(
        result["data"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains(marker)
    );
    review::observe(&mut s, &call, &result);
    review::begin(&mut s, "Saved result.txt").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let evidence = p["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "tool_observation" && e["data"]["tool"] == "file_read")
        .unwrap();
    assert_eq!(evidence["data"]["truncated"], false);
    assert!(
        evidence["data"]["observed"]
            .as_str()
            .unwrap()
            .contains(marker)
    );
    assert!(mnemoarc::context::count(&review::request(&mut s).unwrap(), &s.config.model) <= 24_000);
    assert_eq!(finish(&mut s, true).as_deref(), Some("Saved result.txt"));
}

#[test]
fn repeated_evidence_order_does_not_restart_rejected_completion_review() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(
        &mut s,
        "Conclusion only\nStill missing the requested example\n",
    );
    let mut reads = Vec::new();
    for line in [1, 2] {
        let call = ToolCall {
            id: format!("read-{line}"),
            name: "file_read".into(),
            arguments: json!({"path":"result.txt","start_line":line,"max_lines":1}).to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        review::observe(&mut s, &call, &result);
        reads.push((call, result));
    }
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    assert!(finish(&mut s, false).is_none());
    for (call, result) in reads.iter().cycle().take(12) {
        review::observe(&mut s, call, result);
        assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Repair);
    }
    review::observe(
        &mut s,
        &reads[0].0,
        &json!({"status":"ok","data":{
            "path":dir.path().join("result.txt"),"hash":tools::hash(&std::fs::read(dir.path().join("result.txt")).unwrap()),"suppressed":true
        }}),
    );
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Repair);
    assert_eq!(s.completion_review.attempts, 1);
}

#[test]
fn database_observation_order_remains_part_of_completion_version() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only");
    let call = ToolCall {
        id: "query".into(),
        name: "db_query".into(),
        arguments: "{}".into(),
    };
    let before = json!({"status":"ok","data":{"rows":[{"state":"before"}]}});
    let after = json!({"status":"ok","data":{"rows":[{"state":"after"}]}});
    review::observe(&mut s, &call, &before);
    review::observe(&mut s, &call, &after);
    review::begin(&mut s, "Done").unwrap();
    assert!(finish(&mut s, false).is_none());
    review::observe(&mut s, &call, &before);
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
}

#[test]
fn empty_plan_is_not_acceptance_and_repair_is_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion\n");
    plan(
        &mut s,
        json!([{"op":"insert","texts":["Write result"]},{"op":"complete","id":"T1","result":"Saved file"}]),
    );
    assert!(s.task.current_todo().is_none());
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    assert!(finish(&mut s, false).is_none());
    review::schedule_repairs(&mut s);
    review::schedule_repairs(&mut s);
    assert_eq!(s.task.todos.iter().filter(|t| !t.done).count(), 1);
    assert!(!s.completion_review.approved);
    let id = s.task.current_todo().unwrap().id.clone();
    plan(
        &mut s,
        json!([{"op":"complete","id":id,"result":"Only marked done"}]),
    );
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Repair);
    review::schedule_repairs(&mut s);
    assert_eq!(s.task.current_todo().unwrap().id, id);
    assert_eq!(s.completion_review.attempts, 1);
    write(&mut s, "Conclusion\nExample: real result\n");
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    assert_eq!(finish(&mut s, true).as_deref(), Some("Done"));
}

#[test]
fn reworded_repair_reuses_its_criterion_item_and_distinct_actions_split() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only");

    for (answer, action, distinct) in [
        (
            "Draft one",
            "Add the requested example to result.txt",
            false,
        ),
        ("Draft two", "Read and add the missing example", false),
        ("Draft three", "Read and add the missing example", true),
    ] {
        assert_eq!(review::begin(&mut s, answer).unwrap(), Gate::Review);
        let p = payload(&review::request(&mut s).unwrap());
        let mut response: Value = serde_json::from_str(&verdict(&p, false)).unwrap();
        for check in response["checks"].as_array_mut().unwrap() {
            check["next_action"] = json!(if distinct && check["id"] == "R0" {
                "Correct the conclusion"
            } else {
                action
            });
        }
        assert!(
            review::finish(&mut s, &response.to_string())
                .unwrap()
                .is_none()
        );
        review::schedule_repairs(&mut s);
        let expected = if distinct { 2 } else { 1 };
        assert_eq!(
            s.task.todos.iter().filter(|item| !item.done).count(),
            expected
        );
        assert!(s.task.todos.iter().any(|item| item.id == "T1"));
        if !distinct {
            let current = s.task.current_todo().unwrap();
            assert_eq!(current.id, "T1");
            assert_eq!(current.text, action);
            plan(
                &mut s,
                json!([{"op":"complete","id":"T1","result":"Still incomplete"}]),
            );
        }
    }
}

#[test]
fn approval_requires_every_page_and_is_invalidated_by_files_or_requirements() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion and Example\n");
    s.request_review_criteria.completion = (0..20).map(|i| format!("Condition {i}")).collect();
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    assert!(finish(&mut s, true).is_none());
    assert!(s.completion_review.pending);
    assert!(finish(&mut s, true).is_none());
    assert!(!s.completion_review.approved);
    assert_eq!(finish(&mut s, true).as_deref(), Some("Done"));
    assert_eq!(s.completion_review.checks.len(), 21);
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Accepted);
    std::fs::write(dir.path().join("result.txt"), "Changed outside agent").unwrap();
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Unreviewed
    );
    assert_eq!(review::guidance(&s)["approved"], false);
    assert_eq!(review::view(&s)["checks"], json!([]));
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    let p = payload(&review::request(&mut s).unwrap());
    s.request_review_criteria.completion = vec!["Changed caller requirement".into()];
    assert!(review::finish(&mut s, &verdict(&p, true)).is_err());
    let p = payload(&review::request(&mut s).unwrap());
    assert_eq!(
        p["original_request"],
        "Save result.txt with a conclusion and an example"
    );
    assert_eq!(p["criteria"][0]["id"], "R0");
}

#[test]
fn malformed_missing_duplicate_and_invented_evidence_never_approve() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion");
    review::begin(&mut s, "Done").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let valid: Value = serde_json::from_str(&verdict(&p, true)).unwrap();
    let mut unknown = valid.clone();
    unknown["checks"][0]["evidence"] = json!(["invented"]);
    let mut duplicate = valid.clone();
    duplicate["checks"][1] = duplicate["checks"][0].clone();
    let mut missing = valid.clone();
    missing["checks"].as_array_mut().unwrap().pop();
    let mut self_claim = valid.clone();
    self_claim["checks"][0]["evidence"] = json!(["answer"]);
    for response in [
        "not json".into(),
        json!({"checks":[]}).to_string(),
        unknown.to_string(),
        duplicate.to_string(),
        missing.to_string(),
        self_claim.to_string(),
    ] {
        assert!(review::finish(&mut s, &response).is_err(), "{response}");
        assert!(!s.completion_review.approved);
        assert!(s.completion_review.checks.is_empty());
    }
    let mut unknown = valid;
    unknown["checks"][0]["status"] = json!("unverified");
    unknown["checks"][0]["next_action"] = json!("Read the saved example section");
    assert!(
        review::finish(&mut s, &unknown.to_string())
            .unwrap()
            .is_none()
    );
    assert!(!s.completion_review.approved);
}

#[test]
fn unresolved_and_capacity_remain_visible_without_stopping_or_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion and Example");
    s.task.unresolved = vec!["Unverified requested output".into()];
    review::begin(&mut s, "Done").unwrap();
    assert!(finish(&mut s, true).is_none());
    assert_eq!(s.completion_review.checks.last().unwrap().id, "unresolved");
    plan(
        &mut s,
        json!([{"op":"insert","texts":(0..100).map(|i|format!("Work {i}")).collect::<Vec<_>>()}]),
    );
    s.status = "running".into();
    review::schedule_repairs(&mut s);
    assert_eq!(s.status, "running");
    assert_eq!(s.task.todos.iter().filter(|t| !t.done).count(), 100);
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .contains("completion_review_unmet")
    );
    s.add_user("계속 진행".into());
    assert!(!s.completion_review.checks.is_empty());
    assert_eq!(
        s.answer_review_question,
        "Save result.txt with a conclusion and an example"
    );
    s.add_user("New question".into());
    assert!(s.completion_review.checks.is_empty());
    assert!(!review::required(&s));
}

async fn run(s: Session, client: Arc<dyn LlmClient>) -> (Session, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    });
    let result = run_session(s, client, CancellationToken::new(), tx).await;
    (result, drain.await.unwrap())
}

struct InvalidClient;
#[async_trait]
impl LlmClient for InvalidClient {
    async fn complete(
        &self,
        _: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        Ok(Completion {
            text: "Done".into(),
            ..Default::default()
        })
    }
}
#[test]
fn stale_observations_are_excluded_and_business_ids_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Old content");
    let db = ToolCall {
        id: "query".into(),
        name: "db_query".into(),
        arguments: "{}".into(),
    };
    review::observe(
        &mut s,
        &db,
        &json!({"status":"ok","data":{"rows":[{"id":123,"observed_at":"user timestamp"}]}}),
    );
    std::fs::write(dir.path().join("result.txt"), "New content").unwrap();
    review::begin(&mut s, "Done").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    assert_eq!(p["evidence_omitted"], true);
    let evidence = p["evidence"].to_string();
    assert!(evidence.contains("New content"));
    assert!(!evidence.contains("Old content"));
    assert!(evidence.contains("123"));
    assert!(evidence.contains("user timestamp"));
}

#[tokio::test]
async fn budget_exhaustion_preserves_pending_acceptance_for_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only");
    review::begin(&mut s, "Done").unwrap();
    s.config.run_tokens = 1;
    let (result, _) = run(s, Arc::new(InvalidClient)).await;
    assert_ne!(result.status, "complete");
    assert!(result.completion_review.pending);
    assert!(!result.completion_review.approved);
    assert_eq!(result.task.completion.len(), 2);
}

struct CompatibleReview {
    fenced: bool,
    limited: bool,
}
#[async_trait]
impl LlmClient for CompatibleReview {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut text = verdict(&payload(&request), true);
        if self.fenced {
            text = format!("```JSON\r\n{text}\r\n```");
        }
        Ok(Completion {
            text,
            length_limited: self.limited,
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn complete_json_verdict_tolerates_fences_and_spurious_length_without_restarting_answer() {
    for (fenced, limited) in [(true, false), (false, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        write(&mut s, "Conclusion and Example");
        review::begin(&mut s, "Verified final answer").unwrap();
        let (result, events) = run(s, Arc::new(CompatibleReview { fenced, limited })).await;
        assert_eq!(result.status, "complete", "{:?}", result.last_error);
        assert_eq!(result.completion_review.attempts, 1);
        assert!(
            events
                .iter()
                .any(|e| matches!(e,AgentEvent::Delta{text,..} if text=="Verified final answer"))
        );
    }
}

#[test]
fn runtime_write_log_and_saved_file_precede_newer_observations() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    std::fs::write(dir.path().join("source.rs"), "fn main() {}\n").unwrap();
    write(&mut s, "Conclusion: done\nExample: shown\n");
    for i in 0..3 {
        let call = ToolCall {
            id: format!("read-{i}"),
            name: "file_read".into(),
            arguments: json!({"path":"source.rs","start_line":1,"max_lines":1,"force_read":true})
                .to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        assert_eq!(result["status"], "ok", "{result}");
        review::observe(&mut s, &call, &result);
    }
    review::begin(&mut s, "Saved result.txt").unwrap();
    let request = review::request(&mut s).unwrap();
    assert!(
        request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("runtime_write_log and runtime_citation_coverage are runtime records")
    );
    let p = payload(&request);
    let evidence = p["evidence"].as_array().unwrap();
    assert_eq!(evidence[1]["kind"], "runtime_write_log", "{p}");
    let written = evidence[1]["written_paths"].as_array().unwrap();
    assert_eq!(written.len(), 1, "{p}");
    assert!(written[0].as_str().unwrap().ends_with("result.txt"));
    // A read is an observation, not a write.
    assert!(!evidence[1].to_string().contains("source.rs"));
    assert_eq!(evidence[2]["kind"], "runtime_citation_coverage", "{p}");
    assert_eq!(evidence[3]["kind"], "current_file", "{p}");
    assert_eq!(evidence[4]["kind"], "tool_observation", "{p}");
}

#[test]
fn a_source_document_review_sees_the_project_map_and_judges_scope() {
    // A live "documentation logic" document read one module of a 200-file
    // project and both reviews approved it: the reviewer saw only what the
    // writer had read, so nothing showed the areas it never opened.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/tools")).unwrap();
    std::fs::write(
        dir.path().join("src/tools/documentation.rs"),
        "fn audit() {}\nfn inspect() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/tools/document_review.rs"),
        "fn review() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/agent.rs"), "fn run() {}\n").unwrap();
    std::fs::write(dir.path().join("logo.png"), [0u8, 1, 2]).unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("manual.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.add_user("Explain the documentation logic in detail".into());
    s.select_workflow("source_document").unwrap();
    s.active_tools = tools::ToolRegistry::optional_names();
    document_step(
        &mut s,
        "file_read",
        json!({"path":"src/tools/documentation.rs"}),
    );
    document_step(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Logic\n\nAudits run first (src/tools/documentation.rs:1).\n"}),
    );
    review::begin(&mut s, "Saved manual.md").unwrap();
    let request = review::request(&mut s).unwrap();
    assert!(
        request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("runtime_project_map is a runtime record")
    );
    let p = payload(&request);
    let evidence = p["evidence"].as_array().unwrap();
    let map = evidence
        .iter()
        .find(|e| e["kind"] == "runtime_project_map")
        .expect("project map evidence");
    // Binary files and the output itself are left out.
    assert_eq!(map["files"], 3, "{map}");
    assert_eq!(map["opened"], 1, "{map}");
    assert_eq!(
        map["directories"]["src/tools/"],
        "document_review.rs(1 line) documentation.rs[read all 2 lines, cited]",
        "{map}"
    );
    assert_eq!(map["directories"]["src/"], "agent.rs tools/", "{map}");
    assert_eq!(map["directories"]["./"], "src/", "{map}");
    // The map follows the saved document and precedes newer observations.
    let position = |kind: &str| evidence.iter().position(|e| e["kind"] == kind).unwrap();
    assert!(position("current_file") < position("runtime_project_map"));
    assert!(position("runtime_project_map") < position("tool_observation"));
    let scope = p["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "S1")
        .expect("scope criterion");
    assert!(
        scope["text"]
            .as_str()
            .unwrap()
            .contains("runtime_project_map"),
        "{scope}"
    );

    // A chat answer is not a source document: no map and no scope criterion.
    let other = tempfile::tempdir().unwrap();
    let mut s = session(other.path());
    write(&mut s, "Conclusion: done\nExample: shown\n");
    review::begin(&mut s, "Saved result.txt").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    assert!(!p.to_string().contains("runtime_project_map"), "{p}");
    assert!(
        p["criteria"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"] != "S1")
    );
}

#[test]
fn a_cited_file_read_only_in_part_is_marked_mostly_unread_for_the_scope_check() {
    // A live UI manual read 120 of App.jsx's 1921 lines. The map said "read
    // 120 lines", so the scope check took the file for covered and approved
    // a manual without the project page and the detail panel. The next run
    // never opened App.jsx, and its path alone did not show those screens.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("frontend/src");
    std::fs::create_dir_all(&src).unwrap();
    let function = |name: &str, lines: usize| {
        let body: String = (1..lines - 1)
            .map(|n| format!("  const v{n} = {n};\n"))
            .collect();
        format!("function {name}() {{\n{body}}}\n")
    };
    let app = [
        function("App", 120),
        function("Projects", 40),
        function("Inspector", 240),
    ]
    .concat();
    std::fs::write(src.join("App.jsx"), app).unwrap();
    let settings = [
        format!("export {}", function("ProjectForm", 30)),
        format!("export default {}", function("Settings", 60)),
    ]
    .concat();
    std::fs::write(src.join("Settings.jsx"), settings).unwrap();
    std::fs::write(src.join("App.test.jsx"), function("renders", 50)).unwrap();
    std::fs::write(src.join("styles.css"), "body { margin: 0; }\n").unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("manual.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..support::compact_config()
        },
    );
    s.add_user("ui 사용 심플 매뉴얼 작성".into());
    s.select_workflow("source_document").unwrap();
    s.active_tools = tools::ToolRegistry::optional_names();
    document_step(
        &mut s,
        "file_read",
        json!({"path":"frontend/src/App.jsx","start_line":121,"max_lines":40}),
    );
    document_step(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# UI\n\nProjects are listed here (frontend/src/App.jsx:121-122).\n"}),
    );
    review::begin(&mut s, "Saved manual.md").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let map = p["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "runtime_project_map")
        .expect("project map evidence");
    // The read file names what it holds beyond the read lines, an unopened
    // code file beside it names its declarations, and a test file and a
    // stylesheet stay bare names.
    assert_eq!(
        map["directories"]["frontend/src/"],
        "App.jsx[read 40 of 400 lines (121-160), mostly unread, cited; unread: App 1-120, Inspector 161-400] \
         App.test.jsx Settings.jsx(90 lines: ProjectForm 1-30, Settings 31-90) styles.css",
        "{map}"
    );
    let note = map["note"].as_str().unwrap();
    assert!(note.contains("mostly unread"), "{note}");
    assert!(note.contains("syntax outline"), "{note}");
    let scope = p["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "S1")
        .expect("scope criterion");
    let scope = scope["text"].as_str().unwrap();
    assert!(scope.contains("files marked mostly unread"), "{scope}");
    assert!(
        scope.contains("unread declarations the map lists"),
        "{scope}"
    );
    assert!(scope.contains("line ranges"), "{scope}");
}

#[test]
fn saved_output_is_reviewed_whole_and_observed_sources_are_rehashed() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("manual.md");
    std::fs::write(dir.path().join("ui.js"), "export const label = '저장';\n").unwrap();
    s.active_tools = tools::ToolRegistry::optional_names();
    let read = ToolCall {
        id: "read-ui".into(),
        name: "file_read".into(),
        arguments: json!({"path":"ui.js"}).to_string(),
    };
    let result = tools::run_call(&mut s, &read);
    assert_eq!(result["status"], "ok", "{result}");
    review::observe(&mut s, &read, &result);
    let body = format!(
        "# 안내\n\n{}\n\n## 끝\n\n마지막 문장.\n",
        "설명 문장입니다. ".repeat(700)
    );
    assert!(body.chars().count() > 6000);
    let write = ToolCall {
        id: "write-manual".into(),
        name: "document_edit".into(),
        arguments: json!({"action":"create","text":body}).to_string(),
    };
    let result = tools::run_call(&mut s, &write);
    assert_eq!(result["status"], "ok", "{result}");
    review::observe(&mut s, &write, &result);
    review::begin(&mut s, "Saved manual.md").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let evidence = p["evidence"].as_array().unwrap();
    let log = &evidence[1];
    assert_eq!(log["observed_sources"]["checked"], 1, "{log}");
    assert_eq!(log["observed_sources"]["unchanged"], 1, "{log}");
    let manual = &evidence[3];
    assert!(
        manual["path"].as_str().unwrap().ends_with("manual.md"),
        "{manual}"
    );
    assert_eq!(manual["truncated"], false);
    assert!(manual["text"].as_str().unwrap().contains("마지막 문장."));
}

#[cfg(unix)]
#[test]
fn an_output_behind_a_symlink_is_one_reviewed_file_and_not_a_changed_source() {
    // A live output in /var/folders was read back as /private/var/folders:
    // the reviewer got the manual twice, and the read-then-edited output was
    // listed as a changed source under a path missing from the write log.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("out"), dir.path().join("link")).unwrap();
    std::fs::write(project.join("ui.js"), "export const label = '저장';\n").unwrap();
    let mut s = session(&project);
    s.project.output = dir.path().join("link/manual.md");
    s.select_workflow("source_document").unwrap();
    s.active_tools = tools::ToolRegistry::optional_names();
    let read = document_step(&mut s, "file_read", json!({"path":"ui.js"}));
    assert_eq!(read["status"], "ok", "{read}");
    let created = document_step(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# 안내\n\n저장 버튼을 누릅니다 (`ui.js:1-1`).\n"}),
    );
    assert_eq!(created["status"], "ok", "{created}");
    let output = s.project.output.display().to_string();
    let reread = document_step(&mut s, "file_read", json!({"path":output}));
    assert_eq!(reread["status"], "ok", "{reread}");
    let current = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let edited = document_step(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":current,"old_text":"저장 버튼","text":"저장 단추"}),
    );
    assert_eq!(edited["status"], "ok", "{edited}");

    review::begin(&mut s, "Saved manual.md").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let resolved = dir
        .path()
        .join("out/manual.md")
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let versions: Vec<&String> = p["file_versions"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|path| path.ends_with("manual.md"))
        .collect();
    assert_eq!(versions, [&resolved], "{p}");
    let evidence = p["evidence"].as_array().unwrap();
    let manuals: Vec<&Value> = evidence
        .iter()
        .filter(|e| {
            e["kind"] == "current_file" && e["path"].as_str().unwrap().ends_with("manual.md")
        })
        .collect();
    assert_eq!(manuals.len(), 1, "{p}");
    assert_eq!(manuals[0]["path"], resolved);
    assert!(manuals[0]["text"].as_str().unwrap().contains("저장 단추"));
    let log = evidence
        .iter()
        .find(|e| e["kind"] == "runtime_write_log")
        .unwrap();
    assert_eq!(log["written_paths"], json!([resolved]), "{log}");
    assert_eq!(log["observed_sources"]["checked"], 1, "{log}");
    assert_eq!(
        log["observed_sources"]["changed_or_unreadable"],
        json!([]),
        "{log}"
    );
    review::finish(&mut s, &verdict(&p, true)).unwrap();
    assert!(review::reviewed_files_unchanged(&s));
}

#[test]
fn a_rejection_binds_only_the_result_it_reviewed() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("manual.md");
    s.active_tools = tools::ToolRegistry::optional_names();
    std::fs::write(dir.path().join("ui.js"), "export const hint = 'ok';\n").unwrap();
    let step = |s: &mut Session, id: &str, name: &str, args: Value| {
        let call = ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        };
        let result = tools::run_call(s, &call);
        assert_eq!(result["status"], "ok", "{result}");
        review::observe(s, &call, &result);
        result
    };
    let created = step(
        &mut s,
        "create",
        "document_edit",
        json!({"action":"create","text":"# Manual\n\nConclusion: done\n"}),
    );
    review::begin(&mut s, "Saved manual.md").unwrap();
    assert_eq!(finish(&mut s, false), None);
    assert!(review::rejected_on_current_result(&s));
    // Re-inspecting the unchanged output is not new evidence.
    step(&mut s, "inspect", "document_inspect", json!({}));
    assert!(review::rejected_on_current_result(&s));
    assert_eq!(
        review::begin(&mut s, "Saved manual.md").unwrap(),
        Gate::Repair
    );
    // A section read, unlike an outline-only query, delivers actual content.
    step(
        &mut s,
        "read-section",
        "document_inspect",
        json!({"section":"# Manual"}),
    );
    assert!(!review::rejected_on_current_result(&s));
    assert_eq!(
        review::begin(&mut s, "Saved manual.md").unwrap(),
        Gate::Review
    );
    assert_eq!(finish(&mut s, false), None);
    // A newly delivered source read is a changed basis for the re-review.
    step(&mut s, "read", "file_read", json!({"path":"ui.js"}));
    assert!(!review::rejected_on_current_result(&s));
    review::begin(&mut s, "Saved manual.md").unwrap();
    assert_eq!(finish(&mut s, false), None);
    assert!(review::rejected_on_current_result(&s));
    // So is a repaired document.
    step(
        &mut s,
        "append",
        "document_edit",
        json!({"action":"append","expected_hash":created["data"]["hash"],"text":"Example: shown\n"}),
    );
    assert!(!review::rejected_on_current_result(&s));
}

#[test]
fn a_rejected_page_names_every_failing_check_and_condition() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion");
    review::begin(&mut s, "Done").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let valid: Value = serde_json::from_str(&verdict(&p, true)).unwrap();
    let ids: Vec<String> = p["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(ids.len() >= 2, "{p}");
    // A live run failed five reviews in a row on one sentence that named
    // ten conditions at once. Each problem now names its check and cause.
    let mut bad = valid.clone();
    bad["checks"][0]["status"] = json!("pass");
    bad["checks"][0]["evidence"] = json!(["E999"]);
    bad["checks"][1]["status"] = json!("unmet");
    bad["checks"][1]["next_action"] = json!("");
    // An over-long reason is clipped, not a problem of its own.
    bad["checks"][1]["reason"] = json!("x".repeat(301));
    let error = review::finish(&mut s, &bad.to_string())
        .unwrap_err()
        .to_string();
    for part in [
        format!(
            r#"checks[0] (id "{}"): status "pass" is not one of met, unmet, unverified; did you mean "met"?"#,
            ids[0]
        ),
        format!(
            r#"checks[0] (id "{}"): evidence ["E999"] are not supplied evidence IDs; copy ids from evidence[].id"#,
            ids[0]
        ),
        format!(
            r#"checks[1] (id "{}"): unmet needs one concrete next_action"#,
            ids[1]
        ),
    ] {
        assert!(error.contains(&part), "{part}\n{error}");
    }
    assert!(!error.contains("characters; at most"), "{error}");
    assert!(
        error.starts_with("completion_review_invalid: 3 problem(s)"),
        "{error}"
    );
    // Missing and unexpected criteria are named by ID.
    let mut wrong = valid.clone();
    wrong["checks"][0]["id"] = json!("R99");
    let error = review::finish(&mut s, &wrong.to_string())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(r#"checks[0] (id "R99"): not a criterion on this page"#),
        "{error}"
    );
    assert!(
        error.contains(&format!(r#"no check for criteria ["{}"]"#, ids[0])),
        "{error}"
    );
    let mut met_with_action = valid.clone();
    met_with_action["checks"][0]["next_action"] = json!("Re-read the file");
    let error = review::finish(&mut s, &met_with_action.to_string())
        .unwrap_err()
        .to_string();
    assert!(error.contains("met takes an empty next_action"), "{error}");
    assert!(!s.completion_review.approved);
    // A placeholder action on a met check is not an action to execute.
    let mut placeholder = valid;
    for check in placeholder["checks"].as_array_mut().unwrap() {
        check["next_action"] = json!("None");
    }
    review::finish(&mut s, &placeholder.to_string()).unwrap();
    assert!(
        s.completion_review
            .checks
            .iter()
            .all(|check| check.next_action.is_empty()),
        "{:?}",
        s.completion_review.checks
    );
}

#[test]
fn a_rejection_stays_the_repair_target_until_the_next_review() {
    // Live run 2026-10-07: the first repair read after a rejection changed
    // the reviewed version, so guidance dropped the unmet checks and the run
    // spent 30 rounds on bookkeeping before the next final answer.
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    std::fs::write(dir.path().join("notes.txt"), "Example: shown\n").unwrap();
    write(&mut s, "Conclusion only\n");
    s.task.unresolved = vec!["The example source was not read".into()];
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    assert!(finish(&mut s, false).is_none());
    assert!(review::rejected_on_current_result(&s));
    assert!(review::repair_open(&s));
    assert!(review::prior_rejection(&s).is_none(), "a current verdict");
    let guidance = review::guidance(&s);
    assert_eq!(guidance["remaining"], 4, "{guidance}");
    assert!(guidance["last_review"].is_null(), "{guidance}");

    // Reading evidence changes the reviewed version, not the open repair.
    let read = ToolCall {
        id: "read".into(),
        name: "file_read".into(),
        arguments: json!({"path":"notes.txt"}).to_string(),
    };
    let result = tools::run_call(&mut s, &read);
    assert_eq!(result["status"], "ok", "{result}");
    review::observe(&mut s, &read, &result);
    assert!(!review::rejected_on_current_result(&s));
    assert!(review::repair_open(&s));
    assert!(review::reviewed_files_unchanged(&s));
    let guidance = review::guidance(&s);
    assert_eq!(guidance["checks"], json!([]), "no verdict on this version");
    let last = &guidance["last_review"];
    assert_eq!(last["result_changed"], true, "{guidance}");
    assert_eq!(last["remaining"], 4, "{guidance}");
    assert_eq!(last["checks"][0]["id"], "R0");
    assert_eq!(
        last["checks"][0]["next_action"],
        "Add the requested example to result.txt"
    );
    assert!(last["note"].as_str().unwrap().contains("repair targets"));
    // A review switched off since then asks for no repair.
    s.config.completion_review_enabled = false;
    assert!(!review::repair_open(&s));
    assert!(review::guidance(&s)["last_review"].is_null());
    s.config.completion_review_enabled = true;

    // Clearing the unresolved list settles only the runtime's own check.
    s.task.unresolved.clear();
    let prior = review::prior_rejection(&s).unwrap();
    assert_eq!(
        prior
            .checks
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["R0", "C1", "C2"]
    );
    // A repaired file is still the same open repair, now with changed files.
    write(&mut s, "Conclusion\nExample: shown\n");
    assert!(review::repair_open(&s));
    assert!(!review::reviewed_files_unchanged(&s));

    // The next review replaces it, and its approval closes it.
    assert_eq!(review::begin(&mut s, "Done").unwrap(), Gate::Review);
    assert!(!review::repair_open(&s));
    assert!(review::guidance(&s)["last_review"].is_null());
    assert_eq!(finish(&mut s, true).as_deref(), Some("Done"));
    assert!(!review::repair_open(&s));
    assert!(review::prior_rejection(&s).is_none());
}

#[test]
fn over_long_check_fields_are_clipped_instead_of_rejected() {
    // Live run 2026-10-07: the single closing completion review was lost
    // because one valid R0 verdict gave a 441-character reason.
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only\n");
    review::begin(&mut s, "Done").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let mut response: Value = serde_json::from_str(&verdict(&p, false)).unwrap();
    let evidence = response["checks"][0]["evidence"][0].clone();
    response["checks"][0]["reason"] = json!("근거 없음 ".repeat(90));
    response["checks"][0]["next_action"] = json!("예시를 추가 ".repeat(40));
    response["checks"][0]["evidence"] = json!(vec![evidence; 11]);
    assert!(
        review::finish(&mut s, &response.to_string())
            .unwrap()
            .is_none()
    );
    let check = &s.completion_review.checks[0];
    assert_eq!(check.status, "unmet");
    assert_eq!(check.reason.chars().count(), 300, "{}", check.reason);
    assert!(check.reason.ends_with('…'));
    assert_eq!(check.next_action.chars().count(), 160);
    assert_eq!(check.evidence.len(), 8);
    assert!(review::rejected_on_current_result(&s));
}
