mod support;
use mnemoarc::{
    config::Project,
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
        support::compact_config(),
    );
    session.active_tools = ToolRegistry::optional_names();
    (dir, session)
}
fn search(s: &mut Session, args: Value) -> Value {
    tools::execute(s, "source_search", args).unwrap()
}

#[test]
fn empty_search_query_is_missing_not_conflicting() {
    let (_dir, mut s) = setup();
    for args in [json!({}), json!({"query":"","regex":true})] {
        let error = tools::execute(&mut s, "source_search", args)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "missing_argument: query or queries; send query as one nonempty search string"
            ),
            "{error}"
        );
    }
    let error = tools::execute(
        &mut s,
        "source_search",
        json!({"query":"one","queries":["two"]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("conflicting_arguments:"), "{error}");
}

#[test]
fn exact_directory_scope_treats_glob_characters_as_literal_path_names() {
    let (dir, mut s) = setup();
    for folder in ["[route]", "r", "{routes,handlers}", "routes", "[unclosed"] {
        std::fs::create_dir_all(dir.path().join(folder).join("nested")).unwrap();
        std::fs::write(dir.path().join(folder).join("a.rs"), "fn marker() {}\n").unwrap();
        std::fs::write(
            dir.path().join(folder).join("nested/b.rs"),
            "fn marker() {}\n",
        )
        .unwrap();
    }
    for folder in ["[route]", "{routes,handlers}", "[unclosed"] {
        let first = search(&mut s, json!({"path":folder,"query":"marker","limit":1}));
        assert_eq!(first["matched_files"], 2, "{folder}: {first}");
        assert_eq!(first["total_matching_lines"], 2);
        let second = search(
            &mut s,
            json!({"path":folder,"query":"marker","limit":1,"cursor":first["next_cursor"]}),
        );
        for page in [&first, &second] {
            let found = std::path::Path::new(page["matches"][0]["path"].as_str().unwrap());
            assert!(found.starts_with(dir.path().join(folder).canonicalize().unwrap()));
        }
        assert!(second["next_cursor"].is_null());
        let changed_scope = tools::execute(
            &mut s,
            "source_search",
            json!({"path":"r","query":"marker","limit":1,"cursor":first["next_cursor"]}),
        );
        assert!(changed_scope.is_err());
    }
}

#[test]
fn directory_search_applies_include_patterns_to_files_inside_the_scope() {
    let (dir, mut s) = setup();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "marker\n").unwrap();
    std::fs::write(dir.path().join("src/notes.txt"), "marker\n").unwrap();
    std::fs::write(dir.path().join("src/private.rs"), "marker\n").unwrap();
    s.project.include = vec!["**/*.rs".into()];
    s.project.exclude = vec!["**/private.rs".into()];
    for path in ["src", "."] {
        let result = search(&mut s, json!({"path":path,"query":"marker"}));
        assert_eq!(result["matched_files"], 1);
        assert!(
            result["matches"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("src/main.rs")
        );
    }
    for path in ["src/notes.txt", "src/private.rs"] {
        assert!(
            tools::execute(
                &mut s,
                "source_search",
                json!({"path":path,"query":"marker"})
            )
            .is_err()
        );
    }
}

#[test]
fn casefolded_exclusions_apply_to_direct_reads_and_searches() {
    let (dir, mut s) = setup();
    std::fs::create_dir_all(dir.path().join("Secrets")).unwrap();
    std::fs::write(
        dir.path().join("Secrets/credentials.txt"),
        "private-token-value\n",
    )
    .unwrap();
    s.project.exclude = vec!["secrets/**".into()];

    let read = tools::execute(
        &mut s,
        "file_read",
        json!({"path":"Secrets/credentials.txt"}),
    )
    .unwrap_err();
    assert!(read.to_string().contains("path_excluded"));
    let result = search(&mut s, json!({"query":"private-token-value"}));
    assert_eq!(result["total_matching_lines"], 0);

    std::fs::create_dir_all(dir.path().join("Target")).unwrap();
    std::fs::write(dir.path().join("Target/build.rs"), "private build output\n").unwrap();
    s.project.exclude.clear();
    let read = tools::execute(&mut s, "file_read", json!({"path":"Target/build.rs"})).unwrap_err();
    assert!(read.to_string().contains("path_excluded"));
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

#[test]
fn empty_search_distinguishes_missing_scope_from_literal_miss() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("server.js"),
        "res.once('close', onClose);\n",
    )
    .unwrap();
    for mode in ["matches", "files", "count"] {
        let missing = search(
            &mut s,
            json!({"query":"close","path_glob":"missing.js","mode":mode}),
        );
        assert_eq!(missing["empty_reason"], "no_searchable_files");
        assert_eq!(missing["matched_files"], 0);
        assert!(missing["guidance"].as_str().unwrap().contains("file_list"));
        let miss = search(
            &mut s,
            json!({"query":"req.on(\"close\"","path_glob":"server.js","mode":mode}),
        );
        assert_eq!(miss["empty_reason"], "no_matching_lines");
        assert_eq!(miss["matched_files"], 1);
        assert_eq!(miss["total_matching_lines"], 0);
        assert!(miss["next_cursor"].is_null());
        assert!(s.sources.is_empty());
    }
    let found = search(&mut s, json!({"query":"close","path_glob":"server.js"}));
    assert_eq!(found["matches"][0]["line"], 1);
    assert!(found.get("empty_reason").is_none());
    let alternatives = search(
        &mut s,
        json!({"query":"close|abort","path_glob":"server.js"}),
    );
    assert_eq!(alternatives["total_matching_lines"], 0);
    assert!(
        alternatives["guidance"]
            .as_str()
            .unwrap()
            .contains("regex:true")
    );
    let explicit_regex = search(
        &mut s,
        json!({"query":"close|abort","path_glob":"server.js","regex":true}),
    );
    assert_eq!(explicit_regex["total_matching_lines"], 1);
}

#[test]
fn exact_path_search_is_literal_scoped_and_cursor_bound() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a[1].rs"), "hit\nhit\n").unwrap();
    std::fs::write(dir.path().join("a1.rs"), "hit\n").unwrap();
    let first = search(&mut s, json!({"path":"a[1].rs","query":"hit","limit":1}));
    assert_eq!(first["matched_files"], 1);
    assert_eq!(first["total_matching_lines"], 2);
    assert!(
        first["matches"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("a[1].rs")
    );
    let next = search(
        &mut s,
        json!({"path":"a[1].rs","query":"hit","limit":1,"cursor":first["next_cursor"]}),
    );
    assert_eq!(next["matches"][0]["line"], 2);
    for args in [
        json!({"path":"a1.rs","query":"hit","cursor":first["next_cursor"]}),
        json!({"path":"a[1].rs","path_glob":"*.rs","query":"hit"}),
        json!({"path":"a[1].rs","pattern":"*.rs","query":"hit"}),
        json!({"path":"../outside.rs","query":"hit"}),
    ] {
        assert!(tools::execute(&mut s, "source_search", args).is_err());
    }
    // Existing project exclusions remain enforced on an exact file read.
    s.project.exclude.push("a*".into());
    assert!(
        tools::execute(
            &mut s,
            "source_search",
            json!({"path":"a[1].rs","query":"hit"})
        )
        .is_err()
    );
}

#[test]
fn literal_alternatives_preserve_punctuation_and_cursor_identity() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("a.js"),
        "res.on('close')\nreq.once('close')\nclose|req.on\nelse\n",
    )
    .unwrap();
    let args = json!({"path":"a.js","queries":["res.on(","req.once("],"limit":1});
    let first = search(&mut s, args.clone());
    assert_eq!(first["total_matching_lines"], 2);
    assert_eq!(first["matches"][0]["line"], 1);
    let mut next = args;
    next["cursor"] = first["next_cursor"].clone();
    assert_eq!(search(&mut s, next.clone())["matches"][0]["line"], 2);
    next["queries"] = json!(["else"]);
    assert!(tools::execute(&mut s, "source_search", next).is_err());
    for invalid in [
        json!({}),
        json!({"query":"a","queries":["b"]}),
        json!({"queries":[]}),
        json!({"queries":[""]}),
        json!({"queries":["a"],"regex":true}),
    ] {
        assert!(tools::execute(&mut s, "source_search", invalid).is_err());
    }
    let error = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"a.js","query":".on(","regex":true}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("invalid_search_regex") && error.contains("regex:false"));
    assert_eq!(
        search(&mut s, json!({"path":"a.js","query":".on("}))["total_matching_lines"],
        1
    );
}

#[test]
fn file_list_path_lists_one_directory() {
    let (dir, mut s) = setup();
    std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
    std::fs::write(dir.path().join("frontend/src/App.jsx"), "x\n").unwrap();
    std::fs::write(dir.path().join("frontend/other.js"), "x\n").unwrap();
    // The live shape: a directory path instead of path_glob.
    let listed = tools::execute(
        &mut s,
        "file_list",
        json!({"mode":"paths","path":"frontend/src"}),
    )
    .unwrap();
    assert_eq!(listed["paths"], json!(["frontend/src/App.jsx"]));
    for args in [
        json!({"path":"frontend/src","path_glob":"**"}),
        json!({"path":"frontend/src/App.jsx"}),
    ] {
        assert!(tools::execute(&mut s, "file_list", args).is_err());
    }
}

#[test]
fn file_list_directory_path_is_literal_and_cursor_keeps_its_scope() {
    let (dir, mut s) = setup();
    for folder in ["[route]", "r", "{routes,handlers}", "routes", "[unclosed"] {
        std::fs::create_dir_all(dir.path().join(folder).join("nested")).unwrap();
        std::fs::write(dir.path().join(folder).join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join(folder).join("nested/b.rs"), "fn b() {}\n").unwrap();
    }
    for folder in ["[route]", "{routes,handlers}", "[unclosed"] {
        let first = tools::execute(
            &mut s,
            "file_list",
            json!({"mode":"paths","path":folder,"limit":1}),
        )
        .unwrap();
        assert_eq!(first["total_files"], 2, "{folder}: {first}");
        let second = tools::execute(
            &mut s,
            "file_list",
            json!({"cursor":first["next_cursor"],"limit":1}),
        )
        .unwrap();
        for page in [&first, &second] {
            assert!(
                page["paths"][0].as_str().unwrap().starts_with(folder),
                "{folder}: {page}"
            );
        }
        assert!(second["next_cursor"].is_null());
        let text_mode =
            tools::execute(&mut s, "file_list", json!({"mode":"text","path":folder})).unwrap();
        assert_eq!(text_mode["total_files"], 2, "{folder}: {text_mode}");
        assert!(
            tools::execute(
                &mut s,
                "file_list",
                json!({"mode":"paths","path":"r","cursor":first["next_cursor"]}),
            )
            .is_err()
        );
    }
}

#[test]
fn an_empty_filled_alternative_to_query_is_not_a_conflict() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.js"), "const hint = 'Shift+Enter';\n").unwrap();
    let found = search(
        &mut s,
        json!({"path":"a.js","query":"Shift+Enter","queries":[]}),
    );
    assert_eq!(
        found["matches"].as_array().map(Vec::len),
        Some(1),
        "{found}"
    );
    let found = search(&mut s, json!({"path":"a.js","query":"","queries":["hint"]}));
    assert_eq!(
        found["matches"].as_array().map(Vec::len),
        Some(1),
        "{found}"
    );
    let err = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"a.js","query":"hint","queries":["const"]}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.starts_with("conflicting_arguments:"), "{err}");
}

#[test]
fn search_text_sent_as_pattern_is_named_as_a_file_glob() {
    let (dir, mut s) = setup();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    // A live run sent its search text as pattern three times, then stopped
    // using source_search: the error never said what pattern means.
    let error = tools::execute(
        &mut s,
        "source_search",
        json!({"path":"src","pattern":"fn main","mode":"matches"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("missing_argument: query or queries; pattern \"fn main\" is a file glob"),
        "{error}"
    );
    assert!(error.contains("send the text as query"), "{error}");
    assert!(
        error.contains("drop pattern, since path already sets the scope"),
        "{error}"
    );
    let error = tools::execute(&mut s, "source_search", json!({"pattern":"fn main"}))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("drop pattern"), "{error}");
    // Following the hint succeeds.
    let found = search(&mut s, json!({"path":"src","query":"fn main"}));
    assert_eq!(
        found["matches"].as_array().map(Vec::len),
        Some(1),
        "{found}"
    );
}
