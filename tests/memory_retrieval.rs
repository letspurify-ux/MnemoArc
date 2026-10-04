use mnemoarc::{
    config::{Config, Project},
    context::{self, ContextManager},
    memory::{MemoryInput, MemoryStatus, Source},
    session::{Session, TodoItem},
    tools,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

fn session() -> Session {
    Session::new(
        Project::default(),
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..Default::default()
        },
    )
}

fn save(s: &mut Session, key: &str, title: &str, summary: &str, body: &str) -> String {
    let input: MemoryInput = serde_json::from_value(json!({
        "key":key,"title":title,"summary":summary,"body":body,"kind":"decision"
    }))
    .unwrap();
    let memory = s.memory.save(input, vec![], &s.config).unwrap();
    // Do not rely on clock resolution for recency assertions.
    let timestamp = chrono::DateTime::from_timestamp(s.memory.entries.len() as i64, 0).unwrap();
    s.memory.entries.get_mut(&memory.id).unwrap().updated_at = timestamp;
    memory.id
}

fn keys(s: &Session, query: &str) -> Vec<String> {
    s.memory
        .search(query, &[])
        .into_iter()
        .map(|m| m.key.unwrap())
        .collect()
}

fn index(state: &Value) -> Value {
    json!({"recent_memories":state["recent_memories"],
        "related_memories":state["related_memories"],
        "referenced_memories":state["referenced_memories"]})
}

fn ids(state: &Value) -> Vec<String> {
    ["recent_memories", "related_memories", "referenced_memories"]
        .into_iter()
        .flat_map(|bucket| state[bucket].as_array().unwrap())
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn identifiers_keys_unicode_and_source_paths_are_searchable() {
    let mut s = session();
    let reader = save(
        &mut s,
        "io-policy",
        "memoryRead HTTPServer",
        "reader behavior",
        "Details",
    );
    let korean = save(&mut s, "korean", "기억 검색", "한글 요약", "본문");
    let paths = save(&mut s, "route", "Routing", "Details", "Details");
    s.memory
        .entries
        .get_mut(&paths)
        .unwrap()
        .sources
        .push(Source {
            id: "S1".into(),
            observed_at: chrono::Utc::now(),
            origin: "file".into(),
            path: Some("src/auth/token_store.rs".into()),
            start_line: Some(1),
            end_line: Some(2),
            line_start_complete: true,
            line_end_complete: true,
            evidence_truncated: false,
            hash: Some("observed-hash".into()),
            excerpt: String::new(),
        });
    for query in [
        "memory_read",
        "memory-read",
        "MEMORY READ",
        "ＭＥＭＯＲＹ＿ＲＥＡＤ",
        "http_server",
    ] {
        assert_eq!(s.memory.search(query, &[])[0].id, reader, "{query}");
    }
    assert_eq!(keys(&s, "io policy")[0], "io-policy");
    assert_eq!(
        s.memory.search(&"기억 검색".nfd().collect::<String>(), &[])[0].id,
        korean
    );
    assert_eq!(s.memory.search("src/auth/token_store.rs", &[])[0].id, paths);
}

#[test]
fn coverage_and_word_boundaries_beat_repetition_and_partial_noise() {
    let mut s = session();
    save(
        &mut s,
        "complete",
        "Routing",
        "Details",
        "authentication timeout",
    );
    save(
        &mut s,
        "partial",
        &"authentication ".repeat(50),
        "Details",
        "Details",
    );
    assert_eq!(keys(&s, "authentication timeout")[0], "complete");
    assert_eq!(
        keys(&s, "authentication authentication timeout"),
        keys(&s, "authentication timeout")
    );
    let exact = save(&mut s, "word", "Details", "Details", "token");
    save(&mut s, "substring", "tokenizer", "Details", "Details");
    assert_eq!(s.memory.search("token", &[])[0].id, exact);
    save(
        &mut s,
        "noise",
        "bright historical capital",
        "Details",
        "Details",
    );
    assert!(s.memory.search("hi", &[]).is_empty());
    assert!(s.memory.search("!!!", &[]).is_empty());
    assert!(s.memory.search("neverobserved", &[]).is_empty());
}

#[test]
fn matching_metadata_beats_body_only_and_korean_partial_search_still_works() {
    let mut s = session();
    save(&mut s, "header", "retry timeout", "Details", "Details");
    save(&mut s, "body", "Details", "Details", "retry timeout");
    assert_eq!(keys(&s, "retry timeout")[0], "header");
    save(&mut s, "hangul", "로그인실패 처리", "Details", "Details");
    assert_eq!(keys(&s, "로그인"), ["hangul"]);
}

#[test]
fn literal_key_match_wins_when_another_key_has_the_same_normalized_spelling() {
    let mut s = session();
    save(&mut s, "auth-rule", "Details", "Details", "Details");
    save(&mut s, "AUTH-RULE", "Details", "Details", "Details");
    assert_eq!(keys(&s, "auth-rule")[0], "auth-rule");
    assert_eq!(keys(&s, "AUTH-RULE")[0], "AUTH-RULE");
}

#[test]
fn whitespace_in_a_stored_key_does_not_change_exact_lookup_priority() {
    let mut s = session();
    save(&mut s, " auth ", "Details", "Details", "Details");
    save(&mut s, "auth", "Details", "Details", "Details");
    assert_eq!(keys(&s, " auth ")[0], " auth ");
    assert_eq!(keys(&s, " AUTH ")[0], " auth ");
    assert_eq!(keys(&s, "auth")[0], "auth");
}

#[test]
fn case_folded_whole_identifiers_keep_their_field_priority() {
    let mut s = session();
    save(&mut s, "header", "memoryRead", "Details", "Details");
    save(&mut s, "body", "Details", "Details", "memoryread");
    for query in ["memoryRead", "memoryread", "MEMORYREAD"] {
        assert_eq!(keys(&s, query)[0], "header", "{query}");
    }
}

#[test]
fn long_queries_do_not_round_a_real_match_down_to_no_match() {
    let mut s = session();
    save(&mut s, "relevant", "authentication", "Details", "Details");
    let query = format!(
        "authentication {}",
        (0..2000)
            .map(|n| format!("noise{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(keys(&s, &query), ["relevant"]);
}

#[test]
fn a_large_related_preview_can_borrow_unused_recent_space() {
    let mut s = session();
    let long = "절대로 잊지 말아야 할 조건과 예외 ".repeat(100);
    let important = save(&mut s, &long, &long, &long, "authentication");
    s.memory.entries.get_mut(&important).unwrap().tags = vec![long; 4];
    s.latest_request = "authentication".into();
    s.config.related_count = 1;
    s.config.recent_count = 0;
    s.config.index_tokens = 288;
    assert_eq!(
        ContextManager::state(&s).unwrap()["related_memories"][0]["id"],
        important
    );
    save(&mut s, "ui", "layout", "Details", "Details");
    s.config.recent_count = 1;
    s.config.validate().unwrap();
    assert_eq!(
        ContextManager::state(&s).unwrap()["related_memories"][0]["id"],
        important
    );
}

#[test]
fn status_affects_relevance_but_does_not_hide_explicit_id_key_or_list_results() {
    let mut s = session();
    let active = save(&mut s, "active-rule", "retry timeout", "Details", "Details");
    let review = save(&mut s, "review-rule", "retry timeout", "Details", "Details");
    s.memory.entries.get_mut(&review).unwrap().status = MemoryStatus::NeedsReview;
    let old = save(&mut s, "old-rule", "retry timeout", "Details", "Details");
    s.memory.entries.get_mut(&old).unwrap().status = MemoryStatus::Superseded;
    assert_eq!(
        keys(&s, "retry timeout"),
        ["active-rule", "review-rule", "old-rule"]
    );
    for id in [&active, &review, &old] {
        assert_eq!(s.memory.search(id, &[])[0].id, *id);
    }
    assert_eq!(keys(&s, "OLD-RULE")[0], "old-rule");
    assert_eq!(keys(&s, ""), ["old-rule", "review-rule", "active-rule"]);
    assert_eq!(keys(&s, "   "), keys(&s, ""));
    s.memory.entries.get_mut(&active).unwrap().tags = vec!["Auth".into(), "retry".into()];
    assert_eq!(
        s.memory
            .search("timeout", &["Auth".into(), "retry".into()])
            .len(),
        1
    );
    assert!(s.memory.search("timeout", &["auth".into()]).is_empty());
    let page = s.memory.page("timeout", &[], None, 1).unwrap();
    let cursor = page["next_cursor"].as_str().unwrap();
    assert_eq!(
        s.memory.page("timeout", &[], Some(cursor), 1).unwrap()["items"][0]["id"],
        review
    );
    assert!(s.memory.page("retry", &[], Some(cursor), 1).is_err());
    save(&mut s, "new", "Details", "Details", "Details");
    assert!(s.memory.page("timeout", &[], Some(cursor), 1).is_err());
}

#[test]
fn old_relevant_memory_survives_many_new_irrelevant_memories_under_pressure() {
    let mut s = session();
    let important = save(
        &mut s,
        "auth-rule",
        "authentication timeout",
        "A durable decision",
        "Details",
    );
    for n in 0..30 {
        save(
            &mut s,
            &format!("layout-{n}"),
            "CSS layout",
            &"Visual details ".repeat(100),
            "Details",
        );
    }
    s.latest_request = "authentication timeout".into();
    s.config.index_tokens = 500;
    let state = ContextManager::state(&s).unwrap();
    assert_eq!(state["related_memories"][0]["id"], important);
    assert!(!state["recent_memories"].as_array().unwrap().is_empty());
    assert!(context::count(&index(&state), &s.config.model) <= s.config.index_tokens);
    assert!(state["memory_index_notice"]["omitted"].as_u64().unwrap() > 0);
}

#[test]
fn pins_and_recent_matches_are_deduplicated_without_consuming_related_slots() {
    let mut s = session();
    let old = save(&mut s, "old", "authentication", "Details", "Details");
    let recent = save(&mut s, "recent", "authentication", "Details", "Details");
    let pinned = save(&mut s, "pin", "authentication", "Details", "Details");
    s.task.memory_ids = vec![pinned.clone(), "pin".into()];
    s.latest_request = "authentication".into();
    s.config.related_count = 2;
    let state = ContextManager::state(&s).unwrap();
    assert_eq!(state["referenced_memories"].as_array().unwrap().len(), 1);
    assert_eq!(state["referenced_memories"][0]["id"], pinned);
    assert_eq!(state["related_memories"][0]["id"], recent);
    assert_eq!(state["related_memories"][1]["id"], old);
    assert_eq!(ids(&state).len(), 3);
    assert_eq!(ids(&state).into_iter().collect::<BTreeSet<_>>().len(), 3);
}

#[test]
fn small_request_keeps_priority_over_a_long_repetitive_current_todo() {
    let mut s = session();
    let requested = save(
        &mut s,
        "auth",
        "authentication timeout",
        "Details",
        "Details",
    );
    save(&mut s, "ui", "layout colors spacing", "Details", "Details");
    s.latest_request = "authentication timeout".into();
    s.task.todos.push(TodoItem {
        id: "T1".into(),
        text: "layout colors spacing ".repeat(30),
        done: false,
        result: String::new(),
        reopen_reason: String::new(),
    });
    s.config.related_count = 1;
    let state = ContextManager::state(&s).unwrap();
    assert_eq!(state["related_memories"][0]["id"], requested);
    s.latest_request.clear();
    assert_eq!(
        ContextManager::state(&s).unwrap()["related_memories"][0]["key"],
        "ui"
    );
    s.latest_request = "authentication timeout".into();
    s.task.todos[0].text = "ui".into();
    assert_eq!(
        ContextManager::state(&s).unwrap()["related_memories"][0]["id"],
        requested
    );
}

#[test]
fn pinned_memories_win_under_pressure_and_every_index_obeys_its_combined_budget() {
    for model in ["gpt-4o", "unknown-model"] {
        let mut s = session();
        s.config.model = model.into();
        let pinned = save(
            &mut s,
            "constraint",
            "Preserved choice",
            "Details",
            "Details",
        );
        s.task.memory_ids.push(pinned.clone());
        for n in 0..12 {
            save(
                &mut s,
                &format!("rule-{n}"),
                if n % 2 == 0 {
                    "authentication"
                } else {
                    "layout"
                },
                &"Details with quotes \" and \\ and 한국어. ".repeat(n + 1),
                "Details",
            );
        }
        s.latest_request = "authentication".into();
        for budget in [80, 160, 250, 400, 700] {
            s.config.index_tokens = budget;
            let state = ContextManager::state(&s).unwrap();
            assert!(
                context::count(&index(&state), model) <= budget,
                "{model}, {budget}"
            );
            let ids = ids(&state);
            let unique: BTreeSet<_> = ids.iter().collect();
            assert_eq!(ids.len(), unique.len());
            if budget >= 160 {
                assert_eq!(state["referenced_memories"][0]["id"], pinned);
            }
            if let Some(omitted) = state["memory_index_notice"]["omitted_ids"].as_array() {
                assert!(
                    omitted
                        .iter()
                        .all(|id| !ids.iter().any(|included| id == included))
                );
            }
        }
    }
}

#[test]
fn automatic_recall_keeps_review_labels_and_excludes_superseded_unless_pinned() {
    let mut s = session();
    let review = save(&mut s, "review", "authentication", "Details", "Details");
    s.memory.entries.get_mut(&review).unwrap().status = MemoryStatus::NeedsReview;
    let old = save(&mut s, "old", "authentication", "Details", "Details");
    s.memory.entries.get_mut(&old).unwrap().status = MemoryStatus::Superseded;
    s.latest_request = "authentication".into();
    let state = ContextManager::state(&s).unwrap();
    assert_eq!(ids(&state), [review]);
    assert_eq!(state["related_memories"][0]["status"], "needs_review");
    s.task.memory_ids.push(old.clone());
    assert_eq!(
        ContextManager::state(&s).unwrap()["referenced_memories"][0]["id"],
        old
    );
}

#[test]
fn large_metadata_is_only_shortened_in_the_model_preview() {
    for model in ["gpt-4o", "unknown-model"] {
        let mut s = session();
        s.config.model = model.into();
        let long = "조건과 예외를 반드시 확인해야 합니다. ".repeat(500);
        let memory = save(&mut s, &long, &long, &long, "Entire body");
        s.memory.entries.get_mut(&memory).unwrap().tags = vec![long.clone(); 10];
        let before = json!(s.memory.entries);
        s.task.memory_ids.push(memory.clone());
        s.config.index_tokens = 500;
        let state = ContextManager::state(&s).unwrap();
        let preview = &state["referenced_memories"][0];
        assert_eq!(preview["id"], memory);
        assert_eq!(preview["preview_truncated"], true);
        assert_eq!(preview["revision"], 1);
        assert_eq!(preview["status"], "active");
        assert!(context::count(&index(&state), model) <= s.config.index_tokens);
        assert_eq!(json!(s.memory.entries), before);
        let read = tools::execute(&mut s, "memory_read", json!({"id":memory})).unwrap();
        assert_eq!(read["metadata"]["summary"], long);
        assert_eq!(read["metadata"]["tags"].as_array().unwrap().len(), 10);
    }
}

#[test]
fn unused_bucket_space_is_borrowed_and_zero_counts_disable_buckets() {
    let mut s = session();
    for n in 0..8 {
        save(
            &mut s,
            &format!("rule-{n}"),
            "authentication",
            "Details",
            "Details",
        );
    }
    s.config.index_tokens = 350;
    s.config.related_count = 8;
    s.config.recent_count = 0;
    s.latest_request = "authentication".into();
    let related = ContextManager::state(&s).unwrap();
    assert!(context::count(&index(&related), &s.config.model) > s.config.index_tokens * 7 / 10);
    assert!(context::count(&index(&related), &s.config.model) <= s.config.index_tokens);
    assert!(related["recent_memories"].as_array().unwrap().is_empty());
    s.config.related_count = 0;
    s.config.recent_count = 8;
    let recent = ContextManager::state(&s).unwrap();
    assert_eq!(ids(&related), ids(&recent));
    assert!(recent["related_memories"].as_array().unwrap().is_empty());
    s.config.memory_reuse = false;
    s.task.memory_ids.push("missing-but-disabled".into());
    assert!(ids(&ContextManager::state(&s).unwrap()).is_empty());
}

#[test]
fn tiny_budget_omits_safely_and_reports_recovery_ids() {
    let mut s = session();
    let memory = save(&mut s, "pin", "authentication", "Details", "Details");
    s.task.memory_ids.push(memory.clone());
    s.latest_request = "authentication".into();
    s.config.index_tokens = 1;
    let state = ContextManager::state(&s).unwrap();
    assert!(ids(&state).is_empty());
    assert_eq!(state["memory_index_notice"]["omitted_ids"], json!([memory]));
    assert_eq!(state["memory_index_notice"]["omitted"], 1);
}
