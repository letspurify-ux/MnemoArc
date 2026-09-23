mod support;
use anyhow::Result;
use async_trait::async_trait;
use mnemoarc::{
    agent::run_session,
    config::{Config, Project},
    llm::{Completion, LlmClient, ToolCall},
    session::Session,
    tools::{self, capabilities},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn session(root: &Path) -> Session {
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
fn inv(s: &mut Session, mut args: Value) -> Value {
    if !matches!(args["action"].as_str(), Some("list" | "scan")) {
        args["expected_revision"] = json!(s.capabilities.revision);
    }
    tools::execute(s, "capability_inventory", args).unwrap()
}
fn cov(s: &mut Session, mut args: Value) -> Value {
    if args["action"] == "bind" {
        args["expected_revision"] = json!(s.capabilities.revision);
    }
    tools::execute(s, "documentation_coverage", args).unwrap()
}
fn scan(s: &mut Session) {
    let first = inv(s, json!({"action":"scan"}));
    assert_eq!(first["applied"], true, "{first}");
    while let Some(cursor) = capabilities::summary(s)["next_cursor"].as_str() {
        let r = inv(s, json!({"action":"scan","cursor":cursor}));
        assert_eq!(r["applied"], true, "{r}");
    }
}
fn deliver(s: &mut Session, path: &str) -> Value {
    let call = ToolCall {
        id: mnemoarc::memory::id(),
        name: "file_read".into(),
        arguments: json!({"path":path,"start_line":1,"max_lines":200}).to_string(),
    };
    let r = tools::run_call(s, &call);
    assert_eq!(r["status"], "ok", "{r}");
    tools::record_delivered_read(s, &call, &r);
    r["data"].clone()
}
fn classify(s: &mut Session) {
    for f in s.capabilities.features.clone().iter().filter(|f| f.present) {
        let r = inv(
            s,
            json!({"action":"classify","id":f.id,"decision":"include","kind":f.kind}),
        );
        assert_eq!(r["applied"], true, "{r}");
    }
}
fn review(s: &mut Session, path: &str) {
    let data = deliver(s, path);
    let r = inv(
        s,
        json!({"action":"review_file","path":path,"expected_hash":data["hash"],"source_ids":[data["source"]["id"]],"note":"Reviewed registrations and configured entry points; all requested functions are inventoried."}),
    );
    assert_eq!(r["applied"], true, "{r}; read={data}");
}
fn fields() -> Value {
    json!({"purpose":"Purpose: returns the application's current health.","trigger":"Trigger: GET /health invokes this handler.","inputs":"Inputs: no request body or query parameters.","outputs":"Outputs: a JSON object with status available.","behavior":"Behavior: produces a response without another service call.","data_changes":"Data changes: none, this operation only reads state.","errors":"Errors: connection failures are handled by the server.","permissions":"Permissions: this endpoint is publicly accessible."})
}
fn document(root: &Path) -> String {
    let doc = format!(
        "# Features\n\n## Health\n{}\n",
        fields()
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/source-summary.md"), &doc).unwrap();
    doc
}
fn bind(s: &mut Session, id: &str) {
    let d = deliver(s, "docs/source-summary.md");
    let r = cov(
        s,
        json!({"action":"bind","id":id,"path":"docs/source-summary.md","section":"Health","expected_hash":d["hash"],"fields":fields()}),
    );
    assert_eq!(r["applied"], true, "{r}");
}
fn audit(s: &Session) -> capabilities::Audit {
    capabilities::audit(s, &CancellationToken::new()).unwrap()
}

#[test]
fn shared_discovery_covers_ui_server_and_unknown_files_require_review() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("ui.tsx"),"const page = <Route path=\"/home\" />;\nif (tab === 'logs') {}\n<Dialog role=\"dialog\" />").unwrap();
    fs::write(dir.path().join("server.rs"),"router.route(\n \"/health\", get(handler));\ntokio::spawn(async {});\nwith_graceful_shutdown(shutdown);\nlet socket: WebSocketUpgrade;").unwrap();
    fs::write(
        dir.path().join("service.proto"),
        "service Account { rpc Find (Query) returns (Account); }",
    )
    .unwrap();
    fs::write(
        dir.path().join("listeners.js"),
        "bus.on('order-created', run); cli.command('sync');",
    )
    .unwrap();
    fs::write(
        dir.path().join("legacy.unknown"),
        "register_generated_features(runtime_config)",
    )
    .unwrap();
    fs::write(
        dir.path().join("readme.md"),
        ".route(\"not-source\", get(x))",
    )
    .unwrap();
    fs::write(dir.path().join("Cargo.lock"), "generated").unwrap();
    fs::write(dir.path().join(".DS_Store"), [0xff, 0x00]).unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    assert_eq!(s.capabilities.files.len(), 5);
    for kind in [
        "screen",
        "tab",
        "dialog",
        "http",
        "job",
        "lifecycle",
        "websocket",
        "rpc",
        "event",
        "cli",
    ] {
        assert!(
            s.capabilities.features.iter().any(|f| f.kind == kind),
            "Missing {kind}"
        );
    }
    assert!(
        audit(&s)
            .issues
            .iter()
            .any(|i| i.kind == "file_unreviewed" && i.path.as_deref() == Some("legacy.unknown"))
    );
    assert!(!audit(&s).ready);
    let ids: Vec<_> = s
        .capabilities
        .features
        .iter()
        .map(|f| f.id.clone())
        .collect();
    scan(&mut s);
    assert_eq!(
        ids,
        s.capabilities
            .features
            .iter()
            .map(|f| f.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn scan_pages_do_not_skip_and_stale_cursors_and_mutations_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..25 {
        fs::write(
            dir.path().join(format!("file{n:02}.rs")),
            "fn ordinary() {}\n",
        )
        .unwrap();
    }
    let mut s = session(dir.path());
    let first = inv(&mut s, json!({"action":"scan"}));
    assert_eq!(first["summary"]["scanned_files"], 20);
    let cursor = first["summary"]["next_cursor"].clone();
    let before = serde_json::to_value(&s.capabilities).unwrap();
    assert_eq!(
        inv(&mut s, json!({"action":"scan","cursor":"stale"}))["applied"],
        false
    );
    assert_eq!(serde_json::to_value(&s.capabilities).unwrap(), before);
    assert_eq!(
        inv(&mut s, json!({"action":"scan","cursor":cursor}))["summary"]["scanned_files"],
        25
    );
    fs::write(dir.path().join("new.rs"), "fn new() {}\n").unwrap();
    assert!(
        audit(&s)
            .issues
            .iter()
            .any(|i| i.kind == "manifest_changed")
    );
    scan(&mut s);
    assert_eq!(s.capabilities.files.len(), 26);
    fs::remove_file(dir.path().join("new.rs")).unwrap();
    assert!(
        audit(&s)
            .issues
            .iter()
            .any(|i| i.kind == "manifest_changed")
    );
    scan(&mut s);
    assert_eq!(s.capabilities.files.len(), 25);
}

#[test]
fn bad_argument_types_are_recoverable_and_do_not_poison_retry_ids() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let mut s = session(dir.path());
    for args in [
        json!(null),
        json!({"action":"scan","cursor":12}),
        json!({"action":"bind","fields":[]}),
        json!({"action":"scan","unexpected":true}),
    ] {
        let mut call = ToolCall {
            id: mnemoarc::memory::id(),
            name: if args["action"] == "bind" {
                "documentation_coverage"
            } else {
                "capability_inventory"
            }
            .into(),
            arguments: args.to_string(),
        };
        let r = tools::run_call(&mut s, &call);
        if args.is_object() {
            assert_eq!(r["status"], "ok", "{r}");
            assert_eq!(r["data"]["applied"], false, "{r}");
        } else {
            assert_eq!(r["status"], "error", "{r}");
            assert_eq!(r["recovery"]["class"], "invalid_input");
        }
        assert!(!s.ledger.contains_key(&call.id));
        call.name = "capability_inventory".into();
        call.arguments = json!({"action":"scan"}).to_string();
        assert_eq!(tools::run_call(&mut s, &call)["data"]["applied"], true);
    }
    assert_eq!(s.capabilities.revision, 4);
    let before = serde_json::to_value(&s.capabilities).unwrap();
    let r=tools::execute(&mut s,"capability_inventory",json!({"action":"review_file","expected_revision":0,"path":"a.rs","expected_hash":"bad","note":"test","source_ids":[]})).unwrap();
    assert_eq!(r["applied"], false);
    assert_eq!(before, serde_json::to_value(&s.capabilities).unwrap());
}

#[test]
fn delivered_source_review_and_real_sections_are_required_and_revalidated() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("server.rs"),
        "router.route(\"/health\", get(handler));\n",
    )
    .unwrap();
    let original = document(dir.path());
    let mut s = session(dir.path());
    scan(&mut s);
    classify(&mut s);
    let hash = s.capabilities.files["server.rs"].hash.clone();
    let r = inv(
        &mut s,
        json!({"action":"review_file","path":"server.rs","expected_hash":hash,"note":"No source evidence yet","source_ids":[]}),
    );
    assert_eq!(r["applied"], false);
    review(&mut s, "server.rs");
    let id = s.capabilities.features[0].id.clone();
    assert!(audit(&s).issues.iter().any(|i| i.kind == "undocumented"));
    bind(&mut s, &id);
    assert!(audit(&s).ready);
    fs::write(
        dir.path().join("docs/source-summary.md"),
        format!("{original}\n## Unrelated\nNew prose.\n"),
    )
    .unwrap();
    assert!(audit(&s).ready);
    fs::write(
        dir.path().join("docs/source-summary.md"),
        original.replace("publicly accessible", "restricted to administrators"),
    )
    .unwrap();
    assert!(!audit(&s).ready);
    fs::write(dir.path().join("docs/source-summary.md"), &original).unwrap();
    assert!(audit(&s).ready);
    fs::write(
        dir.path().join("server.rs"),
        "// changed\nrouter.route(\"/health\", get(handler));\n",
    )
    .unwrap();
    assert!(!audit(&s).ready);
    scan(&mut s);
    assert_eq!(s.capabilities.features[0].id, id);
    assert_eq!(s.capabilities.features[0].status, "candidate");
    classify(&mut s);
    review(&mut s, "server.rs");
    assert!(!audit(&s).ready);
    bind(&mut s, &id);
    assert!(audit(&s).ready);
    let prior = audit(&s).fingerprint;
    inv(
        &mut s,
        json!({"action":"classify","id":id,"decision":"exclude","reason":"Out of the original request's scope"}),
    );
    assert_ne!(audit(&s).fingerprint, prior);
}

#[test]
fn manual_features_can_be_refreshed_or_explicitly_excluded_after_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("dynamic.rs"), "registry.load(config);\n").unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    let digest = s.capabilities.files["dynamic.rs"].hash.clone();
    let r = inv(
        &mut s,
        json!({"action":"register","title":"Dynamic event","kind":"event","path":"dynamic.rs","start_line":1,"end_line":1,"expected_hash":digest}),
    );
    assert_eq!(r["applied"], true);
    let id = r["id"].clone();
    fs::write(
        dir.path().join("dynamic.rs"),
        "// new line\nregistry.load(config_v2);\n",
    )
    .unwrap();
    scan(&mut s);
    let bad = inv(
        &mut s,
        json!({"action":"classify","id":id,"decision":"include","kind":"event"}),
    );
    assert_eq!(bad["applied"], false);
    let digest = s.capabilities.files["dynamic.rs"].hash.clone();
    let r = inv(
        &mut s,
        json!({"action":"register","id":id,"title":"Dynamic event","kind":"event","path":"dynamic.rs","start_line":2,"end_line":2,"expected_hash":digest}),
    );
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(s.capabilities.features.len(), 1);
    assert_eq!(s.capabilities.features[0].start_line, 2);
    fs::write(dir.path().join("dynamic.rs"), "fn removed() {}\n").unwrap();
    scan(&mut s);
    let r = inv(
        &mut s,
        json!({"action":"classify","id":id,"decision":"exclude","reason":"Registration was removed in the current source"}),
    );
    assert_eq!(r["applied"], true, "{r}");
    review(&mut s, "dynamic.rs");
    assert!(audit(&s).ready);
}

#[test]
fn inventory_above_100_survives_bounded_repair_batches_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("server.rs"),
        (0..105)
            .map(|n| format!("router.route(\"/api/{n}\", get(handler));\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut s = session(dir.path());
    s.add_user("Document all functions".into());
    scan(&mut s);
    classify(&mut s);
    assert_eq!(s.capabilities.features.len(), 105);
    s.config.state_tokens = 16000;
    let a = audit(&s);
    capabilities::enqueue(&mut s, &a);
    let count = s.task.todos.iter().filter(|t| !t.done).count();
    assert_eq!(count, 100);
    capabilities::enqueue(&mut s, &a);
    assert_eq!(s.task.todos.iter().filter(|t| !t.done).count(), 100);
    assert_eq!(audit(&s).fingerprint, a.fingerprint);
    let saved = serde_json::to_value(&s.capabilities).unwrap();
    s.add_user("계속".into());
    assert_eq!(serde_json::to_value(&s.capabilities).unwrap(), saved);
    s.add_user("A different request".into());
    assert!(!s.capabilities.active);
}

#[test]
fn tiny_results_retain_recovery_metadata_and_pages_resume_without_skipping() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.rs"),
        (0..15)
            .map(|n| format!("router.route(\"/endpoint/{n}\", get(handler));\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    let call = ToolCall {
        id: "list".into(),
        name: "capability_inventory".into(),
        arguments: json!({"action":"list","limit":20}).to_string(),
    };
    let r = tools::run_call(&mut s, &call);
    let small = tools::limit_result(&mut s, &call, r, 1500);
    if let Some(items) = small["data"]["items"].as_array() {
        assert_eq!(small["data"]["next_offset"], items.len());
        assert!(items.len() < 15);
    } else {
        assert_eq!(small["next_cursor"]["tool"], "history");
    }
    let call = ToolCall {
        id: "invalid".into(),
        name: "capability_inventory".into(),
        arguments: json!({"action":"classify","id":false}).to_string(),
    };
    let r = tools::run_call(&mut s, &call);
    let small = tools::limit_result(&mut s, &call, r, 200);
    assert_eq!(small["data"]["applied"], false, "{small}");
    assert!(small["data"]["revision"].is_u64());
    let call = ToolCall {
        id: "scan-small".into(),
        name: "capability_inventory".into(),
        arguments: json!({"action":"scan"}).to_string(),
    };
    let r = tools::run_call(&mut s, &call);
    let small = tools::limit_result(&mut s, &call, r, 200);
    assert_eq!(small["data"]["applied"], true, "{small}");
}

struct PrematureFinal(Mutex<usize>);
#[async_trait]
impl LlmClient for PrematureFinal {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        assert!(
            support::acceptance(&request).is_none(),
            "Incomplete coverage must precede final semantic review"
        );
        let mut n = self.0.lock().unwrap();
        *n += 1;
        if *n == 1 {
            return Ok(Completion {
                text: "All finished.".into(),
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
        )
        .unwrap();
        let task = &state["run_guidance"]["current_todo"];

        assert!(
            state["run_guidance"]["completion_error"]
                .as_str()
                .unwrap()
                .starts_with("documentation_coverage")
        );
        // Deliberately pretend each repair completed, without changing evidence.
        if (*n).is_multiple_of(2) {
            assert!(task["id"].is_string());
            Ok(Completion{calls:vec![ToolCall{id:format!("pretend-{n}"),name:"task_plan".into(),arguments:json!({"action":"apply","expected_revision":state["task"]["plan_revision"],"operations":[{"op":"complete","id":task["id"],"result":"Pretended to review without any actual source read"}]}).to_string()}],..Default::default()})
        } else {
            Ok(Completion {
                text: "All finished again.".into(),
                ..Default::default()
            })
        }
    }
}
#[tokio::test]
async fn empty_todos_cannot_bypass_inventory_gate_and_unchanged_churn_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.unknown"), "runtime_register(config)\n").unwrap();
    let mut s = session(dir.path());
    s.add_user("Document every registered function".into());
    scan(&mut s);
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let s = run_session(
        s,
        Arc::new(PrematureFinal(Mutex::new(0))),
        CancellationToken::new(),
        tx,
    )
    .await;
    drain.await.unwrap();
    assert_eq!(s.status, "partial", "{:?}", s.last_error);
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .starts_with("documentation_coverage_no_progress"),
        "{:?}",
        s.last_error
    );
    assert!(s.task.current_todo().is_some());
    assert!(s.capabilities.active);
}

#[test]
fn partial_reads_empty_files_cancellation_and_scope_changes_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
    fs::write(dir.path().join("empty.rs"), "").unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    let call = ToolCall {
        id: "partial-read".into(),
        name: "file_read".into(),
        arguments: json!({"path":"a.rs","max_lines":1}).to_string(),
    };
    let r = tools::run_call(&mut s, &call);
    tools::record_delivered_read(&mut s, &call, &r);
    let r = inv(
        &mut s,
        json!({"action":"review_file","path":"a.rs","expected_hash":r["data"]["hash"],"source_ids":[r["data"]["source"]["id"]],"note":"Only saw first function"}),
    );
    assert_eq!(r["applied"], false);
    assert!(
        r["reason"]
            .as_str()
            .unwrap()
            .contains("first unread line 2")
    );
    review(&mut s, "a.rs");
    let digest = s.capabilities.files["empty.rs"].hash.clone();
    assert_eq!(
        inv(
            &mut s,
            json!({"action":"review_file","path":"empty.rs","expected_hash":digest,"source_ids":[],"note":"Empty file has no executable registrations"})
        )["applied"],
        true
    );
    assert!(audit(&s).ready);
    let before = serde_json::to_value(&s.capabilities).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        capabilities::execute(
            &mut s,
            "capability_inventory",
            &json!({"action":"scan"}),
            &cancel
        )
        .is_err()
    );
    assert_eq!(before, serde_json::to_value(&s.capabilities).unwrap());
    s.project.exclude.push("a.rs".into());
    assert!(!audit(&s).ready);
    assert_eq!(inv(&mut s, json!({"action":"scan"}))["applied"], false);
}

#[test]
fn metadata_rewording_does_not_reset_gap_stalls_and_known_repairs_survive_verify_phase() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("server.rs"),
        "router.route(\"/health\", get(handler));\n",
    )
    .unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    classify(&mut s);
    let before = audit(&s);
    let id = s.capabilities.features[0].id.clone();
    inv(
        &mut s,
        json!({"action":"classify","id":id,"decision":"include","kind":"http","title":"Health endpoint"}),
    );
    let after = audit(&s);
    assert_ne!(before.fingerprint, after.fingerprint);
    assert_eq!(before.progress_fingerprint, after.progress_fingerprint);
    let doc = document(dir.path());
    bind(&mut s, &id);
    let before = audit(&s);
    fs::write(
        dir.path().join("docs/source-summary.md"),
        format!("{doc}\n## Unrelated\nReworded notes.\n"),
    )
    .unwrap();
    let after = audit(&s);
    assert_ne!(before.fingerprint, after.fingerprint);
    assert_eq!(
        before.progress_fingerprint, after.progress_fingerprint,
        "Unrelated prose must not reset retries for the same unresolved file review"
    );
    s.active_tools.insert("investigation".into());
    tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","title":"Existing source review"}),
    )
    .unwrap();
    s.run_guidance = json!({"phase":"verify"});
    assert!(
        tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","title":"Unrelated scope expansion"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","title":"Health endpoint"})
        )
        .is_ok()
    );
}

struct Accepted;
#[async_trait]
impl LlmClient for Accepted {
    async fn complete(
        &self,
        request: Value,
        _: &Config,
        _: CancellationToken,
        _: mpsc::Sender<String>,
    ) -> Result<Completion> {
        if let Some(result) = support::acceptance(&request) {
            return Ok(result);
        }
        Ok(Completion {
            text: "Requested documentation is linked and current.".into(),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn satisfied_inventory_reaches_independent_acceptance_and_completion() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("server.rs"),
        "router.route(\"/health\", get(handler));\n",
    )
    .unwrap();
    document(dir.path());
    let mut s = session(dir.path());
    s.add_user("Confirm that the health documentation is linked".into());
    scan(&mut s);
    classify(&mut s);
    review(&mut s, "server.rs");
    let id = s.capabilities.features[0].id.clone();
    bind(&mut s, &id);
    let (tx, mut rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let s = run_session(s, Arc::new(Accepted), CancellationToken::new(), tx).await;
    drain.await.unwrap();
    assert_eq!(s.status, "complete", "{:?}", s.last_error);
    assert!(s.completion_review.approved);
    assert_eq!(s.capabilities.last_audit.unwrap()["ready"], true);
}

#[test]
fn multiple_dynamic_functions_can_share_one_source_range() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("dynamic.rs"), "register_from_config();\n").unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    let hash = s.capabilities.files["dynamic.rs"].hash.clone();
    for title in ["User screen", "Admin screen", "User screen"] {
        let r = inv(
            &mut s,
            json!({"action":"register","title":title,"kind":"screen","path":"dynamic.rs","start_line":1,"end_line":1,"expected_hash":hash}),
        );
        assert_eq!(r["applied"], true, "{r}");
    }
    assert_eq!(s.capabilities.features.len(), 2);
    review(&mut s, "dynamic.rs");
    assert_eq!(audit(&s).total, 2);
}

#[test]
fn literal_bindings_reject_invented_fields_outside_sections_and_metadata_overflow() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.rs"),
        "router.route(\"/health\", get(h));\n",
    )
    .unwrap();
    document(dir.path());
    let mut s = session(dir.path());
    scan(&mut s);
    classify(&mut s);
    let id = s.capabilities.features[0].id.clone();
    let d = deliver(&mut s, "docs/source-summary.md");
    let before = serde_json::to_value(&s.capabilities).unwrap();
    let mut bad = fields();
    bad["purpose"] = json!("Invented explanation that isn't in the document");
    let r = cov(
        &mut s,
        json!({"action":"bind","id":id,"path":"docs/source-summary.md","section":"Health","expected_hash":d["hash"],"fields":bad}),
    );
    assert_eq!(r["applied"], false);
    assert_eq!(serde_json::to_value(&s.capabilities).unwrap(), before);
    s.config.memory_bytes = 200;
    let r = inv(
        &mut s,
        json!({"action":"classify","id":id,"decision":"include","kind":"http","title":"Changed title"}),
    );
    assert_eq!(r["applied"], false);
    assert_eq!(serde_json::to_value(&s.capabilities).unwrap(), before);
}

#[test]
fn repaired_first_window_enqueues_remaining_features_without_losing_inventory() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("server.rs"),
        (0..101)
            .map(|n| format!("router.route(\"/api/{n}\", get(handler));\n"))
            .collect::<String>(),
    )
    .unwrap();
    document(dir.path());
    let mut s = session(dir.path());
    s.config.state_tokens = 16000;
    s.config.result_tokens = 8000;
    scan(&mut s);
    classify(&mut s);
    review(&mut s, "server.rs");
    let a = audit(&s);
    capabilities::enqueue(&mut s, &a);
    assert_eq!(s.task.todos.iter().filter(|t| !t.done).count(), 100);
    let d = deliver(&mut s, "docs/source-summary.md");
    for feature in s.capabilities.features.clone().into_iter().take(100) {
        let r = cov(
            &mut s,
            json!({"action":"bind","id":feature.id,"path":"docs/source-summary.md","section":"Health","expected_hash":d["hash"],"fields":fields()}),
        );
        assert_eq!(r["applied"], true, "{r}");
        let current = s.task.current_todo().unwrap().id.clone();
        let revision = s.task.plan_revision;
        let r=tools::execute(&mut s,"task_plan",json!({"action":"apply","expected_revision":revision,"operations":[{"op":"complete","id":current,"result":"Bound the corresponding feature description"}]})).unwrap();
        assert_eq!(r["applied"], true, "{r}");
    }
    let a = audit(&s);
    assert_eq!(a.documented, 100);
    assert_eq!(a.issues.len(), 1);
    capabilities::enqueue(&mut s, &a);
    assert_eq!(s.task.todos.iter().filter(|t| !t.done).count(), 1);
    assert!(s.task.current_todo().unwrap().text.starts_with("F101:"));
    assert_eq!(s.capabilities.features.len(), 101);
}

#[test]
fn large_tool_contract_is_exposed_only_after_inventory_starts() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let mut s = session(dir.path());
    let initial = tools::ToolRegistry::definitions(&s);
    let initial = initial
        .iter()
        .find(|t| t["function"]["name"] == "capability_inventory")
        .unwrap();
    assert_eq!(
        initial["function"]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert!(
        !tools::ToolRegistry::definitions(&s)
            .iter()
            .any(|t| t["function"]["name"] == "documentation_coverage")
    );
    scan(&mut s);
    let active = tools::ToolRegistry::definitions(&s);
    assert!(
        active
            .iter()
            .any(|t| t["function"]["name"] == "documentation_coverage")
    );
    let active = active
        .iter()
        .find(|t| t["function"]["name"] == "capability_inventory")
        .unwrap();
    assert!(
        active["function"]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("expected_hash")
    );
}

#[test]
fn markdown_pages_are_sources_while_ordinary_markdown_stays_documentation() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/pages")).unwrap();
    fs::write(
        dir.path().join("src/pages/help.md"),
        "# Help\nScreen contents\n",
    )
    .unwrap();
    fs::write(dir.path().join("src/pages/about.astro"), "<h1>About</h1>\n").unwrap();
    fs::write(dir.path().join("README.md"), "# Project docs\n").unwrap();
    let mut s = session(dir.path());
    scan(&mut s);
    assert_eq!(s.capabilities.files.len(), 2);
    assert_eq!(s.capabilities.features.len(), 2);
    assert!(s.capabilities.features.iter().all(|f| f.kind == "screen"));
}

#[test]
fn independent_acceptance_sees_exclusion_reasons_and_scope_limits() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.rs"),
        "router.route(\"/health\", get(handler));\n",
    )
    .unwrap();
    let mut s = session(dir.path());
    s.add_user("Review the requested catalog scope".into());
    scan(&mut s);
    let id = s.capabilities.features[0].id.clone();
    inv(
        &mut s,
        json!({"action":"classify","id":id,"decision":"exclude","reason":"Health endpoint was explicitly excluded by the requested scope"}),
    );
    review(&mut s, "a.rs");
    assert!(audit(&s).ready);
    assert!(matches!(
        tools::completion_review::begin_final(&mut s, "Scope reviewed.", false).unwrap(),
        tools::completion_review::Gate::Review
    ));
    let request = tools::completion_review::request(&mut s).unwrap();
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["capability_inventory"]["exclusions"][0]["id"], id);
    assert!(
        payload["capability_inventory"]["exclusions"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("explicitly excluded")
    );
    assert!(
        payload["capability_inventory"]["limitation"]
            .as_str()
            .unwrap()
            .contains("model-authored assertions")
    );
}
