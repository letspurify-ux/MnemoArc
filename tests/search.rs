use mnemoarc::{
    config::{Config, Project},
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};

fn setup() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        Config::default(),
    );
    session.active_tools = ToolRegistry::optional_names();
    (dir, session)
}
fn search(s: &mut Session, args: Value) -> Value {
    tools::execute(s, "source_search", args).unwrap()
}

#[test]
fn literal_regex_case_word_and_unicode_context() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("main.rs"),
        "앞줄\r\nLoad load_loader load\r\n뒷줄\r\nload()\r\n한글 한글자\r\n",
    )
    .unwrap();
    let r = search(
        &mut s,
        json!({"query":"load","case_sensitive":false,"whole_word":true,"before":1,"after":1}),
    );
    assert_eq!(r["total_matching_lines"], 2);
    assert_eq!(
        r["matches"][0]["context"],
        json!([
            {"line":1,"text":"앞줄","truncated":false},
            {"line":3,"text":"뒷줄","truncated":false}
        ])
    );
    assert_eq!(r["matches"][0]["source"]["start_line"], 2);
    assert_eq!(r["matches"][0]["source"]["end_line"], 2);
    assert_eq!(
        search(&mut s, json!({"query":"LOAD"}))["total_matching_lines"],
        0
    );
    assert_eq!(
        search(&mut s, json!({"query":"load()"}))["total_matching_lines"],
        1
    );
    assert_eq!(
        search(&mut s, json!({"query":"^load\\(\\)$","regex":true}))["matches"][0]["line"],
        4
    );
    assert_eq!(
        search(&mut s, json!({"query":"한글","whole_word":true}))["matches"][0]["line"],
        5
    );
    assert_eq!(
        search(&mut s, json!({"query":"loader","whole_word":true}))["total_matching_lines"],
        0
    );
}

#[test]
fn compact_modes_paginate_files_and_count_lines_without_sources() {
    let (dir, mut s) = setup();
    for name in ["a.rs", "b.rs"] {
        std::fs::write(dir.path().join(name), "hit hit\nhit\n").unwrap();
    }
    std::fs::write(dir.path().join("other.rs"), "nothing\n").unwrap();
    let r = search(&mut s, json!({"query":"hit","mode":"files","limit":1}));
    assert_eq!(r["files"].as_array().unwrap().len(), 1);
    assert_eq!(r["matched_files"], 3);
    assert_eq!(r["matching_files"], 2);
    assert_eq!(r["total_matching_lines"], 4);
    let next = search(
        &mut s,
        json!({"query":"hit","mode":"files","limit":1,"cursor":r["next_cursor"]}),
    );
    assert_ne!(r["files"][0], next["files"][0]);
    assert!(next["next_cursor"].is_null());
    let r = search(&mut s, json!({"query":"hit","mode":"count","limit":1}));
    assert_eq!(r["counts"][0]["matching_lines"], 2);
    let next = search(
        &mut s,
        json!({"query":"hit","mode":"count","cursor":r["next_cursor"]}),
    );
    assert_ne!(r["counts"][0]["path"], next["counts"][0]["path"]);
    assert!(s.sources.is_empty());
}

#[test]
fn pages_bind_options_and_source_revisions_and_only_observe_delivered_rows() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "hit\nhit\nhit\n").unwrap();
    let first = search(&mut s, json!({"query":"hit","limit":1}));
    assert_eq!(s.sources.len(), 1);
    let next = search(
        &mut s,
        json!({"query":"hit","limit":2,"cursor":first["next_cursor"]}),
    );
    assert_eq!(next["matches"][0]["line"], 2);
    assert_eq!(next["matches"][1]["line"], 3);
    assert!(next["next_cursor"].is_null());
    for patch in [
        json!({"mode":"files"}),
        json!({"case_sensitive":false}),
        json!({"before":1}),
        json!({"path_glob":"*.rs"}),
    ] {
        let mut args = json!({"query":"hit","cursor":first["next_cursor"]});
        args.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(
            tools::execute(&mut s, "source_search", args)
                .unwrap_err()
                .to_string()
                .contains("cursor_expired")
        );
    }
    std::fs::write(dir.path().join("a.rs"), "hit\nnew\nhit\n").unwrap();
    assert!(
        tools::execute(
            &mut s,
            "source_search",
            json!({"query":"hit","cursor":first["next_cursor"]})
        )
        .unwrap_err()
        .to_string()
        .contains("cursor_expired")
    );
}

#[test]
fn fast_paths_preserve_exclusions_and_text_modes_skip_unsupported_files() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "fn hit() {}\n").unwrap();
    std::fs::write(dir.path().join("binary"), b"hit\0").unwrap();
    std::fs::write(dir.path().join("invalid"), [255]).unwrap();
    std::fs::create_dir(dir.path().join("node_modules")).unwrap();
    std::fs::write(dir.path().join("node_modules/hidden.rs"), "hit").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", dir.path().join("escape")).unwrap();
    let regular = tools::execute(&mut s, "file_list", json!({})).unwrap();
    assert_eq!(regular["paths"], json!(["a.rs"]));
    let fast = tools::execute(&mut s, "file_list", json!({"mode":"paths"})).unwrap();
    assert_eq!(fast["paths"], json!(["a.rs", "binary", "invalid"]));
    assert_eq!(search(&mut s, json!({"query":"hit"}))["matched_files"], 1);
    assert_eq!(
        tools::execute(&mut s, "symbol_search", json!({"query":"hit"})).unwrap()["symbols"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    s.project.exclude.push("*.rs".into());
    assert_eq!(
        search(&mut s, json!({"query":"hit"}))["total_matching_lines"],
        0
    );
}

#[test]
fn bounded_context_marks_truncation_and_invalid_options_fail() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("a.rs"),
        format!("{}\nhit\n{}", "가".repeat(501), "나".repeat(501)),
    )
    .unwrap();
    let r = search(&mut s, json!({"query":"hit","before":20,"after":20}));
    assert_eq!(r["matches"][0]["context"].as_array().unwrap().len(), 2);
    assert_eq!(
        r["matches"][0]["context"][0]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        500
    );
    assert_eq!(r["matches"][0]["context"][1]["truncated"], true);
    for args in [
        json!({"query":"hit","before":21}),
        json!({"query":"hit","after":u64::MAX}),
        json!({"query":"hit","mode":"files","after":1}),
        json!({"query":"[","regex":true}),
    ] {
        assert!(tools::execute(&mut s, "source_search", args).is_err());
    }
}

#[test]
fn broad_search_retains_only_the_requested_page() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "hit\n".repeat(100_001)).unwrap();
    let first = search(
        &mut s,
        json!({"query":"hit","limit":1,"before":1,"after":1}),
    );
    assert_eq!(first["total_matching_lines"], 100_001);
    assert_eq!(first["matches"].as_array().unwrap().len(), 1);
    assert_eq!(s.sources.len(), 1);
    let next = search(
        &mut s,
        json!({"query":"hit","limit":1,"before":1,"after":1,"cursor":first["next_cursor"]}),
    );
    assert_eq!(next["matches"][0]["line"], 2);
    assert_eq!(next["matches"][0]["context"][0]["line"], 1);
}
