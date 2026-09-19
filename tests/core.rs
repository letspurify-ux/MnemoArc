use mnemoarc::{
    config::{Config, Project},
    context::{self, ContextManager},
    memory::{MemoryInput, MemoryKind, MemoryStatus, MemoryStore},
    session::Session,
    tools,
};
use serde_json::json;
use std::collections::BTreeSet;
fn config() -> Config {
    Config {
        model: "gpt-4o".into(),
        model_context: Some(128000),
        ..Default::default()
    }
}
fn input(key: &str, body: &str, rev: Option<u64>) -> MemoryInput {
    MemoryInput {
        key: Some(key.into()),
        title: key.into(),
        summary: "reusable finding".into(),
        body: body.into(),
        tags: vec!["태그".into()],
        kind: MemoryKind::Fact,
        inferred: false,
        source_ids: vec![],
        metadata: json!(null),
        expected_revision: rev,
    }
}
fn session(root: &std::path::Path) -> Session {
    Session::new(
        Project {
            root: root.into(),
            ..Default::default()
        },
        config(),
    )
}
#[test]
fn memory_conflicts_paging_and_no_silent_eviction() {
    let mut store = MemoryStore::default();
    let c = config();
    let a = store
        .save(
            input("first", "한국어 검색 create_session", None),
            vec![],
            &c,
        )
        .unwrap();
    assert!(
        store
            .save(input("first", "change", None), vec![], &c)
            .is_err()
    );
    let old = store.entries[&a.id].updated_at;
    assert_eq!(store.get("first").unwrap().id, a.id);
    assert_eq!(old, store.entries[&a.id].updated_at);
    assert_eq!(store.search("한국어", &[]).len(), 1);
    let same = store
        .save(
            input("first", "한국어 검색 create_session", Some(1)),
            vec![],
            &c,
        )
        .unwrap();
    assert_eq!(same.revision, 1);
    store
        .save(input("second", "another body", None), vec![], &c)
        .unwrap();
    let page = store.page("", &[], None, 1).unwrap();
    let cursor = page["next_cursor"].as_str().unwrap();
    assert_eq!(
        store.page("", &[], Some(cursor), 1).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    store
        .save(input("third", "third body", None), vec![], &c)
        .unwrap();
    assert!(
        store
            .page("", &[], Some(cursor), 1)
            .unwrap_err()
            .to_string()
            .contains("cursor_expired")
    );
    let mut small = c.clone();
    small.memory_count = 3;
    assert!(
        store
            .save(input("fourth", "body", None), vec![], &small)
            .is_err()
    );
    assert_eq!(store.entries.len(), 3);
    assert!(
        store
            .delete(&a.id, &BTreeSet::from([a.id.clone()]))
            .is_err()
    );
    let replaced = store
        .replace(
            std::slice::from_ref(&a.id),
            input("replacement", "new", None),
            vec![],
            &small,
        )
        .unwrap();
    assert_eq!(store.entries.len(), 3);
    assert!(store.get(&replaced.id).is_ok());
}
#[test]
fn replacement_failure_is_atomic() {
    let mut store = MemoryStore::default();
    let c = config();
    let a = store.save(input("a", "old", None), vec![], &c).unwrap();
    let invalid = input("b", &"x".repeat(9000), None);
    assert!(
        store
            .replace(std::slice::from_ref(&a.id), invalid, vec![], &c)
            .is_err()
    );
    assert_eq!(store.get(&a.id).unwrap().body, "old");
}
#[test]
fn sessions_and_history_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let b = session(dir.path());
    a.memory
        .save(input("key", "only session a", None), vec![], &a.config)
        .unwrap();
    a.add_user("private goal".into());
    assert!(b.memory.entries.is_empty());
    assert!(b.history.bundles.is_empty());
    let id = a.history.next_id;
    assert!(a.history.prune(1).is_err());
    assert!(a.history.read(id).is_ok());
    a.history.bundles[0].active = false;
    a.history.bundles[0].reviewed = true;
    a.history.prune(1).unwrap();
    assert!(a.history.read(id).is_err());
    assert_eq!(a.history.pruned_through, Some(id));
}
#[test]
fn checkpoint_three_cycles_preserve_memory_and_whole_tool_bundles() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.task.constraints.push("Never modify source code".into());
    let m = s
        .memory
        .save(
            input("failure", "Do not repeat failed parser approach", None),
            vec![],
            &s.config,
        )
        .unwrap();
    s.task.memory_ids.push(m.id.clone());
    for cycle in 0..3 {
        for i in 0..5 {
            s.history.push(vec![json!({"role":"assistant","tool_calls":[{"id":format!("{cycle}-{i}"),"type":"function","function":{"name":"x","arguments":"{}"}}]}),json!({"role":"tool","tool_call_id":format!("{cycle}-{i}"),"content":"result"})],true);
        }
        assert!(ContextManager::prepare(&mut s, 60000).unwrap());
        let before = s.history.active();
        assert!(ContextManager::commit(&mut s).is_err());
        assert_eq!(before, s.history.active());
        let cp = s.checkpoint.as_ref().unwrap().id.clone();
        tools::execute(
            &mut s,
            "checkpoint_complete",
            json!({"id":cp,"no_save_reason":"All required facts already stored"}),
        )
        .unwrap();
        ContextManager::commit(&mut s).unwrap();
        for b in &s.history.bundles {
            assert_eq!(b.messages.len(), 2);
        }
    }
    assert_eq!(s.memory.get("failure").unwrap().id, m.id);
    assert_eq!(s.task.constraints.len(), 1);
    let state = ContextManager::state(&s).unwrap();
    assert!(
        state["referenced_memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == m.id)
    );
}
#[test]
fn failed_checkpoint_tool_cannot_acknowledge() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("a".into());
    s.add_user("b".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let cp = s.checkpoint.as_ref().unwrap().id.clone();
    let call = mnemoarc::llm::ToolCall {
        id: "bad".into(),
        name: "memory_write".into(),
        arguments: "{}".into(),
    };
    let result = tools::run_call(&mut s, &call);
    assert_eq!(result["status"], "error");
    assert!(
        tools::execute(
            &mut s,
            "checkpoint_complete",
            json!({"id":cp,"no_save_reason":"skip"})
        )
        .is_err()
    );
    assert!(ContextManager::commit(&mut s).is_err());
    assert_eq!(s.history.active().len(), 2);
}
#[test]
fn file_evidence_document_conflict_and_revalidation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();
    let mut s = session(dir.path());
    assert!(tools::execute(&mut s, "file_read", json!({"path":"main.rs"})).is_err());
    tools::execute(
        &mut s,
        "tool_select",
        json!({"action":"add","names":["source-docs"]}),
    )
    .unwrap();
    assert!(s.active_tools.is_empty());
    s.active_tools = s.pending_tools.take().unwrap();
    let read = tools::execute(&mut s, "file_read", json!({"path":"main.rs"})).unwrap();
    let source_id = read["source"]["id"].as_str().unwrap();
    let mut m = input("entrypoint", "main prints hello", None);
    m.source_ids.push(source_id.into());
    let sources = s.source_refs(&m.source_ids).unwrap();
    let meta = s.memory.save(m, sources, &s.config).unwrap();
    let doc = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain prints hello. [source](../main.rs#L1)\n"}),
    )
    .unwrap();
    let h = doc["hash"].as_str().unwrap();
    assert!(
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"append","text":"bad"})
        )
        .is_err()
    );
    let item=tools::execute(&mut s,"investigation",json!({"action":"upsert","title":"entry","status":"written","memory_ids":[meta.id],"source_ids":[source_id],"section":"# Entry"})).unwrap();
    tools::execute(&mut s,"investigation",json!({"action":"verify","id":item["id"],"source_ids":[source_id],"verification_note":"Compared main body with the Entry section"})).unwrap();
    assert_eq!(s.investigations[0].status, "verified");
    let call = mnemoarc::llm::ToolCall {
        id: "append-1".into(),
        name: "document_edit".into(),
        arguments: json!({"action":"append","text":"\nExtra.\n","expected_hash":h}).to_string(),
    };
    let first = tools::run_call(&mut s, &call);
    assert_eq!(first["status"], "ok");
    let second = tools::run_call(&mut s, &call);
    assert_eq!(first, second);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("docs/source-summary.md"))
            .unwrap()
            .matches("Extra.")
            .count(),
        1
    );
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    tools::revalidate(&mut s).unwrap();
    assert_eq!(
        s.memory.get("entrypoint").unwrap().status,
        MemoryStatus::NeedsReview
    );
    assert_eq!(s.investigations[0].status, "written");
}
#[test]
fn source_pagination_and_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..3 {
        std::fs::write(dir.path().join(format!("{i}.rs")), "needle\n").unwrap();
    }
    let mut s = session(dir.path());
    s.active_tools = tools::ToolRegistry::optional_names();
    let first =
        tools::execute(&mut s, "source_search", json!({"query":"needle","limit":1})).unwrap();
    let next = tools::execute(
        &mut s,
        "source_search",
        json!({"query":"needle","limit":1,"cursor":first["next_cursor"]}),
    )
    .unwrap();
    assert_ne!(first["matches"][0]["path"], next["matches"][0]["path"]);
    assert!(tools::execute(&mut s, "file_read", json!({"path":"/etc/hosts"})).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/hosts", dir.path().join("escape")).unwrap();
        assert!(tools::execute(&mut s, "file_read", json!({"path":"escape"})).is_err());
    }
}
#[test]
fn settings_validation_and_lowering_keeps_original() {
    let mut c = config();
    c.recent_count = 100;
    assert!(c.validate().is_err());
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.memory
        .save(input("a", "body", None), vec![], &s.config)
        .unwrap();
    let original = s.config.memory_bytes;
    let mut c = s.config.clone();
    c.memory_bytes = 1;
    assert!(mnemoarc::agent::apply_config(&mut s, c).is_err());
    assert_eq!(s.config.memory_bytes, original);
}
#[test]
fn config_saved_then_session_override() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    let c = config();
    c.save(&p).unwrap();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("model".into(), json!("custom-model"));
    let loaded = Config::load(&p, &overrides).unwrap();
    assert_eq!(loaded.model, "custom-model");
    assert_eq!(loaded.model_context, c.model_context);
}
#[test]
fn unicode_truncation_is_safe() {
    let s = "한국어🦀".repeat(500);
    let (part, cut) = context::truncate(&s, 100, "unknown-model");
    assert!(cut);
    assert!(part.len() <= 100);
    assert!(s.starts_with(&part));
}

#[test]
fn history_pruning_and_checkpoint_capacity_failure_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("old constraint".repeat(100));
    s.add_user("new task".repeat(100));
    s.history.bundles[0].active = false;
    s.history.bundles[0].reviewed = true;
    let before = serde_json::to_value(&s.history.bundles).unwrap();
    assert!(s.history.prune(1).is_err());
    assert_eq!(before, serde_json::to_value(&s.history.bundles).unwrap());
    assert!(s.history.pruned_through.is_none());
    s.history.bundles[0].active = true;
    s.history.bundles[0].reviewed = false;
    s.config.history_bytes = 1;
    ContextManager::prepare(&mut s, 60000).unwrap();
    s.checkpoint.as_mut().unwrap().acknowledged = true;
    let before = serde_json::to_value(&s.history.bundles).unwrap();
    assert!(ContextManager::commit(&mut s).is_err());
    assert_eq!(before, serde_json::to_value(&s.history.bundles).unwrap());
    assert!(s.checkpoint.is_some());
    assert_eq!(s.checkpoints_completed, 0);
}

#[test]
fn fixed_source_fingerprint_ignores_outputs_but_detects_source_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut project = Project {
        root: dir.path().into(),
        ..Default::default()
    };
    project.exclude.push("results/**".into());
    let first = tools::project_fingerprint(&project).unwrap();
    std::fs::create_dir(dir.path().join("results")).unwrap();
    std::fs::write(dir.path().join("results/summary.md"), "Generated").unwrap();
    assert_eq!(first, tools::project_fingerprint(&project).unwrap());
    std::fs::write(dir.path().join("main.rs"), "fn main() { panic!(); }\n").unwrap();
    assert_ne!(first, tools::project_fingerprint(&project).unwrap());
}

#[test]
fn history_pressure_selects_enough_groups_even_when_context_is_small() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    for _ in 0..5 {
        s.add_user("retained original text".repeat(100));
    }
    s.config.history_bytes = s.history.bytes() / 2;
    ContextManager::prepare(&mut s, 100).unwrap();
    assert!(s.checkpoint.as_ref().unwrap().bundle_ids.len() >= 3);
    s.checkpoint.as_mut().unwrap().acknowledged = true;
    ContextManager::commit(&mut s).unwrap();
    assert!(s.history.bytes() <= s.config.history_bytes);
    assert!(s.history.bundles.back().unwrap().active);
}
