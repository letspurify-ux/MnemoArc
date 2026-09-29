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
use std::sync::{Arc, Mutex};
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
            source_answer_review: false,
            ..Default::default()
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
    s.task.require_investigation = true;
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
            ..Default::default()
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
            ..Default::default()
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
fn changed_requirements_invalidate_rejection_without_reopening_stale_repairs() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only\n");
    review::begin(&mut s, "Saved result.txt").unwrap();
    assert!(finish(&mut s, false).is_none());
    review::schedule_repairs(&mut s);
    let id = s.task.current_todo().unwrap().id.clone();
    plan(
        &mut s,
        json!([{"op":"complete","id":id,"result":"Only marked done"}]),
    );
    assert!(review::rejected_on_current_result(&s));

    s.request_review_criteria
        .constraints
        .push("Use Korean".into());
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Unreviewed
    );
    assert!(!review::rejected_on_current_result(&s));
    assert_eq!(review::guidance(&s)["needs_review"], true);
    assert_eq!(review::guidance(&s)["remaining"], 0);
    assert_eq!(review::view(&s)["checks"], json!([]));
    review::schedule_repairs(&mut s);
    assert!(s.task.current_todo().is_none());
    assert!(
        !s.completion_review.checks.is_empty(),
        "Keep the historical verdict for diagnostics"
    );
    assert_eq!(
        review::begin(&mut s, "Saved result.txt").unwrap(),
        Gate::Review
    );
    assert!(finish(&mut s, false).is_none());
    assert!(review::rejected_on_current_result(&s));
}

#[test]
fn unavailable_review_binds_only_the_result_it_could_not_review() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only\n");
    review::begin(&mut s, "Saved result.txt").unwrap();
    review::request(&mut s).unwrap();
    review::mark_unavailable(&mut s, Some("Invalid reviewer response".into()));
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Unavailable
    );
    assert_eq!(
        review::begin(&mut s, "Saved result.txt").unwrap(),
        Gate::Unavailable
    );
    assert_eq!(review::view(&s)["unavailable"], true);

    write(&mut s, "Conclusion\nExample: saved\n");
    assert_eq!(
        review::current_verdict(&s),
        review::CurrentVerdict::Unreviewed
    );
    assert_eq!(review::view(&s)["unavailable"], false);
    assert_eq!(review::view(&s)["unavailable_reason"], Value::Null);
    assert_eq!(review::view(&s)["needs_review"], true);
    assert_eq!(
        review::begin(&mut s, "Saved result.txt").unwrap(),
        Gate::Review
    );
    assert!(finish(&mut s, true).is_some());
}

#[test]
fn verified_manual_gets_a_fresh_review_without_a_document_edit() {
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
            ..Default::default()
        },
    );
    s.add_user("ui 사용자 매뉴얼 만들어줘".into());
    s.select_workflow("source_document").unwrap();
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{
            "completion":["모든 조사 항목이 written 상태이며 감사로 구조/인용 오류가 없다"]
        }}),
    )
    .unwrap();
    let read = tools::execute(&mut s, "file_read", json!({"path":"ui.js"})).unwrap();
    let body = (1..=7)
        .map(|i| format!("# Screen {i}\nOpen the chat screen. ui.js:1\n\n"))
        .collect::<String>();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":body}),
    )
    .unwrap();
    for i in 1..=7 {
        tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":format!("T{i}"),
            "title":format!("Screen {i}"),"section":format!("# Screen {i}"),"status":"written"}),
        )
        .unwrap();
        if i > 1 {
            tools::execute(&mut s, "investigation", json!({"action":"verify","id":format!("T{i}"),
                "source_ids":[read["source"]["id"]],"verification_note":"Compared the chat function"})).unwrap();
        }
    }
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
    assert_eq!(
        p["criteria"].as_array().unwrap().len(),
        1,
        "Only the actual user request is a requirement"
    );
    assert_eq!(p["criteria"][0]["id"], "R0");
    assert_eq!(p["evidence"][2]["items"][0]["status"], "written");
    assert_eq!(p["evidence"][2]["items"][1]["status"], "verified");
    let mut response: Value = serde_json::from_str(&verdict(&p, false)).unwrap();
    response["checks"][0]["reason"] = json!("T1 is still written; six other items are verified");
    response["checks"][0]["next_action"] = json!("Verify T1 against the delivered source");
    assert!(
        review::finish(&mut s, &response.to_string())
            .unwrap()
            .is_none()
    );
    assert!(review::rejected_on_current_result(&s));

    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"T1",
        "source_ids":[read["source"]["id"]],"verification_note":"Compared the chat function"}),
    )
    .unwrap();
    assert_eq!(s.last_document_write.as_ref().unwrap().1, saved_hash);
    assert!(
        s.investigations
            .iter()
            .all(|item| item.status == "verified")
    );
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
    assert!(
        p["evidence"][2]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "verified")
    );
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

struct RepairClient {
    root: std::path::PathBuf,
    reviews: Mutex<usize>,
}
#[async_trait]
impl LlmClient for RepairClient {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let p: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if p["completion_review"] == true {
            *self.reviews.lock().unwrap() += 1;
            let saved = std::fs::read_to_string(self.root.join("result.txt"))?;
            return Ok(Completion {
                text: verdict(&p, saved.contains("Example")),
                ..Default::default()
            });
        }
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        let current = &state["run_guidance"]["current_todo"];
        let saved = std::fs::read_to_string(self.root.join("result.txt"))?;
        let call = if current.is_null() {
            return Ok(Completion {
                text: "Done".into(),
                ..Default::default()
            });
        } else if !saved.contains("Example") {
            ToolCall {id:"repair-file".into(),name:"file_write".into(),arguments:json!({"path":"result.txt","expected_hash":tools::hash(saved.as_bytes()),"content":"Conclusion\nExample: saved outcome\n"}).to_string()}
        } else {
            ToolCall {id:format!("finish-{}",state["task"]["plan_revision"]),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":current["id"],"result":"Added and checked the saved example"}]}).to_string()}
        };
        Ok(Completion {
            calls: vec![call],
            ..Default::default()
        })
    }
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

#[tokio::test]
async fn all_todos_done_but_missing_criterion_repairs_and_verifies_before_final() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion\n");
    plan(
        &mut s,
        json!([{"op":"insert","texts":["Write requested output"]},{"op":"complete","id":"T1","result":"Saved result.txt"}]),
    );
    let client = Arc::new(RepairClient {
        root: dir.path().into(),
        reviews: Mutex::new(0),
    });
    let (result, events) = run(s, client.clone()).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.completion_review.approved);
    assert_eq!(*client.reviews.lock().unwrap(), 2);
    assert!(result.task.current_todo().is_none());
    assert_eq!(result.task.todos_completed_total, 2);
    assert!(
        std::fs::read_to_string(dir.path().join("result.txt"))
            .unwrap()
            .contains("Example")
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e,AgentEvent::Delta{text,..} if text=="Done"))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .filter_map(|e| if let AgentEvent::Snapshot(s) = e {
                Some(s)
            } else {
                None
            })
            .filter(|s| s.status == "complete")
            .all(|s| s.completion_review.approved)
    );
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
#[tokio::test]
async fn invalid_review_is_partial_and_resume_keeps_the_candidate_and_requirements() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion and Example\n");
    let (mut result, events) = run(s, Arc::new(InvalidClient)).await;
    assert_eq!(result.status, "partial");
    assert!(result.completion_review.pending);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e,AgentEvent::Delta{text,..} if text=="Done"))
    );
    result.add_user("resume".into());
    let client = Arc::new(RepairClient {
        root: dir.path().into(),
        reviews: Mutex::new(0),
    });
    let (result, _) = run(result, client).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.completion_review.approved);
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

struct ChurnClient(Mutex<usize>);
#[async_trait]
impl LlmClient for ChurnClient {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let p: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if p["completion_review"] == true {
            *self.0.lock().unwrap() += 1;
            return Ok(Completion {
                text: verdict(&p, false),
                ..Default::default()
            });
        }
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        let current = &state["run_guidance"]["current_todo"];
        if current.is_null() {
            return Ok(Completion {
                text: "Done".into(),
                ..Default::default()
            });
        }
        Ok(Completion { calls:vec![ToolCall {id:format!("mark-{}",state["task"]["plan_revision"]),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":current["id"],"result":"Claimed completion without fixing result"}]}).to_string()}], ..Default::default() })
    }
}

#[tokio::test]
async fn unchanged_rejection_cannot_loop_reviews_or_publish_success() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only");
    let client = Arc::new(ChurnClient(Mutex::new(0)));
    let (result, events) = run(s, client.clone()).await;
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert!(
        result
            .last_error
            .unwrap()
            .starts_with("completion_review_no_progress")
    );
    assert_eq!(*client.0.lock().unwrap(), 1);
    assert_eq!(result.task.todos.len(), 1);
    assert!(result.task.current_todo().is_some());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e,AgentEvent::Delta{text,..} if text=="Done"))
    );
}

struct RepeatingReadClient {
    reviews: Mutex<usize>,
    calls: Mutex<usize>,
}
#[async_trait]
impl LlmClient for RepeatingReadClient {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let p: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if p["completion_review"] == true {
            *self.reviews.lock().unwrap() += 1;
            return Ok(Completion {
                text: verdict(&p, false),
                ..Default::default()
            });
        }
        if *self.reviews.lock().unwrap() == 0 {
            return Ok(Completion {
                text: "Done".into(),
                ..Default::default()
            });
        }
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let (name, arguments) = if let Some(id) = state["checkpoint"]["id"].as_str() {
            (
                "checkpoint_complete",
                json!({"id":id,"progress":"The rejected completion review remains unresolved.","no_save_reason":"The review checks already preserve the missing requirement."}),
            )
        } else if let Some(id) = state["run_guidance"]["current_todo"]["id"].as_str() {
            (
                "task_plan",
                json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":id,"result":"Repeated the existing check"}]}),
            )
        } else {
            (
                "file_read",
                json!({"path":"result.txt","start_line":1,"max_lines":1,"force_read":true}),
            )
        };
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("repeat-{}", *calls),
                name: name.into(),
                arguments: arguments.to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn rejected_review_bounds_tool_only_repair_loop_and_keeps_work_for_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only");
    let client = Arc::new(RepeatingReadClient {
        reviews: Mutex::new(0),
        calls: Mutex::new(0),
    });
    let (result, events) = run(s, client.clone()).await;
    assert_eq!(
        result.status,
        "partial",
        "error={:?}, calls={}, reviews={}",
        result.last_error,
        *client.calls.lock().unwrap(),
        *client.reviews.lock().unwrap()
    );
    assert!(
        result
            .last_error
            .as_deref()
            .unwrap()
            .starts_with("completion_review_no_progress")
    );
    assert_eq!(*client.reviews.lock().unwrap(), 1);
    assert!(*client.calls.lock().unwrap() <= 42);
    assert!(
        result.task.current_todo().is_none(),
        "A new read invalidated the review; do not reopen its completed repair from a stale verdict"
    );
    assert_eq!(review::guidance(&result)["needs_review"], true);
    assert!(!result.completion_review.checks.is_empty());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Delta { text, .. } if text == "Done"))
    );
}

struct ChangingAnswerClient(Mutex<usize>);
#[async_trait]
impl LlmClient for ChangingAnswerClient {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let p: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap_or(""))
                .unwrap_or(Value::Null);
        if p["completion_review"] == true {
            *self.0.lock().unwrap() += 1;
            return Ok(Completion {
                text: verdict(&p, false),
                ..Default::default()
            });
        }
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )?;
        if let Some(id) = state["run_guidance"]["current_todo"]["id"].as_str() {
            return Ok(Completion {
                calls: vec![ToolCall {
                    id: format!("complete-{id}-{}", *self.0.lock().unwrap()),
                    name: "task_plan".into(),
                    arguments: json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":id,"result":"No actual result changed"}]}).to_string(),
                }],
                ..Default::default()
            });
        }
        Ok(Completion {
            text: format!("Done, review {}", *self.0.lock().unwrap()),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn changing_final_words_do_not_reset_rejected_review_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    write(&mut s, "Conclusion only");
    let client = Arc::new(ChangingAnswerClient(Mutex::new(0)));
    let (result, events) = run(s, client.clone()).await;
    assert_eq!(result.status, "partial", "{:?}", result.last_error);
    assert_eq!(*client.0.lock().unwrap(), 6);
    assert_eq!(result.completion_review.stalled_reviews, 6);
    assert!(result.task.current_todo().is_some());
    assert!(!result.completion_review.approved);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Delta { text, .. } if text.starts_with("Done")))
    );
    let (recovered, _) = run(
        result,
        Arc::new(RepairClient {
            root: dir.path().into(),
            reviews: Mutex::new(0),
        }),
    )
    .await;
    assert_eq!(recovered.status, "complete", "{:?}", recovered.last_error);
    assert!(recovered.completion_review.approved);
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
        assert!(result.answer_draft.is_none());
        assert!(
            events
                .iter()
                .any(|e| matches!(e,AgentEvent::Delta{text,..} if text=="Verified final answer"))
        );
    }
}

struct ContinuedAnswer(Mutex<usize>);
#[async_trait]
impl LlmClient for ContinuedAnswer {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        let mut step = self.0.lock().unwrap();
        let response = match *step {
            0 => Completion {
                text: "```mermaid\nflowchart LR\n A -->".into(),
                length_limited: true,
                ..Default::default()
            },
            1 => Completion {
                text: " B\n```".into(),
                ..Default::default()
            },
            2 => {
                let p = payload(&request);
                assert_eq!(p["completion_review"], true);
                assert_eq!(
                    p["evidence"][0]["text"],
                    "```mermaid\nflowchart LR\n A --> B\n```"
                );
                assert_eq!(p["evidence"][0]["truncated"], false);
                Completion {
                    text: verdict(&p, true),
                    ..Default::default()
                }
            }
            _ => panic!("unexpected continuation/review loop"),
        };
        *step += 1;
        Ok(response)
    }
}
#[tokio::test]
async fn planned_answer_continuation_reviews_whole_answer_and_keeps_join_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("Draw a flowchart in the final answer".into());
    plan(
        &mut s,
        json!([{"op":"insert","texts":["Prepare diagram"]},{"op":"complete","id":"T1","result":"Prepared diagram"}]),
    );
    let (result, _) = run(s, Arc::new(ContinuedAnswer(Mutex::new(0)))).await;
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    let messages = result.history.active();
    let final_message = messages.last().unwrap();
    assert_eq!(final_message["continues_previous"], true);
    assert_eq!(final_message["content"], " B\n```");
    assert!(result.completion_review.approved);
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
            .contains("runtime_write_log and runtime_investigations are runtime records")
    );
    let p = payload(&request);
    let evidence = p["evidence"].as_array().unwrap();
    assert_eq!(evidence[1]["kind"], "runtime_write_log", "{p}");
    let written = evidence[1]["written_paths"].as_array().unwrap();
    assert_eq!(written.len(), 1, "{p}");
    assert!(written[0].as_str().unwrap().ends_with("result.txt"));
    // A read is an observation, not a write.
    assert!(!evidence[1].to_string().contains("source.rs"));
    assert_eq!(evidence[2]["kind"], "runtime_investigations", "{p}");
    assert_eq!(evidence[3]["kind"], "current_file", "{p}");
    assert_eq!(evidence[4]["kind"], "tool_observation", "{p}");
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
fn investigation_status_is_supplied_as_runtime_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.active_tools.insert("investigation".into());
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"flow","title":"Main flow"}),
    )
    .unwrap();
    write(&mut s, "Conclusion: done\nExample: shown\n");
    review::begin(&mut s, "Saved result.txt").unwrap();
    let p = payload(&review::request(&mut s).unwrap());
    let items = &p["evidence"][2];
    assert_eq!(items["kind"], "runtime_investigations", "{p}");
    assert_eq!(items["items"][0]["id"], "flow");
    assert_eq!(items["items"][0]["status"], s.investigations[0].status);
}
