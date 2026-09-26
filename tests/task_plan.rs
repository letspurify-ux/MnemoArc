mod support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::run_session,
    config::{Config, Project},
    context::ContextManager,
    llm::{Completion, LlmClient, ToolCall},
    session::{Session, TaskState},
    tools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn session(root: &std::path::Path) -> Session {
    Session::new(
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
    )
}

fn apply(s: &mut Session, operations: Value) -> Value {
    tools::execute(
        s,
        "task_plan",
        json!({"action":"apply","expected_revision":s.task.plan_revision,"operations":operations}),
    )
    .unwrap()
}

#[test]
fn operation_encodings_are_normalized_and_replayed_without_duplicate_items() {
    let operation = json!({"op":"insert","texts":["조사한 섹션 저장"]});
    for operations in [
        json!([operation]),
        json!(json!([operation]).to_string()),
        operation.clone(),
        json!(operation.to_string()),
        json!([operation.to_string()]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let call = ToolCall {
            id: "create-plan".into(),
            name: "task_plan".into(),
            arguments: json!({"action":"apply","expected_revision":0,"operations":operations})
                .to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        assert_eq!(result["status"], "ok", "{result}");
        assert_eq!(result["data"]["applied"], true, "{result}");
        assert_eq!(
            result["data"]["input_normalized"],
            !operations.is_array() || operations[0].is_string()
        );
        assert_eq!(s.task.current_todo().unwrap().text, "조사한 섹션 저장");
        assert_eq!(tools::run_call(&mut s, &call), result);
        assert_eq!(s.task.todos.len(), 1);
        assert_eq!(s.task.plan_revision, 1);
    }
}

#[test]
fn invalid_operation_shapes_preserve_plan_without_poisoning_error_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.status = "running".into();
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Continue actual work"]}]),
    );
    let original = json!(s.task);
    let mut failures = tools::recovery::FailureTracker::default();
    for (i, operations) in [
        Value::Null,
        json!(true),
        json!(42),
        json!("[unfinished JSON"),
        json!({"T1":{"op":"complete","result":"Ambiguous keyed operation"}}),
        json!([{"op":"remove","id":"T1","reason":"Must remain atomic"}, false]),
    ]
    .into_iter()
    .enumerate()
    {
        let call = ToolCall {
            id: format!("invalid-{i}"), name: "task_plan".into(),
            arguments: json!({"action":"apply","expected_revision":s.task.plan_revision,"operations":operations}).to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        assert_eq!(result["status"], "ok", "{result}");
        assert_eq!(result["data"]["applied"], false);
        assert_eq!(result["data"]["input_error"]["field"], "operations");
        assert!(
            failures
                .observe("task_plan", &call.arguments, &result, 2)
                .is_none()
        );
        assert_eq!(json!(s.task), original);
        assert_eq!(s.status, "running");
        assert!(!s.ledger.contains_key(&call.id));
    }
    let corrected = tools::run_call(&mut s, &ToolCall {
        id: "invalid-0".into(), name: "task_plan".into(),
        arguments: json!({"action":"apply","expected_revision":1,"operations":{"op":"insert","texts":["Follow-up work"]}}).to_string(),
    });
    assert_eq!(corrected["data"]["applied"], true, "{corrected}");
    assert_eq!(s.task.todos.len(), 2);
}

#[test]
fn normalized_inputs_still_enforce_revision_order_bounds_and_atomicity() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    apply(
        &mut s,
        json!([{"op":"insert","texts":["First outcome","Second outcome"]}]),
    );
    let original = json!(s.task);
    let stale = tools::execute(&mut s, "task_plan", json!({"action":"apply","expected_revision":0,"operations":json!({"op":"complete","id":"T1","result":"Stale result"}).to_string()})).unwrap();
    assert_eq!(stale["applied"], false);
    for operations in [
        json!(json!({"op":"complete","id":"T2","result":"Cannot skip current"}).to_string()),
        json!(
            json!([
                {"op":"remove","id":"T1","reason":"Do not commit part of a batch"},
                {"op":"insert","texts":["Too long".repeat(30)]}
            ])
            .to_string()
        ),
        json!({"op":"insert","texts":["Must reject extra fields"],"done":true}),
        json!(json!(vec![json!({"op":"move","id":"T1"}); 17]).to_string()),
        json!([{"op":"insert","texts":(0..101).map(|i|format!("Overflow {i}")).collect::<Vec<_>>()}]),
        json!([]),
        json!(" ".repeat(128 * 1024 + 1)),
    ] {
        let refused = apply(&mut s, operations);
        assert_eq!(refused["applied"], false, "{refused}");
        assert_eq!(json!(s.task), original);
    }
}

#[test]
fn small_results_keep_the_operation_correction_visible() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.state_tokens = 12000;
    let descriptions: Vec<_> = (0..8)
        .map(|i| {
            format!(
                "{i}: {}",
                "확인한 근거를 바탕으로 해당 섹션을 작성하고 결과를 검증합니다. ".repeat(3)
            )
        })
        .collect();
    assert_eq!(
        apply(&mut s, json!([{"op":"insert","texts":descriptions}]))["applied"],
        true
    );
    s.config.result_tokens = 200;
    let call = ToolCall {
        id: "bad-shape".into(),
        name: "task_plan".into(),
        arguments: json!({"action":"apply","expected_revision":1,"operations":false}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["data"]["applied"], false, "{result}");
    assert_eq!(result["data"]["input_error"]["field"], "operations");
    assert_eq!(
        result["data"]["input_error"]["expected"],
        "array of operation objects"
    );
    assert!(tools::result_tokens(&call, &result, &s.config.model) <= 200);
    assert_eq!(s.task.todos.len(), 8);
    assert!(!s.ledger.contains_key(&call.id));
}

#[test]
fn small_result_budget_preserves_successful_plan_revision() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.result_tokens = 200;
    let call = ToolCall {
        id: "long-success".into(),
        name: "task_plan".into(),
        arguments: json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["세부 작업을 완료하고 결과를 저장합니다. ".repeat(6)]}]}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["data"]["applied"], true, "{result}");
    assert_eq!(result["data"]["plan"]["revision"], 1, "{result}");
    assert!(tools::result_tokens(&call, &result, &s.config.model) <= 200);
    assert_eq!(s.task.todos.len(), 1);
}

#[test]
fn prerequisites_insert_move_remove_and_complete_in_order_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.task.completion = vec!["Write and verify requested output".into()];
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Write section","Verify output","Unused work"]}]),
    );
    let write = s.task.todos[0].id.clone();
    let verify = s.task.todos[1].id.clone();
    let unused = s.task.todos[2].id.clone();
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Read missing declaration"],"before":write}]),
    );
    let prerequisite = s.task.current_todo().unwrap().id.clone();
    let before = serde_json::to_value(&s.task).unwrap();
    let refused = apply(
        &mut s,
        json!([
            {"op":"remove","id":unused,"reason":"Outside requested scope"},
            {"op":"complete","id":verify,"result":"Tried to skip the prerequisite"}
        ]),
    );
    assert_eq!(refused["applied"], false);
    assert_eq!(serde_json::to_value(&s.task).unwrap(), before);
    apply(
        &mut s,
        json!([
            {"op":"move","id":unused,"before":write},
            {"op":"remove","id":unused,"reason":"Outside requested scope"},
            {"op":"complete","id":prerequisite,"result":"Read the exact declaration"},
            {"op":"complete","id":write,"result":"Saved the section"}
        ]),
    );
    assert_eq!(s.task.current_todo().unwrap().id, verify);
    assert_eq!(s.task.completion, ["Write and verify requested output"]);
    apply(
        &mut s,
        json!([{"op":"reopen","id":write,"reason":"Review found a factual error"}]),
    );
    assert_eq!(s.task.current_todo().unwrap().id, write);
    assert!(
        !s.task
            .todos
            .iter()
            .find(|item| item.id == write)
            .unwrap()
            .done
    );
}

#[test]
fn split_refines_current_and_later_items_without_losing_the_original_goal() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.task.completion = vec!["Produce and review the requested report".into()];
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Produce report","Publish result"]}]),
    );
    let split = tools::run_call(
        &mut s,
        &ToolCall {
            id: "split-current".into(),
            name: "task_plan".into(),
            arguments: json!({"action":"apply","expected_revision":1,"operations":{"op":"split","id":"T1","texts":["Read required source","Write report","Review report"]}}).to_string(),
        },
    );
    assert_eq!(split["status"], "ok", "{split}");
    assert_eq!(split["data"]["applied"], true, "{split}");
    assert_eq!(split["data"]["input_normalized"], true);
    assert_eq!(s.task.current_todo().unwrap().id, "T1");
    assert_eq!(
        s.task
            .todos
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["T1", "T3", "T4", "T2"]
    );
    assert_eq!(s.task.todos[0].text, "Read required source");
    assert_eq!(
        s.task.completion,
        ["Produce and review the requested report"]
    );

    let original = json!(s.task);
    for operations in [
        json!([{"op":"split","id":"T1","texts":["Only one part"]}]),
        json!([{"op":"split","id":"T1","texts":["Same part","Same part"]}]),
        json!([{"op":"split","id":"T1","texts":["Read required source","New part"]}]),
        json!([{"op":"split","id":"T1","texts":["New part","Publish result"]}]),
        json!([
            {"op":"split","id":"T1","texts":["Inspect source","Save findings"]},
            {"op":"complete","id":"T2","result":"Cannot skip current"}
        ]),
    ] {
        assert_eq!(apply(&mut s, operations)["applied"], false);
        assert_eq!(json!(s.task), original);
    }

    apply(
        &mut s,
        json!([{"op":"complete","id":"T1","result":"Read the source"}]),
    );
    assert_eq!(s.task.current_todo().unwrap().id, "T3");
    let later = apply(
        &mut s,
        json!([{"op":"split","id":"T2","texts":["Prepare release","Send result"]}]),
    );
    assert_eq!(later["applied"], true, "{later}");
    assert_eq!(
        s.task
            .todos
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["T1", "T3", "T4", "T2", "T5"]
    );
    assert_eq!(s.task.current_todo().unwrap().id, "T3");
    let completed = apply(
        &mut s,
        json!([{"op":"split","id":"T1","texts":["Reread source","Check source"]}]),
    );
    assert_eq!(completed["applied"], false);
    assert_eq!(s.task.todos.len(), 5);
}

#[test]
fn split_at_capacity_is_recoverable_and_can_share_a_batch_with_removal() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.status = "running".into();
    let texts = (1..=100)
        .map(|i| format!("Outcome {i}"))
        .collect::<Vec<_>>();
    assert_eq!(
        apply(&mut s, json!([{"op":"insert","texts":texts}]))["applied"],
        true
    );
    let original = json!(s.task);
    let refused = apply(
        &mut s,
        json!([{"op":"split","id":"T1","texts":["Inspect first outcome","Finish first outcome"]}]),
    );
    assert_eq!(refused["applied"], false);
    assert_eq!(json!(s.task), original);
    assert_eq!(s.status, "running");
    let applied = apply(
        &mut s,
        json!([
            {"op":"remove","id":"T100","reason":"No longer needed"},
            {"op":"split","id":"T1","texts":["Inspect first outcome","Finish first outcome"]}
        ]),
    );
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(s.task.todos.iter().filter(|item| !item.done).count(), 100);
    assert_eq!(s.task.todos[0].id, "T1");
    assert_eq!(s.task.todos[1].id, "T101");
    assert_eq!(s.task.current_todo().unwrap().text, "Inspect first outcome");
}

#[test]
fn full_plan_and_stale_revision_do_not_fail_or_lose_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.status = "running".into();
    let texts: Vec<_> = (1..=100)
        .map(|i| {
            format!(
                "Outcome {i}: {}",
                "작업 결과를 확인하고 저장합니다. ".repeat(5)
            )
        })
        .collect();
    assert_eq!(
        apply(&mut s, json!([{"op":"insert","texts":texts}]))["applied"],
        true
    );
    let state = ContextManager::state(&s).unwrap();
    assert_eq!(state["task"]["todo_window"]["pending_count"], 100);
    assert!(state["task"]["todos"].as_array().unwrap().len() <= 6);
    assert_eq!(s.task.todos.len(), 100);
    let mut offset = 0;
    let mut listed = Vec::new();
    loop {
        let page = tools::execute(
            &mut s,
            "task_plan",
            json!({"action":"list","offset":offset,"limit":20}),
        )
        .unwrap();
        assert_eq!(page["pending_count"], 100);
        assert_eq!(page["total_items"], 100);
        listed.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_string()),
        );
        if let Some(next) = page["next_offset"].as_u64() {
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(listed.len(), 100);
    assert_eq!(listed.first().unwrap(), "T1");
    assert_eq!(listed.last().unwrap(), "T100");
    s.config.result_tokens = 1000;
    let page_call = ToolCall {
        id: "bounded-plan-page".into(),
        name: "task_plan".into(),
        arguments: json!({"action":"list","offset":0,"limit":20}).to_string(),
    };
    let first_page = tools::run_call(&mut s, &page_call);
    assert_eq!(first_page["status"], "ok", "{first_page}");
    let shown = first_page["data"]["items"].as_array().unwrap().len();
    assert!(shown > 0 && shown < 20, "{first_page}");
    assert_eq!(first_page["data"]["next_offset"], shown);
    let next_page = tools::run_call(
        &mut s,
        &ToolCall {
            id: "bounded-plan-next-page".into(),
            name: "task_plan".into(),
            arguments: json!({"action":"list","offset":shown,"limit":20}).to_string(),
        },
    );
    assert_eq!(
        next_page["data"]["items"][0]["id"],
        format!("T{}", shown + 1)
    );
    let original = s.task.todos.clone();
    let refused = apply(&mut s, json!([{"op":"insert","texts":["Later work"]}]));
    assert_eq!(refused["applied"], false);
    assert_eq!(s.task.todos, original);
    assert_eq!(s.status, "running");
    let stale = tools::execute(&mut s, "task_plan", json!({"action":"apply","expected_revision":0,"operations":[{"op":"remove","id":"T1","reason":"Stale plan"}]})).unwrap();
    assert_eq!(stale["applied"], false);
    apply(
        &mut s,
        json!([
            {"op":"complete","id":"T1","result":"Produced outcome 1"},
            {"op":"insert","texts":["Later work"]}
        ]),
    );
    assert_eq!(s.task.todos.iter().filter(|item| !item.done).count(), 100);
    assert_eq!(s.task.current_todo().unwrap().id, "T2");
    assert_eq!(
        ContextManager::state(&s).unwrap()["task"]["todo_window"]["pending_count"],
        100
    );
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"findings":["The first outcome is saved"]}}),
    )
    .unwrap();
    s.add_user("계속".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let checkpoint_id = s.checkpoint.as_ref().unwrap().id.clone();
    tools::execute(&mut s,"checkpoint_complete",json!({"id":checkpoint_id,"progress":"Continue with the current outcome","no_save_reason":"No new reusable facts"})).unwrap();
    assert_eq!(s.task.todos.iter().filter(|item| !item.done).count(), 100);
}

#[test]
fn completed_history_is_bounded_and_checkpoint_preserves_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    for i in 0..12 {
        apply(
            &mut s,
            json!([{"op":"insert","texts":[format!("Outcome {i}")]}]),
        );
        let id = s.task.current_todo().unwrap().id.clone();
        apply(
            &mut s,
            json!([{"op":"complete","id":id,"result":format!("Saved result {i}")} ]),
        );
    }
    assert_eq!(s.task.todos.len(), 5);
    assert_eq!(s.task.todos_completed_total, 12);
    apply(&mut s, json!([{"op":"insert","texts":["Continue work"]}]));
    let plan = s.task.todos.clone();
    s.add_user("Continue current plan".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let id = s.checkpoint.as_ref().unwrap().id.clone();
    let plan_revision = s.task.plan_revision;
    let invalid = tools::run_call(
        &mut s,
        &ToolCall {
            id: "bad-maintenance-plan".into(),
            name: "task_plan".into(),
            arguments:
                json!({"action":"apply","expected_revision":plan_revision,"operations":false})
                    .to_string(),
        },
    );
    assert_eq!(invalid["status"], "ok");
    assert_eq!(invalid["data"]["applied"], false);
    assert!(!s.checkpoint.as_ref().unwrap().failed);
    tools::execute(&mut s,"checkpoint_complete",json!({"id":id,"progress":"Necessary facts are stored","next":"Legacy summary must not overwrite the list","no_save_reason":"No new facts"})).unwrap();
    ContextManager::commit(&mut s).unwrap();
    assert_eq!(s.task.todos, plan);
    s.add_user("계속".into());
    assert_eq!(s.task.todos, plan);
    s.add_user("A new task".into());
    assert!(s.task.todos.is_empty());
    assert_eq!(s.task.todos_completed_total, 0);
}

#[test]
fn legacy_progress_is_migrated_once_and_no_longer_serialized() {
    let mut task: TaskState = serde_json::from_value(
        json!({"current":"Read source","next":"Write section","done":["Located entry"]}),
    )
    .unwrap();
    task.migrate_legacy_plan();
    let once = task.todos.clone();
    task.migrate_legacy_plan();
    assert_eq!(task.todos, once);
    assert_eq!(task.current_todo().unwrap().text, "Write section");
    let serialized = json!(task);
    assert!(serialized.get("current").is_none());
    assert!(serialized.get("next").is_none());
    assert!(serialized.get("done").is_none());
}

struct PlanChurn(Mutex<usize>);
#[async_trait]
impl LlmClient for PlanChurn {
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
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        let mut step = self.0.lock().unwrap();
        let plan_revision = state["task"]["plan_revision"].clone();
        let id = state["run_guidance"]["current_todo"]["id"].clone();
        let calls = if *step < 3 {
            vec![ToolCall { id:format!("plan-{step}"),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":plan_revision,"operations":[{"op":"update","id":id,"text":format!("Save section, plan revision {step}")}]}).to_string() }]
        } else if *step == 3 {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
            vec![ToolCall {
                id: "write".into(),
                name: "document_edit".into(),
                arguments:
                    json!({"action":"create","text":"# Result\nSaved the requested section.\n"})
                        .to_string(),
            }]
        } else if (4..=12).contains(&*step) {
            if *step == 4 {
                assert_eq!(state["run_guidance"]["progress_recovery"]["active"], false);
            }
            // Even more than the evidence retry allowance must not stop the
            // task just because the plan has not been marked complete yet.
            *step += 1;
            return Ok(Completion {
                text: "Premature completion".into(),
                ..Default::default()
            });
        } else if *step == 13 {
            assert!(
                state["run_guidance"]["completion_error"]
                    .as_str()
                    .unwrap()
                    .starts_with("task_plan_pending")
            );
            vec![ToolCall { id:"finish-item".into(),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":plan_revision,"operations":[{"op":"complete","id":id,"result":"Saved docs/source-summary.md"}]}).to_string() }]
        } else if *step == 14 {
            // Advancing the list must discard the previous item's pending
            // warning, otherwise the next request can repeat completed work.
            assert!(state["run_guidance"]["completion_error"].is_null());
            assert_eq!(
                state["run_guidance"]["current_todo"]["text"],
                "Confirm the saved result"
            );
            vec![ToolCall { id:"finish-confirmation".into(),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":plan_revision,"operations":[{"op":"complete","id":id,"result":"Confirmed the successful save result from document_edit"}]}).to_string() }]
        } else {
            *step += 1;
            return Ok(Completion {
                text: "Saved the completed document.".into(),
                ..Default::default()
            });
        };
        *step += 1;
        Ok(Completion {
            calls,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn plan_churn_cannot_reset_recovery_and_pending_items_resume_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.stall_round_limit = 3;
    s.task.workflow = "document_edit".into();
    s.active_tools.insert("document_edit".into());
    s.add_user("Write the section".into());
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Save the requested section", "Confirm the saved result"]}]),
    );
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let result = run_session(
        s,
        Arc::new(PlanChurn(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    let events = drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.task.current_todo().is_none());
    assert_eq!(result.task.todos_completed_total, 2);
    assert!(dir.path().join("docs/source-summary.md").exists());
    assert!(!events.iter().any(|event|matches!(event,mnemoarc::agent::AgentEvent::Delta{text,..} if text == "Premature completion")));
}

struct EmptyPlanChurn(Mutex<usize>);
#[async_trait]
impl LlmClient for EmptyPlanChurn {
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
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        let mut step = self.0.lock().unwrap();
        if *step == 3 {
            assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
            assert_eq!(
                state["run_guidance"]["progress_recovery"]["rounds_without_progress"],
                3
            );
            return Ok(Completion {
                text: "Answer from existing information.".into(),
                ..Default::default()
            });
        }
        let id = format!("T{}", *step + 1);
        *step += 1;
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("churn-{step}"), name: "task_plan".into(),
                arguments: json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[
                    {"op":"insert","texts":["Rephrase existing evidence"]},
                    {"op":"complete","id":id,"result":"Updated the wording"},
                    {"op":"remove","id":id,"reason":"Remove temporary plan entry"}
                ]}).to_string(),
            }], ..Default::default()
        })
    }
}

#[tokio::test]
async fn completing_and_removing_items_does_not_masquerade_as_actual_progress() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.stall_round_limit = 3;
    s.add_user("Explain the known information".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        s,
        Arc::new(EmptyPlanChurn(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert!(result.task.todos.is_empty());
    assert_eq!(result.task.todos_completed_total, 3);
}

struct InvalidPlanRecovery(Mutex<usize>);
#[async_trait]
impl LlmClient for InvalidPlanRecovery {
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
        let state: Value = serde_json::from_str(
            request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .split_once('\n')
                .unwrap()
                .1,
        )
        .unwrap();
        let mut step = self.0.lock().unwrap();
        let (name, args) = match *step {
            0..=2 => (
                "task_plan",
                json!({"action":"apply","expected_revision":0,"operations":false}),
            ),
            3 => {
                assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
                assert_eq!(state["task"]["plan_revision"], 0);
                (
                    "task_plan",
                    json!({"action":"apply","expected_revision":0,"operations":json!([{"op":"insert","texts":["Write the output"]}]).to_string()}),
                )
            }
            4 => {
                assert_eq!(state["run_guidance"]["progress_recovery"]["active"], true);
                (
                    "file_write",
                    json!({"path":"note.txt","content":"Actual requested result\n"}),
                )
            }
            5 => {
                assert_eq!(state["run_guidance"]["progress_recovery"]["active"], false);
                (
                    "task_plan",
                    json!({"action":"apply","expected_revision":1,"operations":{"op":"complete","id":"T1","result":"Saved note.txt"}}),
                )
            }
            _ => {
                return Ok(Completion {
                    text: "Saved note.txt".into(),
                    ..Default::default()
                });
            }
        };
        *step += 1;
        Ok(Completion {
            calls: vec![ToolCall {
                id: format!("step-{step}"),
                name: name.into(),
                arguments: args.to_string(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn invalid_operations_recover_even_before_the_first_plan_exists() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.stall_round_limit = 3;
    s.add_user("Write the requested output".into());
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_session(
        s,
        Arc::new(InvalidPlanRecovery(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(result.status, "complete", "{:?}", result.last_error);
    assert_eq!(result.task.todos_completed_total, 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "Actual requested result\n"
    );
}

#[test]
fn invalid_json_text_shows_where_it_broke() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // The live shape: operations as hand-written JSON text with Korean
    // content and the closing bracket of texts missing.
    let text =
        r#"[{"op":"insert","texts":["조사: 첫 화면 (App.jsx)"},{"op":"insert","texts":["b"]}]"#;
    let result = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":text}),
    )
    .unwrap();
    assert_eq!(result["applied"], false, "{result}");
    let reason = result["reason"].as_str().unwrap();
    assert!(
        reason.contains(r#"[\"조사: 첫 화면 (App.jsx)\"" <here> "},{"#),
        "{reason}"
    );
    assert!(reason.contains("not as quoted text"), "{reason}");
}

#[test]
fn a_flattened_operation_is_applied_as_one_operation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    apply(&mut s, json!([{"op":"insert","texts":["첫 화면 확인"]}]));
    // The live shape: the operation's fields at the top level.
    let result = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"complete","expected_revision":1,"id":"T1","result":"App.jsx 확인"}),
    )
    .unwrap();
    assert_eq!(result["applied"], true, "{result}");
    assert!(s.task.current_todo().is_none());
    // Unknown fields still reach normal validation.
    let error = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"insert","expected_revision":2,"texts":["x"],"note":"y"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("unknown_argument: note"), "{error}");
}

#[test]
fn a_failed_operation_in_a_batch_is_named_with_the_current_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    apply(
        &mut s,
        json!([{"op":"insert","texts":["a","b","c","d","e"]}]),
    );
    // The live shape: completing T1 and then a later item that is not
    // current once T1 is done.
    let result = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":1,"operations":[
            {"op":"complete","id":"T1","result":"done"},
            {"op":"complete","id":"T5","result":"done"}
        ]}),
    )
    .unwrap();
    assert_eq!(result["applied"], false);
    let reason = result["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("operations[1] failed: Complete the current item first"),
        "{reason}"
    );
    assert!(
        reason.contains("current item at that point: T2"),
        "{reason}"
    );
    assert!(
        reason.contains("T5 can complete only after T2, T3, T4 are completed or removed"),
        "{reason}"
    );
    assert!(
        reason.contains("No operation in this batch was applied"),
        "{reason}"
    );
    assert_eq!(s.task.current_todo().unwrap().id, "T1");
}

#[test]
fn a_bare_apply_names_the_missing_revision_and_operations() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    apply(&mut s, json!([{"op":"insert","texts":["a"]}]));
    // The live shape: {"action":"apply"} with nothing else.
    let result = tools::execute(&mut s, "task_plan", json!({"action":"apply"})).unwrap();
    let reason = result["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("expected_revision is missing; the plan is at revision 1"),
        "{reason}"
    );
    assert!(reason.contains("operations is also missing"), "{reason}");
    let result = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["b"]}]}),
    )
    .unwrap();
    let reason = result["reason"].as_str().unwrap();
    assert!(
        reason.contains("expected_revision 0 but the plan is at revision 1"),
        "{reason}"
    );
    assert!(!reason.contains("operations is also missing"), "{reason}");
}

#[test]
fn insert_accepts_a_single_text_and_ignores_an_invented_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // The live shapes: {"op":"insert","text":...} with and without an "id".
    let result = apply(
        &mut s,
        json!([
            {"op":"insert","text":"Read the entry point"},
            {"op":"insert","text":"Write the overview","id":"t1"},
        ]),
    );
    assert_eq!(result["applied"], true, "{result}");
    assert_eq!(result["input_normalized"], true);
    let texts: Vec<_> = s.task.todos.iter().map(|item| item.text.as_str()).collect();
    assert_eq!(texts, ["Read the entry point", "Write the overview"]);
    assert!(s.task.todos.iter().all(|item| item.id != "t1"));
}

#[test]
fn insert_discards_shared_schema_fields_and_non_id_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let result = apply(
        &mut s,
        json!([{"op":"insert","before":"x","id":"x","reason":"Organize work","result":"pending","text":"unused","texts":["Read settings","Write manual"]}]),
    );
    assert_eq!(result["applied"], true, "{result}");
    assert_eq!(result["input_normalized"], true);
    assert_eq!(
        s.task
            .todos
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["Read settings", "Write manual"]
    );
}

#[test]
fn complete_accepts_the_shared_schema_reason_without_changing_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    apply(
        &mut s,
        json!([{"op":"insert","texts":["Write the section"]}]),
    );
    let id = s.task.todos[0].id.clone();
    let result = apply(
        &mut s,
        json!([{"op":"complete","id":id,"result":"Section saved","reason":"Already done","before":"T1","text":"unused","texts":["unused"]}]),
    );
    assert_eq!(result["applied"], true, "{result}");
    assert_eq!(result["input_normalized"], true);
    assert_eq!(s.task.todos[0].result, "Section saved");
}
