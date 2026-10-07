use crate::support;
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
        ..support::compact_config()
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
fn new_memory_key_accepts_zero_revision_placeholder_but_existing_key_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let args = json!({"key":"new-key","title":"Finding","summary":"Observed detail","body":"The detail is retained","kind":"fact","expected_revision":0});
    let created = tools::execute(&mut s, "memory_write", args.clone()).unwrap();
    assert_eq!(created["revision"], 1);
    let err = tools::execute(&mut s, "memory_write", args)
        .unwrap_err()
        .to_string();
    assert!(err.contains("revision_conflict: expected 1"), "{err}");
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
            json!({"id":cp,"progress":"Saved facts and preserved next steps","no_save_reason":"All required facts already stored"}),
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
            json!({"id":cp,"progress":"Saved facts and preserved next steps","no_save_reason":"skip"})
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
    s.select_workflow("source_document").unwrap();
    s.active_tools.clear();
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
    // Section edits without expected_hash are refused unless the document is
    // still the model's own last write (covered in documentation.rs); anchored
    // text edits never need it, their exact old_text is the guard.
    s.last_document_write = None;
    assert!(
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"insert_after","section":"# Entry","text":"# Bad\n"})
        )
        .is_err()
    );
    let _ = meta;
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
fn llm_transport_settings_are_validated_and_endpoint_is_composed_safely() {
    let mut c = Config {
        base_url: "ftp://example.com/v1".into(),
        ..support::compact_config()
    };
    assert!(c.validate().is_err());

    c.base_url = "https://example.com/v1?tenant=alpha".into();
    assert_eq!(
        c.completion_url().unwrap().as_str(),
        "https://example.com/v1/chat/completions?tenant=alpha"
    );
    c.base_url = "https://example.com/v1#fragment".into();
    assert!(c.validate().is_err());

    c.base_url = "https://example.com/v1".into();
    c.proxy = Some("http://[invalid".into());
    assert!(c.validate().is_err());
    c.disable_proxy = true;
    assert!(c.validate().is_ok());
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
fn config_load_uses_saved_credential_for_cli_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = config();
    config.save(&path).unwrap();
    std::fs::write(
        path.with_extension("credentials.json"),
        serde_json::to_vec(&json!({config.api_key_env.clone(): "saved-test-key"})).unwrap(),
    )
    .unwrap();
    let loaded = Config::load(&path, &Default::default()).unwrap();
    assert_eq!(
        loaded.api_key.as_ref().map(|key| key.0.as_str()),
        Some("saved-test-key")
    );
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("saved-test-key")
    );
}
#[test]
fn unicode_truncation_is_safe() {
    let s = "한국어🦀".repeat(500);
    let (part, cut) = context::truncate(&s, 100, "unknown-model");
    assert!(cut);
    assert!(context::tokens(&part, "unknown-model") <= 100);
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
    // Work added after preparation has not been approved for removal.
    s.add_user("Unconfirmed new work".into());
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

#[test]
fn unknown_model_can_store_korean_metadata_and_budget_twenty_memories() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.model = "z-ai/glm-5.3-flash".into();
    assert!(context::is_estimated(&s.config.model));
    for i in 0..20 {
        let mut item = input(
            &format!("flow-{i}"),
            "실패한 접근과 다음 조사 항목을 보존합니다.",
            None,
        );
        item.title = "주요 실행 흐름".into();
        item.summary = "요청 검증 후 에이전트를 실행하고 오류를 반환한다.".into();
        s.memory.save(item, vec![], &s.config).unwrap();
    }
    let state = ContextManager::state(&s).unwrap();
    assert_eq!(state["recent_memories"].as_array().unwrap().len(), 20);
    let sample = "한국어 구조와 오류 처리".repeat(20);
    let estimate = context::tokens(&sample, &s.config.model);
    assert!(estimate < sample.len());
    let baseline = tiktoken_rs::cl100k_base()
        .unwrap()
        .encode_with_special_tokens(&sample)
        .len();
    assert_eq!(estimate, (baseline * 5).div_ceil(4));
}

#[test]
fn bounded_file_results_keep_sources_offsets_and_fit_serialized_message_budget() {
    let dir = tempfile::tempdir().unwrap();
    let text = "const value = \"한국어 \\\\ 경로\"; // 근거\n".repeat(120);
    std::fs::write(dir.path().join("main.js"), &text).unwrap();
    let mut s = session(dir.path());
    s.config.model = "z-ai/glm-5.3-flash".into();
    s.config.result_tokens = 1200;
    s.active_tools.insert("file_read".into());
    let call = mnemoarc::llm::ToolCall {
        id: "file-1".into(),
        name: "file_read".into(),
        arguments: json!({"path":"main.js","max_lines":120}).to_string(),
    };
    let output = tools::run_call(&mut s, &call);
    assert_eq!(output["status"], "ok");
    assert!(tools::result_tokens(&call, &output, &s.config.model) <= 1200);
    let original_source = s.sources[output["data"]["source"]["id"].as_str().unwrap()].clone();
    let reduced = tools::limit_result(&mut s, &call, output, 600);
    assert!(tools::result_tokens(&call, &reduced, &s.config.model) <= 600);
    assert!(reduced["data"]["preview"].is_null());
    let archive_count = s.history.bundles.len();
    let twice = tools::limit_result(&mut s, &call, reduced.clone(), 400);
    assert!(tools::result_tokens(&call, &twice, &s.config.model) <= 400);
    assert_eq!(s.history.bundles.len(), archive_count);

    assert!(reduced["data"]["source"]["id"].is_string(), "{reduced}");
    let shown = reduced["data"]["content"]["text"].as_str().unwrap();
    assert!(!shown.is_empty());
    assert!(text.starts_with(shown));
    assert_eq!(reduced["data"]["next_offset"], shown.chars().count());
    let start = reduced["data"]["content"]["line_start"].as_u64().unwrap() as usize;
    let end = start + shown.lines().count() - 1;
    assert_eq!(
        reduced["data"]["source"]["end_line"], end,
        "source range must describe delivered text, not the archived longer read"
    );
    let source = s
        .sources
        .get(reduced["data"]["source"]["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(source.end_line, Some(end));
    assert_ne!(source.id, original_source.id);
    assert_eq!(
        s.sources[&original_source.id].end_line,
        original_source.end_line
    );
    assert_eq!(
        s.sources[&original_source.id].excerpt,
        original_source.excerpt
    );
    assert_eq!(reduced["data"]["content"]["line_end"], end);
    assert_eq!(reduced["data"]["content"]["first_line_complete"], true);
    let boundary_complete =
        shown.ends_with('\n') || text.chars().nth(shown.chars().count()) == Some('\n');
    assert_eq!(
        reduced["data"]["content"]["last_line_complete"],
        boundary_complete
    );

    assert_eq!(source.excerpt, shown.chars().take(2000).collect::<String>());

    let next = mnemoarc::llm::ToolCall {
        id: "file-2".into(),
        name: "file_read".into(),
        arguments:
            json!({"path":"main.js","max_lines":120,"offset":reduced["data"]["next_offset"]})
                .to_string(),
    };
    let continuation = tools::run_call(&mut s, &next);
    let continued_text = continuation["data"]["content"]["text"].as_str().unwrap();
    let continued_start = continuation["data"]["content"]["line_start"]
        .as_u64()
        .unwrap() as usize;
    let continued_end = continued_start + continued_text.lines().count() - 1;
    assert_eq!(continuation["data"]["source"]["end_line"], continued_end);
    assert_eq!(
        continuation["data"]["content"]["first_line_complete"],
        shown.ends_with('\n')
    );
    let combined = format!(
        "{}{}",
        shown,
        continuation["data"]["content"]["text"].as_str().unwrap()
    );
    assert!(text.starts_with(&combined));
}

#[test]
fn relimited_crlf_read_does_not_certify_a_split_line_ending() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("crlf.txt"), "line body\r\n".repeat(80)).unwrap();
    let mut s = session(dir.path());
    let call = mnemoarc::llm::ToolCall {
        id: "crlf-budget".into(),
        name: "file_read".into(),
        arguments: json!({"path":"crlf.txt","max_lines":80}).to_string(),
    };
    let full = tools::run_call(&mut s, &call);
    assert_eq!(full["status"], "ok");
    let mut found = false;
    for budget in 300..=450 {
        let page = tools::limit_result(&mut s, &call, full.clone(), budget);
        if page["data"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.ends_with('\r'))
        {
            found = true;
            assert_eq!(page["data"]["content"]["last_line_complete"], false);
            assert_eq!(page["data"]["source"]["line_end_complete"], false);
            break;
        }
    }
    assert!(found, "expected a budget to split a CRLF line ending");
}

#[test]
fn history_continuations_do_not_recursively_archive_previews() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.config.model = "unknown-model".into();
    s.config.result_tokens = 500;
    let id = s.history.push(
        vec![json!({"role":"tool_archive","result":{"text":"\\\"한국어\\\"\n".repeat(400)}})],
        true,
    );
    s.history.bundles.back_mut().unwrap().active = false;
    let original = serde_json::to_string(&s.history.read(id).unwrap().messages).unwrap();
    let mut joined = String::new();
    let mut offset = 0;
    for index in 0..100 {
        let call = mnemoarc::llm::ToolCall {
            id: format!("page-{index}"),
            name: "history".into(),
            arguments: json!({"action":"read","id":id,"offset":offset}).to_string(),
        };
        let output = tools::run_call(&mut s, &call);
        assert!(tools::result_tokens(&call, &output, &s.config.model) <= 500);
        let chunk = output["data"]["text"].as_str().unwrap();
        joined.push_str(chunk);
        if !output["data"]["truncated"].as_bool().unwrap() {
            break;
        }
        let next = output["data"]["next_offset"].as_u64().unwrap();
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(joined, original);
    assert_eq!(s.history.bundles.len(), 1);
}

#[test]
fn checkpoint_cannot_execute_hidden_source_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.active_tools.insert("file_read".into());
    s.add_user("Keep constraints".into());
    s.add_user("Latest work".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let error = tools::execute(&mut s, "file_read", json!({"path":"main.rs"})).unwrap_err();
    assert!(error.to_string().contains("checkpoint_pending"));
}

#[test]
fn compatible_tool_integer_strings_are_normalized_without_loose_coercion() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "first\nsecond\nthird\n").unwrap();
    let mut s = session(dir.path());
    s.active_tools.insert("file_read".into());
    let value = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"main.rs","start_line":"2","max_lines":"1","offset":"0"}),
    )
    .unwrap();
    assert_eq!(value["content"]["text"], "second");
    for invalid in ["-1", "2.5", "1e2", " 2", "18446744073709551616", ""] {
        let err = tools::execute(
            &mut s,
            "file_read",
            json!({"path":"main.rs","start_line":invalid}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid_argument_type"));
    }
}

#[test]
fn catalog_finds_spaced_tool_names_and_new_sessions_can_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    assert!(s.task.deliverables.is_empty());
    assert!(s.active_tools.contains("file_read"));
    assert!(s.active_tools.contains("document_inspect"));
    // Simple edits run in answer; source-document tools stay off.
    assert!(s.active_tools.contains("document_edit"));
    assert!(!s.active_tools.contains("document_audit"));
    std::fs::write(
        dir.path().join("nav.rs"),
        "fn target() { println!(\"found\"); }\n",
    )
    .unwrap();
    let search = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"nav.rs","query":"target"}),
    )
    .unwrap();
    assert_eq!(search["total_matching_lines"], 1);
    let outline = tools::execute(
        &mut s,
        "code_outline",
        json!({"path":"nav.rs","query":"target","match":"exact"}),
    )
    .unwrap();
    let body = tools::execute(
        &mut s,
        "symbol_read",
        json!({"path":"nav.rs","symbol_id":outline["symbols"][0]["symbol_id"],"max_lines":5}),
    )
    .unwrap();
    assert!(body["content"]["text"].as_str().unwrap().contains("found"));
    for (query, expected) in [
        ("file read", "file_read"),
        ("document inspect edit", "document_inspect"),
        ("DOCUMENT_EDIT", "document_edit"),
    ] {
        let result = tools::execute(&mut s, "tool_catalog", json!({"query":query})).unwrap();
        assert!(
            result["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == expected)
        );
    }
}

#[test]
fn source_ids_are_short_and_unknown_sources_give_recovery_without_saving() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), "fn main() {}\n").unwrap();
    let mut s = session(dir.path());
    let read = tools::execute(&mut s, "file_read", json!({"path":"sample.rs"})).unwrap();
    let id = read["source"]["id"].as_str().unwrap();
    assert!(id.starts_with('S') && id.len() < 16);
    let err = tools::execute(&mut s, "memory_write", json!({"title":"test","summary":"test","body":"test","kind":"fact","source_ids":["S-invalid"]})).unwrap_err().to_string();
    assert!(err.contains("unknown_source") && err.contains(id) && err.contains("sample.rs"));
    assert!(s.memory.entries.is_empty());
    let unsourced = tools::execute(
        &mut s,
        "memory_write",
        json!({"title":"test","summary":"test","body":"test","kind":"fact"}),
    )
    .unwrap();
    assert_eq!(unsourced["status"], "needs_review");
    let sourced = tools::execute(&mut s, "memory_write", json!({"key":"fact","title":"test","summary":"test","body":"test","kind":"fact","source_ids":[id]})).unwrap();
    assert_eq!(sourced["status"], "active");
    assert!(tools::execute(&mut s, "memory_write", json!({"key":"fact","expected_revision":1,"title":"test","summary":"test","body":"changed","kind":"fact"})).unwrap_err().to_string().contains("memory_sources_required"));
}

#[test]
fn directory_read_has_actionable_recovery_and_search_distinguishes_no_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.js"),
        "export function handleQuestion() {}\n",
    )
    .unwrap();
    let mut s = session(dir.path());
    s.active_tools = tools::ToolRegistry::optional_names();
    let error = tools::execute(&mut s, "file_read", json!({"path":"."}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("path_is_directory") && error.contains("file_list"));
    let bad = tools::execute(
        &mut s,
        "symbol_search",
        json!({"pattern":"^export (async )?function","query":"agent.js"}),
    )
    .unwrap_err()
    .to_string();
    assert!(bad.contains("invalid_path_glob"));
    let found = tools::execute(
        &mut s,
        "symbol_search",
        json!({"path_glob":"main.js","query":"handle"}),
    )
    .unwrap();
    assert_eq!(found["matched_files"], 1);
    assert_eq!(found["scanned_files"], 1);
    assert_eq!(found["symbols"].as_array().unwrap().len(), 1);
    let no_symbol = tools::execute(
        &mut s,
        "symbol_search",
        json!({"path_glob":"main.js","query":"missing"}),
    )
    .unwrap();
    assert_eq!(no_symbol["matched_files"], 1);
    assert!(no_symbol["symbols"].as_array().unwrap().is_empty());
    let no_file =
        tools::execute(&mut s, "symbol_search", json!({"path_glob":"missing.js"})).unwrap();
    assert_eq!(no_file["matched_files"], 0);
    let legacy = tools::execute(
        &mut s,
        "symbol_search",
        json!({"pattern":"main.js","query":"handle"}),
    )
    .unwrap();
    assert_eq!(legacy["symbols"][0]["name"], found["symbols"][0]["name"]);
}

#[test]
fn source_id_allocation_is_unique_across_workers() {
    let workers: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                (0..100)
                    .map(|_| mnemoarc::memory::source_id())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let ids: BTreeSet<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    assert_eq!(ids.len(), 800);
}

#[test]
fn continuation_preserves_progress_but_new_task_resets_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.task.phase = "draft".into();
    s.task_rounds = 12;
    s.add_user("계속 진행".into());
    assert_eq!(s.task.phase, "draft");
    assert_eq!(s.task_rounds, 12);
    s.add_user("다른 문서를 조사해줘".into());
    assert_eq!(s.task.phase, "");
    assert_eq!(s.task_rounds, 0);
}

#[test]
fn continuation_keeps_the_original_request_for_checkpointed_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let original =
        "Document the routes, preserving the authentication and error-handling requirements";
    s.add_user(original.into());
    let criteria = s.task.completion.clone();
    // The original prompt can leave the active context after a checkpoint.
    for bundle in &mut s.history.bundles {
        bundle.active = false;
        bundle.reviewed = true;
    }
    for continuation in ["계속 진행", "continue!", "resume"] {
        s.add_user(continuation.into());
        assert_eq!(s.latest_request, original);
        assert_eq!(s.task.completion, criteria);
        assert_eq!(
            s.history.bundles.back().unwrap().messages[0]["content"],
            continuation
        );
        assert!(
            ContextManager::state(&s)
                .unwrap()
                .to_string()
                .contains(original)
        );
    }
    s.start_new_task("continue".into());
    assert_eq!(s.latest_request, "continue");
    assert_ne!(s.task.completion, criteria);
}

#[test]
fn first_prompt_preserves_prepared_completion_and_new_tasks_start_with_criteria() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.select_workflow("source_document").unwrap();
    s.task.constraints.push("Use Korean".into());
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"completion":["Check every requested flow"],"deliverables":["Report"]}}),
    )
    .unwrap();
    let prepared_revision = s.task.revision;
    s.add_user("Document the routes".into());
    assert_eq!(s.task.completion, ["Check every requested flow"]);
    assert_eq!(s.task.deliverables, ["Report"]);
    assert_eq!(
        s.request_review_criteria.completion,
        ["Check every requested flow"]
    );
    assert_eq!(s.request_review_criteria.constraints, ["Use Korean"]);
    assert_eq!(s.request_review_criteria.deliverables, ["Report"]);
    assert_eq!(s.task.workflow, "source_document");
    assert!(s.task.revision > prepared_revision);
    s.add_user("계속 진행".into());
    assert_eq!(s.task.completion, ["Check every requested flow"]);
    s.task
        .constraints
        .push("Record every investigation ID in the document".into());
    s.add_user("Explain a different module".into());
    assert_eq!(s.task.completion.len(), 1);
    assert!(s.task.completion[0].contains("Explain a different module"));
    assert!(s.task.deliverables.is_empty());
    assert!(s.request_review_criteria.completion.is_empty());
    assert_eq!(s.request_review_criteria.constraints, ["Use Korean"]);
    assert!(s.request_review_criteria.deliverables.is_empty());
    // The session's selection carries over to the new task.
    assert_eq!(s.task.workflow, "source_document");
}

#[test]
fn completion_cannot_be_emptied_by_task_state_update() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("Document the routes".into());
    let original = s.task.completion.clone();
    let revision = s.task.revision;
    let error = tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"completion":[]}}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("completion_required"));
    assert_eq!(s.task.completion, original);
    assert_eq!(s.task.revision, revision);
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"findings":["Reading route code"]}}),
    )
    .unwrap();
    assert_eq!(s.task.completion, original);
}

#[test]
fn the_model_cannot_change_the_selected_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    assert_eq!(s.task.workflow, "answer");
    for patch in [
        json!({"workflow":"source_document"}),
        json!({"workflow":"source_document","scope":"UI"}),
    ] {
        let error = tools::execute(
            &mut s,
            "task_state",
            json!({"action":"update","patch":patch}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("workflow_selected_by_user:"), "{error}");
    }
    assert!(!s.is_document_work());
    assert!(s.select_workflow("").is_err());
}

#[test]
fn answer_workflow_document_edit_skips_document_verification() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("Add a note to the summary".into());
    assert_eq!(s.task.workflow, "answer");
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Note\nPlain edit.\n"}),
    )
    .unwrap();
    assert!(s.document_written);
    assert!(!s.is_document_work());
    assert!(!tools::completion_review::required(&s));
    // The same write is verified in a document workflow.
    s.select_workflow("source_document").unwrap();
    assert!(s.is_document_work());
    assert!(tools::completion_review::required(&s));
}

#[test]
fn answer_workflow_runs_no_reviews_and_hides_review_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.select_workflow("source_document").unwrap();
    assert!(s.active_tools.contains("document_audit"));
    s.select_workflow("answer").unwrap();
    s.add_user("Plan and answer".into());
    // Plans, deliverables and writes do not start a completion review.
    tools::execute(&mut s, "task_plan", json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":["Answer the question"]}]})).unwrap();
    s.task.deliverables = vec!["answer".into()];
    assert!(s.config.completion_review_enabled);
    assert!(!tools::completion_review::required(&s));
    let name = "document_audit";
    {
        assert!(!s.active_tools.contains(name), "{name}");
        assert!(
            !tools::ToolRegistry::definitions(&s)
                .iter()
                .any(|t| t["function"]["name"] == name),
            "{name}"
        );
    }
    let error = tools::execute(
        &mut s,
        "tool_select",
        json!({"action":"add","names":["document_audit"]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("workflow_forbidden:"), "{error}");
    // The same task is reviewed in source_document.
    s.select_workflow("source_document").unwrap();
    assert!(tools::completion_review::required(&s));
}

#[test]
fn answer_workflow_sends_no_review_or_verification_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("Add a note to the summary".into());
    let review_terms = [
        "completion_review",
        "document_review",
        "unread_citation",
        "citation_check",
    ];
    let request = ContextManager::request(&s, tools::ToolRegistry::definitions(&s)).unwrap();
    let text = request.to_string();
    for term in review_terms {
        assert!(!text.contains(term), "{term}");
    }
    let state = ContextManager::state(&s).unwrap();
    for key in ["completion_review", "document_review"] {
        assert!(state.get(key).is_none(), "{key}");
    }
    let written = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Note\nPlain edit.\n"}),
    )
    .unwrap();
    for key in [
        "verification_required_ids",
        "verification_guidance",
        "citation_check",
    ] {
        assert!(written.get(key).is_none(), "{key}");
    }
    assert!(written["hash"].is_string());
    // Source documentation keeps the full verification prompt.
    s.select_workflow("source_document").unwrap();
    assert_eq!(context::system_prompt(&s), context::SYSTEM);
}

#[test]
fn answer_workflow_withholds_document_audit() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.select_workflow("source_document").unwrap();
    assert!(s.active_tools.contains("document_audit"));
    // Switching to answer drops it from the active and queued selections.
    s.pending_tools = Some(s.active_tools.clone());
    s.select_workflow("answer").unwrap();
    assert!(!s.active_tools.contains("document_audit"));
    assert!(!s.pending_tools.as_ref().unwrap().contains("document_audit"));
    assert!(
        tools::execute(&mut s, "document_audit", json!({}))
            .unwrap_err()
            .to_string()
            .starts_with("tool_not_active:")
    );
    // The model cannot select it by name, and the group skips it.
    let error = tools::execute(
        &mut s,
        "tool_select",
        json!({"action":"add","names":["document_audit"]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("workflow_forbidden:"), "{error}");
    let selected = tools::execute(
        &mut s,
        "tool_select",
        json!({"action":"add","names":["source-docs"]}),
    )
    .unwrap();
    let pending = selected["pending"].as_array().unwrap();
    assert!(pending.iter().any(|n| n == "document_edit"));
    assert!(!pending.iter().any(|n| n == "document_audit"));
    // A selection made under another workflow is filtered at the boundary.
    let mut names = tools::ToolRegistry::optional_names();
    assert!(tools::ToolRegistry::validate_tool_selection(&s, &names).is_err());
    names = tools::ToolRegistry::normalize_tool_selection(&s, &names);
    assert!(!names.contains("document_audit"));
    // A new request keeps it withheld.
    s.active_tools.insert("document_audit".into());
    s.add_user("Explain main".into());
    assert!(!s.active_tools.contains("document_audit"));
}

#[test]
fn long_user_request_keeps_a_bounded_completion_and_full_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let request = "사용자 요구 조건과 예외 처리 확인. ".repeat(2000);
    s.add_user(request.clone());
    assert_eq!(s.latest_request, request);
    assert!(s.task.completion[0].contains("latest_request"));
    assert!(ContextManager::state(&s).is_ok());
}

#[test]
fn relimiting_reads_preserves_immutable_cursor_positions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pages.md"),
        "정확한 커서 위치😀 and source evidence\n".repeat(200),
    )
    .unwrap();
    let mut s = session(dir.path());
    s.config.result_tokens = 1000;
    let call = mnemoarc::llm::ToolCall {
        id: "repeated-position".into(),
        name: "file_read".into(),
        arguments: json!({"path":"pages.md","max_lines":200}).to_string(),
    };
    for _ in 0..4 {
        // Bypass the call-ID ledger: these are fresh reads of the same range.
        let raw = tools::execute(
            &mut s,
            "file_read",
            serde_json::from_str(&call.arguments).unwrap(),
        )
        .unwrap();
        let original = tools::limit_result(&mut s, &call, tools::envelope(Ok(raw)), 1000);
        let original_id = original["next_cursor"]["cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        let original_offset = s.file_cursors[&original_id].offset;
        let narrowed = tools::limit_result(&mut s, &call, original, 700);
        assert!(tools::result_tokens(&call, &narrowed, &s.config.model) <= 700);
        assert_eq!(s.file_cursors[&original_id].offset, original_offset);
        let cursor = narrowed["next_cursor"]["cursor"].as_str().unwrap();
        assert_eq!(
            s.file_cursors[cursor].offset,
            narrowed["data"]["next_offset"].as_u64().unwrap() as usize
        );
    }
}

#[test]
fn opaque_file_cursor_survives_relimiting_without_skips_or_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<_> = (1..=50)
        .map(|i| format!("{i:03}: {}", "한글😀 exact position ".repeat(6)))
        .collect();
    std::fs::write(dir.path().join("pages.md"), lines.join("\n")).unwrap();
    let expected = lines[6..31].join("\n");
    let mut s = session(dir.path());
    s.config.result_tokens = 1400;
    let mut args = json!({"path":"pages.md","start_line":7,"max_lines":25});
    let mut joined = String::new();
    let mut completed = false;
    for page in 0..100 {
        let call = mnemoarc::llm::ToolCall {
            id: format!("opaque-{page}"),
            name: "file_read".into(),
            arguments: args.to_string(),
        };
        let original = tools::run_call(&mut s, &call);
        assert_eq!(original["status"], "ok", "{original}");
        let original_cursor = original["next_cursor"]["cursor"]
            .as_str()
            .map(str::to_owned);
        let original_position = original_cursor.as_ref().map(|id| s.file_cursors[id].offset);
        let result = tools::limit_result(&mut s, &call, original, 800);
        assert!(tools::result_tokens(&call, &result, &s.config.model) <= 800);
        let text = result["data"]["content"]["text"]
            .as_str()
            .expect("must retain text");
        assert!(!text.is_empty());
        joined.push_str(text);
        assert!(
            expected.starts_with(&joined),
            "a cursor skipped or repeated content on page {page}"
        );
        assert_eq!(result["data"]["read_start"], 7);
        if let Some(id) = original_cursor {
            assert_eq!(
                s.file_cursors[&id].offset,
                original_position.unwrap(),
                "relimiting must not mutate previously issued cursors"
            );
        }
        if result["next_cursor"].is_null() {
            completed = true;
            assert_eq!(result["data"]["next_line"], 32);
            break;
        }
        assert_eq!(result["next_cursor"].as_object().unwrap().len(), 2);
        args = json!({"cursor":result["next_cursor"]["cursor"]});
    }
    assert!(completed);
    assert_eq!(joined, expected);
    let definition = tools::ToolRegistry::definitions(&s)
        .into_iter()
        .find(|t| t["function"]["name"] == "file_read")
        .unwrap();
    assert!(
        definition["function"]["parameters"]["properties"]
            .get("offset")
            .is_none()
    );
}

#[test]
fn file_cursors_reject_mixed_ranges_changed_files_and_other_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages.md");
    std::fs::write(&path, "긴 문서와 정확한 위치😀\n".repeat(200)).unwrap();
    let mut s = session(dir.path());
    s.config.result_tokens = 1000;
    let call = mnemoarc::llm::ToolCall {
        id: "initial".into(),
        name: "file_read".into(),
        arguments: json!({"path":"pages.md","max_lines":200}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    let cursor = result["next_cursor"]["cursor"].clone();
    assert!(cursor.is_string());
    // An explicit start line on the cursor's file reads that new range.
    let ranged = tools::execute(
        &mut s,
        "file_read",
        json!({"cursor":cursor,"path":"pages.md","start_line":90,"max_lines":5}),
    )
    .unwrap();
    assert_eq!(ranged["read_start"], 90);
    assert_eq!(ranged["read_max_lines"], 5);
    // Other mixes stay ambiguous and are rejected.
    std::fs::write(dir.path().join("other.md"), "x\n").unwrap();
    for extra in [
        json!({"start_line":90,"offset":0}),
        json!({"offset":0}),
        json!({"path":"other.md"}),
    ] {
        let mut args = extra;
        args["cursor"] = cursor.clone();
        assert!(
            tools::execute(&mut s, "file_read", args)
                .unwrap_err()
                .to_string()
                .contains("cursor_arguments_conflict")
        );
    }
    // A page size with a cursor changes nothing: the cursor continues its
    // original range and the ignored argument is reported.
    let continued =
        tools::execute(&mut s, "file_read", json!({"cursor":cursor,"max_lines":20})).unwrap();
    assert_eq!(continued["ignored_arguments"], json!(["max_lines"]));
    assert_eq!(continued["read_start"], result["data"]["read_start"]);
    assert_eq!(
        continued["read_max_lines"],
        result["data"]["read_max_lines"]
    );
    assert!(continued["read_offset"].as_u64().unwrap() > 0);
    // The same with the cursor's own path, as a live run sent it.
    let with_path = tools::execute(
        &mut s,
        "file_read",
        json!({"cursor":cursor,"path":"pages.md","max_lines":20}),
    )
    .unwrap();
    assert_eq!(with_path["ignored_arguments"], json!(["max_lines"]));
    assert_eq!(with_path["read_offset"], continued["read_offset"]);
    let mut other = session(dir.path());
    assert!(
        tools::execute(&mut other, "file_read", json!({"cursor":cursor}))
            .unwrap_err()
            .to_string()
            .contains("invalid_file_cursor")
    );
    std::fs::write(&path, "changed").unwrap();
    assert!(
        tools::execute(&mut s, "file_read", json!({"cursor":cursor}))
            .unwrap_err()
            .to_string()
            .contains("file_cursor_expired")
    );
    let err = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"pages.md","start_line":1,"max_lines":10,"offset":8900}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("8900 exceeds 7") && err.contains("omit offset"));
}

#[test]
fn a_whole_next_cursor_sent_as_the_cursor_continues_the_read() {
    // A live model sent the whole next_cursor object as the cursor string
    // and the read failed as an invalid cursor.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pages.md"),
        "긴 문서와 정확한 위치😀\n".repeat(200),
    )
    .unwrap();
    let mut s = session(dir.path());
    s.config.result_tokens = 1000;
    let call = mnemoarc::llm::ToolCall {
        id: "initial".into(),
        name: "file_read".into(),
        arguments: json!({"path":"pages.md","max_lines":200}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    let next = result["next_cursor"].clone();
    let cursor = next["cursor"].as_str().unwrap().to_owned();
    let expected = tools::execute(&mut s, "file_read", json!({"cursor":cursor})).unwrap();
    // As JSON text and as an object.
    for wrapped in [json!(next.to_string()), next.clone()] {
        let continued = tools::execute(&mut s, "file_read", json!({"cursor":wrapped})).unwrap();
        assert_eq!(continued["read_offset"], expected["read_offset"]);
        assert_eq!(continued["content"], expected["content"]);
    }
    // Another tool's continuation is not this read's cursor.
    let other = json!({"tool":"code_outline","path":"pages.md","cursor":cursor}).to_string();
    assert!(
        tools::execute(&mut s, "file_read", json!({"cursor":other}))
            .unwrap_err()
            .to_string()
            .contains("invalid_file_cursor")
    );
}

#[test]
fn a_cursor_dropped_for_a_start_line_is_reported_with_its_continuation() {
    // A start_line beside a cursor reads a new range. The result says that
    // the cursor was not continued and where it would have continued.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages.md");
    std::fs::write(&path, "긴 문서와 정확한 위치😀\n".repeat(200)).unwrap();
    let mut s = session(dir.path());
    s.config.result_tokens = 1000;
    let call = mnemoarc::llm::ToolCall {
        id: "initial".into(),
        name: "file_read".into(),
        arguments: json!({"path":"pages.md","max_lines":200}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    let cursor = result["next_cursor"]["cursor"].as_str().unwrap().to_owned();
    let next = tools::execute(&mut s, "file_read", json!({"cursor":cursor})).unwrap()["content"]
        ["line_start"]
        .as_u64()
        .unwrap();
    assert!(next > 1);
    let new_range = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"pages.md","cursor":cursor,"start_line":2,"max_lines":3}),
    )
    .unwrap();
    assert_eq!(new_range["read_start"], 2);
    assert_eq!(new_range["ignored_arguments"], json!(["cursor"]));
    let note = new_range["ignored_note"].as_str().unwrap();
    assert!(
        note.contains(&format!("{cursor} continues at line {next}"))
            && note.contains(&format!("{{\"cursor\":\"{cursor}\"}}")),
        "{note}"
    );
    // A new range from the line the cursor would continue at loses nothing.
    let from_next = tools::execute(
        &mut s,
        "file_read",
        json!({"cursor":cursor,"start_line":next,"max_lines":5}),
    )
    .unwrap();
    assert!(from_next.get("ignored_arguments").is_none(), "{from_next}");
    // The cursor of a changed file cannot be continued at all.
    std::fs::write(&path, "changed\n".repeat(20)).unwrap();
    let changed = tools::execute(
        &mut s,
        "file_read",
        json!({"cursor":cursor,"start_line":3,"max_lines":2}),
    )
    .unwrap();
    assert_eq!(changed["content"]["text"], "changed\nchanged");
    assert!(
        changed["ignored_note"]
            .as_str()
            .unwrap()
            .contains("its file changed after it was issued"),
        "{changed}"
    );
}

#[test]
fn a_cursor_beside_a_placeholder_first_line_is_continued() {
    // A provider that fills every field sends each continuation as its
    // cursor beside start_line 1 and a one-line page. Read as a new range,
    // that returned line 1 alone: a live run sent the same call again and
    // gave up on another cursor.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pages.md"),
        "긴 문서와 정확한 위치😀\n".repeat(200),
    )
    .unwrap();
    let mut s = session(dir.path());
    s.config.result_tokens = 1000;
    let call = mnemoarc::llm::ToolCall {
        id: "initial".into(),
        name: "file_read".into(),
        arguments: json!({"path":"pages.md","max_lines":200}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    let cursor = result["next_cursor"]["cursor"].as_str().unwrap().to_owned();
    let continued = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"pages.md","cursor":cursor,"start_line":1,"max_lines":1,"limit":1,"force_read":false}),
    )
    .unwrap();
    let alone = tools::execute(&mut s, "file_read", json!({"cursor":cursor})).unwrap();
    assert_eq!(continued["content"], alone["content"], "{continued}");
    let next = continued["content"]["line_start"].as_u64().unwrap();
    assert!(next > 1, "{continued}");
    assert_eq!(
        continued["ignored_arguments"],
        json!(["start_line", "max_lines"])
    );
    let note = continued["ignored_note"].as_str().unwrap();
    assert!(
        note.contains(&format!("{cursor} was continued from line {next}")),
        "{note}"
    );
    // A one-line read of any other line is a real new range.
    let line = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"pages.md","cursor":cursor,"start_line":5,"max_lines":1}),
    )
    .unwrap();
    assert_eq!(line["read_start"], 5);
    assert_eq!(line["ignored_arguments"], json!(["cursor"]));
}

#[test]
fn file_read_limit_alias_preserves_ranges_and_rejects_ambiguity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("read.md"), "one\ntwo\nthree\nfour\n").unwrap();
    let mut s = session(dir.path());
    for args in [
        json!({"path":"read.md","start_line":2,"limit":2}),
        json!({"path":"read.md","start_line":"2","limit":"2","max_lines":2}),
        json!({"path":"read.md","start_line":2,"limit":0,"max_lines":2}),
        json!({"path":"read.md","start_line":2,"limit":2,"max_lines":0}),
    ] {
        let result = tools::execute(&mut s, "file_read", args).unwrap();
        assert_eq!(result["content"]["text"], "two\nthree");
        assert_eq!(result["content"]["line_start"], 2);
        assert_eq!(result["content"]["line_end"], 3);
    }
    for (args, error) in [
        (
            json!({"path":"read.md","limit":1,"max_lines":2}),
            "conflicting_arguments",
        ),
        (
            json!({"path":"read.md","start_line":160,"offset":"160","limit":"140"}),
            "ambiguous_file_read_range",
        ),
        (
            json!({"path":"read.md","limit":"2.5"}),
            "invalid_argument_type",
        ),
        (json!({"cursor":"R0","limit":2}), "cursor"),
    ] {
        assert!(
            tools::execute(&mut s, "file_read", args)
                .unwrap_err()
                .to_string()
                .contains(error)
        );
    }
    let error = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"read.md","line_count":2}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("allowed arguments") && error.contains("max_lines"));
}

#[test]
fn checkpoint_source_lookup_returns_only_existing_matching_evidence() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn entry() {}\n").unwrap();
    let mut s = session(dir.path());
    let read = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":1,"max_lines":1}),
    )
    .unwrap();
    let id = read["source"]["id"].as_str().unwrap();
    tools::execute(&mut s, "memory_write", json!({"title":"Entry", "summary":"Observed", "kind":"fact", "body":"An entry exists", "source_ids":[id]})).unwrap();
    s.sources.clear(); // Evidence stored in memory must remain discoverable.
    s.add_user("Preserve findings".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let found = tools::execute(&mut s, "source_lookup", json!({"path":"a.rs","limit":1})).unwrap();
    assert_eq!(found["items"][0]["id"], id);
    assert_eq!(found["items"][0]["start_line"], 1);
    assert_eq!(found["total"], 1);
    let missing = tools::execute(&mut s, "source_lookup", json!({"path":"unseen.rs"})).unwrap();
    assert_eq!(missing["total"], 0);
    let third = tools::execute(&mut s, "source_lookup", json!({"id":id,"path":"a.rs"})).unwrap();
    assert_eq!(third["checkpoint_lookup_remaining"], 0);
    assert!(
        !tools::ToolRegistry::definitions(&s)
            .iter()
            .any(|tool| tool["function"]["name"] == "source_lookup")
    );
    let err = tools::execute(&mut s, "source_lookup", json!({"id":id}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("checkpoint_lookup_limit"), "{err}");
    assert!(tools::execute(&mut s, "file_read", json!({"path":"a.rs"})).is_err());
    assert!(
        s.source_refs(&["S93-missing".into()])
            .unwrap_err()
            .to_string()
            .contains("source_lookup")
    );
    let mut old = serde_json::to_value(s.checkpoint.as_ref().unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("failed_attempts");
    old.as_object_mut().unwrap().remove("last_failure");
    let restored: mnemoarc::session::Checkpoint = serde_json::from_value(old).unwrap();
    assert_eq!(restored.failed_attempts, 0);
    assert!(restored.last_failure.is_none());
}

#[test]
fn oversized_tool_error_keeps_cause_and_archive_for_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    let call = mnemoarc::llm::ToolCall {
        id: "error".into(),
        name: "memory_write".into(),
        arguments: "{}".into(),
    };
    let error = format!(
        "unknown_source: S93-missing; {}",
        "long recovery details ".repeat(500)
    );
    let limited = tools::limit_result(&mut s, &call, json!({"status":"error","error":error}), 200);
    assert_eq!(limited["status"], "error");
    assert!(
        limited["error"]
            .as_str()
            .unwrap()
            .starts_with("unknown_source: S93-missing")
    );
    assert_eq!(limited["next_cursor"]["tool"], "history");
    assert!(tools::result_tokens(&call, &limited, &s.config.model) <= 200);
}

#[test]
fn leaked_native_tool_call_markup_is_split_into_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // The live shape: the tags value and the title key fused into one key
    // that holds the title, with and without an <arg_value> opener.
    for (key, leaked) in [
        (
            "first",
            r#"tags</arg_key> ["manual","frontend"]</arg_value><arg_key>title"#,
        ),
        (
            "second",
            r#"tags</arg_key><arg_value>["manual","frontend"]</arg_value><arg_key>title"#,
        ),
    ] {
        let mut args = json!({"key":key,"summary":"s","body":"b","kind":"fact"});
        args[leaked] = json!("Frontend facts");
        tools::execute(&mut s, "memory_write", args).unwrap();
        let memory = s.memory.get(key).unwrap();
        assert_eq!(memory.title, "Frontend facts");
        assert_eq!(memory.tags, ["manual", "frontend"]);
    }
    // A repaired call that still fails names the fields that did arrive, so
    // the model resends the missing one instead of repeating the call.
    s.active_tools = tools::ToolRegistry::optional_names();
    let error = tools::execute(
        &mut s,
        "symbol_relations",
        json!({"path</arg_key> a.rs</arg_value><arg_key>relation":"callers"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("missing_argument: symbol_id"), "{error}");
    assert!(
        error.contains("only these fields were received: path, relation"),
        "{error}"
    );
    // An ordinary failure without leaked markup carries no such note.
    let error = tools::execute(
        &mut s,
        "symbol_relations",
        json!({"path":"a.rs","relation":"callers"}),
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("<arg_key>"), "{error}");
    // A leaked value that disagrees with an explicit field is not dropped.
    let error = tools::execute(
        &mut s,
        "memory_write",
        json!({"key":"third","title":"Other","summary":"s","body":"b","kind":"fact",
            "title</arg_key>Leaked</arg_value><arg_key>tags":["x"]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("conflicting_arguments: title"), "{error}");
    // Markup that does not parse completely stays an unknown argument.
    let error = tools::execute(
        &mut s,
        "memory_write",
        json!({"key":"fourth","title":"t","summary":"s","body":"b","kind":"fact","tags</arg_key>[\"x\"]":"y"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("unknown_argument:"), "{error}");
}

#[test]
fn type_errors_name_the_expected_and_received_type() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // The live shape: patch as JavaScript-like text with an unquoted key.
    let error = tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":"{completion:[\"check\"]}"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("invalid_argument_type: patch must be object, got string"),
        "{error}"
    );
    assert!(error.contains("object keys need double quotes"), "{error}");
    let error = tools::execute(&mut s, "task_state", json!({"action":"update","patch":3}))
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("invalid_argument_type: patch must be object, got number"),
        "{error}"
    );
    assert!(error.contains("correct this field"), "{error}");
}

#[test]
fn an_invented_cursor_explains_how_to_page() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "button\nonClick\n").unwrap();
    let mut s = session(dir.path());
    // The live shape: a cursor made up as JSON text.
    let error = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"a.js","queries":["button"],"cursor":"{\"action\": \"read\", \"id\": 5}"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("invalid_cursor: "), "{error}");
    assert!(error.contains("copy next_cursor exactly"), "{error}");
}

#[test]
fn explicit_null_optional_read_arguments_are_treated_as_omitted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "button\n").unwrap();
    let mut s = session(dir.path());
    // The live shape: every optional field of a read-only tool spelled out,
    // unused ones as null. Write tools keep rejecting null.
    let result = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"a.js","query":"button","cursor":null,"queries":null,"limit":10}),
    )
    .unwrap();
    assert!(result.to_string().contains("button"), "{result}");
    // A required argument given as null is still reported.
    let error = tools::execute(&mut s, "file_read", json!({"path":null}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("path"), "{error}");
}

#[test]
fn a_bare_task_state_update_shows_the_call_to_send() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // The live shape: {"action":"update"} repeated with nothing else.
    let error = tools::execute(&mut s, "task_state", json!({"action":"update"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("missing_argument: patch"), "{error}");
    assert!(
        error.contains(r#"{"action":"update","patch":{"completion":"#),
        "{error}"
    );
}

#[test]
fn no_tool_definition_offers_a_top_level_union() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.active_tools = tools::ToolRegistry::optional_names();
    // A top-level oneOf made a provider drop every argument but action.
    for definition in tools::ToolRegistry::definitions(&s) {
        assert!(
            definition["function"]["parameters"].get("oneOf").is_none(),
            "{}",
            definition["function"]["name"]
        );
        assert!(
            definition["function"]["parameters"].get("anyOf").is_none(),
            "{}",
            definition["function"]["name"]
        );
    }
}

#[test]
fn restating_the_workflow_is_ignored_but_reclassification_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    // Restating the current workflow selection is not a reclassification.
    let workflow = s.task.workflow.clone();
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":workflow,"scope":"UI"}}),
    )
    .unwrap();
    assert_eq!(s.task.scope, "UI");
    let other = if s.task.workflow == "answer" {
        "source_document"
    } else {
        "answer"
    };
    let err = tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":other}}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.starts_with("workflow_selected_by_user:"), "{err}");
}

#[test]
fn filled_history_memory_and_state_placeholders_are_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    tools::execute(
        &mut s,
        "history",
        json!({"action":"search","after":0,"id":0,"limit":5,"offset":0,"query":"x"}),
    )
    .unwrap();
    let memory = json!({"key":null,"expected_revision":null,"title":"Finding","summary":"Observed","body":"Detail","kind":"fact"});
    tools::execute(&mut s, "memory_write", memory).unwrap();
    let err = tools::execute(
        &mut s,
        "memory_write",
        json!({"key":"fresh","expected_revision":1,"title":"T","summary":"S","body":"B","kind":"fact"}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("omit expected_revision to create it"), "{err}");
    let summary = s.task.checkpoint_summary.clone();
    tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"checkpoint_summary":summary,"scope":"UI"}}),
    )
    .unwrap();
    let err = tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"checkpoint_summary":"invented"}}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("is not a task_state patch field; allowed patch fields:"),
        "{err}"
    );
}

#[test]
fn written_before_the_output_exists_and_stray_checkpoint_ack_explain_the_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.project.output = dir.path().join("manual.md");
    s.select_workflow("source_document").unwrap();
    let err = tools::execute(&mut s, "document_audit", json!({}))
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("document_missing:"), "{err}");
    assert!(err.contains("action=create"), "{err}");
    assert!(!err.contains("os error"), "{err}");
    let err = tools::execute(
        &mut s,
        "checkpoint_complete",
        json!({"id":"S1","progress":"Section 1 written","no_save_reason":"none pending"}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.starts_with("no_checkpoint:"), "{err}");
    assert!(err.contains("CHECKPOINT CONTROL REQUEST"), "{err}");
}

#[test]
fn checkpoint_completes_without_a_memory_write_or_no_save_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("a".into());
    s.add_user("b".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let cp = s.checkpoint.as_ref().unwrap().id.clone();
    let generation = s.memory.generation;
    // Saving a memory is optional: progress alone acknowledges the checkpoint.
    tools::execute(
        &mut s,
        "checkpoint_complete",
        json!({"id":cp,"progress":"Continue reading main.rs:20-40 for the retry bound"}),
    )
    .unwrap();
    assert_eq!(s.memory.generation, generation);
    ContextManager::commit(&mut s).unwrap();
    assert!(s.checkpoint.is_none());
    assert!(s.task.checkpoint_summary.contains("main.rs:20-40"));
}

#[test]
fn saved_checkpoint_progress_is_not_shown_as_a_pending_checkpoint() {
    // After a checkpoint cleared the context, a live model read its own
    // progress ("stopped source reads as the checkpoint instructed") as a
    // pending checkpoint and acknowledged it again under an invented id.
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    s.add_user("a".into());
    s.add_user("b".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    let cp = s.checkpoint.as_ref().unwrap().id.clone();
    tools::execute(
        &mut s,
        "checkpoint_complete",
        json!({"id":cp,"progress":"Stopped source reads as the checkpoint instructed; next audit the output"}),
    )
    .unwrap();
    // A pending checkpoint is named by its control request instead.
    let pending = ContextManager::state(&s).unwrap();
    assert!(pending.get("checkpoint_note").is_none(), "{pending}");
    ContextManager::commit(&mut s).unwrap();
    let request = ContextManager::request(&s, vec![]).unwrap();
    let state = request["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        state.contains("No checkpoint is pending")
            && state.contains("task.checkpoint_summary is the progress saved"),
        "{state}"
    );
}

#[test]
fn checkpoint_complete_is_offered_only_while_a_checkpoint_is_pending() {
    // Offered on every request, it was called to announce a finished task
    // with no checkpoint pending, once in each of three live runs.
    fn offered(s: &Session) -> bool {
        tools::ToolRegistry::definitions(s)
            .iter()
            .any(|t| t["function"]["name"] == "checkpoint_complete")
    }
    let dir = tempfile::tempdir().unwrap();
    let mut s = session(dir.path());
    assert!(!offered(&s));
    s.add_user("a".into());
    s.add_user("b".into());
    ContextManager::prepare(&mut s, 60000).unwrap();
    assert!(offered(&s));
    let cp = s.checkpoint.as_ref().unwrap().id.clone();
    tools::execute(
        &mut s,
        "checkpoint_complete",
        json!({"id":cp,"progress":"Continue with the next plan item"}),
    )
    .unwrap();
    ContextManager::commit(&mut s).unwrap();
    assert!(!offered(&s));
}
