//! Regressions from the 2026-10-23 performance review of the document
//! tools: each test reproduces a document or source size at which a save,
//! an audit or a history search took seconds before it was fixed.

use crate::support;
use mnemoarc::{
    config::Project,
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::json;
use std::time::{Duration, Instant};

/// Generous for unoptimized builds; each case took far longer before.
const LIMIT: Duration = Duration::from_secs(20);

fn document(doc: &str) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("summary.md"),
            ..Default::default()
        },
        support::compact_config(),
    );
    s.active_tools = ToolRegistry::optional_names();
    std::fs::write(dir.path().join("summary.md"), doc).unwrap();
    s.task.workflow = "source_document".into();
    s.workflow_mode = "source_document".into();
    (dir, s)
}

fn write(dir: &tempfile::TempDir, path: &str, text: &str) {
    let full = dir.path().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// Each folder-less citation's ready fix scanned and rewrote the whole
/// document, and identical lines were widened 40 times before giving up:
/// 6000 such citations in a 1.3MB document took 40 seconds per audit.
/// Fixes are built for one batch of lines; every issue keeps its suggested
/// path.
#[test]
fn audit_builds_ready_fixes_for_one_batch_of_lines() {
    let mut doc = String::from("# Doc\n\n");
    let filler = "Background that explains the helper in more words. ".repeat(3);
    for i in 0..6000 {
        doc.push_str(&format!(
            "## S{i}\n\n{filler}\n\nThe helper is in a{}.rs:1.\n\n",
            i % 10
        ));
    }
    let (dir, mut s) = document(&doc);
    for i in 0..10 {
        write(&dir, &format!("src/a{i}.rs"), "fn helper() {}\n");
    }
    let started = Instant::now();
    let audit = tools::execute(&mut s, "document_audit", json!({"limit":100})).unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(audit["issue_kinds"]["citation_path"], 6000, "{audit}");
    let issues = audit["issues"].as_array().unwrap();
    assert!(
        issues.iter().all(|issue| issue["suggested"]
            .as_str()
            .is_some_and(|s| s.starts_with("src/a"))),
        "{audit}"
    );
}

/// A name a claim mentions was looked up in every line above each cited
/// range, and a pattern was compiled per claim: 2000 claims citing a
/// 20000-line file took 5 seconds per save.
#[test]
fn citation_mismatch_check_scales_with_claims_and_file_size() {
    let source: String = (0..20_000)
        .map(|j| {
            if j % 20 == 0 {
                format!("fn func_{}() {{\n", j / 20)
            } else {
                format!("    let v{j} = call_{}(x);\n", j % 97)
            }
        })
        .collect();
    let mut doc = String::from("# Doc\n\n");
    for i in 0..2000 {
        let line = 19_999 - i % 50;
        doc.push_str(&format!(
            "## S{i}\n\nThe `missing_name_{}` helper runs here (src/big.rs:{line}-{line}).\n\n",
            i % 300
        ));
    }
    // The last claim names the function that encloses its lines.
    doc.push_str("## Last\n\nThe loop in `func_999` (src/big.rs:19990-19991).\n");
    let (dir, mut s) = document("# Doc\n");
    write(&dir, "src/big.rs", &source);
    std::fs::write(dir.path().join("summary.md"), &doc).unwrap();
    let hash = tools::hash(doc.as_bytes());
    let started = Instant::now();
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","text":"\nEnd.\n","expected_hash":hash}),
    )
    .unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(saved["citation_mismatch_check"]["flagged"], 2000, "{saved}");
}

/// Every inserted section's path was found by parsing the whole document
/// again, twice: 200 sections into a 4000-heading document took seconds.
#[test]
fn inserting_many_sections_parses_the_document_once() {
    let mut doc = String::from("# Doc\n\n");
    for i in 0..2000 {
        doc.push_str(&format!(
            "## {i}. Part\n\nText {i}.\n\n### {i}.1 Child\n\nMore.\n\n"
        ));
    }
    let (_dir, mut s) = document(&doc);
    let text: String = (0..200)
        .map(|j| format!("### 1000.x{j} New\n\nNew text {j}.\n\n"))
        .collect();
    let started = Instant::now();
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"## 1000. Part","text":text,"expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(saved["heading_count"], 4201, "{saved}");
    // A duplicate among them is still refused.
    let doc = std::fs::read_to_string(_dir.path().join("summary.md")).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"## 1000. Part","text":"### 1000.x7 New\n\nAgain.\n","expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("already exists under the same parent"),
        "{error}"
    );
}

/// A history search serialized and searched every bundle even after its
/// page was full.
#[test]
fn history_search_pages_stay_complete() {
    let (_dir, mut s) = document("# Doc\n");
    for i in 0..30 {
        s.history.push(
            vec![json!({"role":"user","content":format!("note {i} about topic")})],
            true,
        );
    }
    let page = tools::execute(
        &mut s,
        "history",
        json!({"action":"search","query":"topic","limit":10}),
    )
    .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 10, "{page}");
    let next = page["next_cursor"].as_u64().unwrap();
    let rest = tools::execute(
        &mut s,
        "history",
        json!({"action":"search","query":"topic","after":next,"limit":50}),
    )
    .unwrap();
    assert_eq!(rest["items"].as_array().unwrap().len(), 20, "{rest}");
    assert!(rest["next_cursor"].is_null(), "{rest}");
}

/// A path given as a memory source ID is replaced by the sources delivered
/// from that file; each was checked against every source found before it,
/// so a file with 60,000 delivered sources took seconds per memory_write.
#[test]
fn path_source_ids_resolve_linearly_in_delivered_sources() {
    let (dir, mut s) = document("# Doc\n");
    let lines = 60_000;
    let text: String = (0..lines).map(|i| format!("needle_{i}\n")).collect();
    write(&dir, "src/big.rs", &text);
    let digest = tools::hash(text.as_bytes());
    let path = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("src/big.rs")
        .display()
        .to_string();
    for i in 0..lines {
        let source = mnemoarc::memory::Source {
            id: format!("SB{i:06}"),
            observed_at: Default::default(),
            origin: "file".into(),
            path: Some(path.clone()),
            start_line: Some(i + 1),
            end_line: Some(i + 1),
            line_start_complete: true,
            line_end_complete: true,
            evidence_truncated: false,
            hash: Some(digest.clone()),
            excerpt: format!("needle_{i}"),
        };
        s.sources
            .insert(source.id.clone(), std::sync::Arc::new(source));
    }
    let started = Instant::now();
    let saved = tools::execute(
        &mut s,
        "memory_write",
        json!({"title":"Needles","summary":"s","body":"b","kind":"fact","source_ids":["src/big.rs"]}),
    )
    .unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    // At most eight of the file's delivered sources, the most recent first;
    // observed at the same time, they keep their order in the file.
    assert_eq!(
        saved["resolved_source_ids"]["src/big.rs"],
        json!((0..8).map(|i| format!("SB{i:06}")).collect::<Vec<_>>()),
        "{saved}"
    );
}

/// An unaccepted argument name is repeated by its error. A leaked call can
/// carry a whole file in one key: a 4 MiB key was repeated in the message
/// and its data, and bounding that result took seconds.
#[test]
fn errors_repeat_only_the_start_of_a_long_argument_name() {
    let (_dir, mut s) = document("# Doc\n");
    let key = "k".repeat(4 << 20);
    let call = mnemoarc::llm::ToolCall {
        id: "c1".into(),
        name: "source_search".into(),
        arguments: json!({"query":"a", key: 1}).to_string(),
    };
    let started = Instant::now();
    let result = tools::run_call(&mut s, &call);
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    let error = result["error"].as_str().unwrap();
    assert!(error.starts_with("unknown_argument: kkk"), "{error}");
    assert!(error.len() < 2000, "{}", error.len());
    assert!(
        result.to_string().len() < 8000,
        "{}",
        result.to_string().len()
    );
    // A nested key is cut the same way, with the field it lacks.
    let batch = tools::execute(
        &mut s,
        "file_patch",
        json!({"operations":[{"path":"a.rs", "x".repeat(1 << 20): "y"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(batch.len() < 2000, "{}", batch.len());
}

fn call(s: &mut Session, id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
    let call = mnemoarc::llm::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    };
    tools::run_call(s, &call)
}

/// Errors quote the value they refuse: a memory ID, a section, a path or a
/// tool name. A 1 MiB value was repeated whole and bounding each result
/// took 0.15 s per MiB; a refused plan batch and an unknown task_state patch
/// field repeated it in their details as well. The start and the end of the
/// message are kept.
#[test]
fn refusals_repeat_only_the_ends_of_a_long_value() {
    let doc = "# Doc\n\n## A\n\nText.\n";
    let (_dir, mut s) = document(doc);
    s.document_written = true;
    let digest = tools::hash(doc.as_bytes());
    let big = "a".repeat(1 << 20);
    let cases = [
        ("memory_read", json!({"id":big})),
        ("memory_manage", json!({"action":"delete","ids":[big]})),
        ("tool_select", json!({"action":"add","names":[big]})),
        ("file_read", json!({"path":big})),
        (
            "document_edit",
            json!({"action":"section","section":big,"text":"x","expected_hash":digest,"expected_section_hash":digest}),
        ),
        (
            "task_state",
            json!({"action":"update","patch":{big.clone():1}}),
        ),
        (
            "task_plan",
            json!({"action":"apply","expected_revision":0,"operations":[{"op":"update","id":big,"text":"x"}]}),
        ),
    ];
    for (index, (name, args)) in cases.into_iter().enumerate() {
        let started = Instant::now();
        let result = call(&mut s, &format!("c{index}"), name, args);
        assert!(started.elapsed() < LIMIT, "{name}: {:?}", started.elapsed());
        let shown = result.to_string();
        assert!(shown.len() < 24_000, "{name}: {}", shown.len());
        if let Some(error) = result["error"].as_str() {
            assert!(error.contains("characters omitted"), "{name}: {error:.200}");
        }
    }
    let error = call(&mut s, "m", "memory_read", json!({"id":big}))["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(error.starts_with("memory_not_found: "), "{error:.200}");
    assert!(error.ends_with("never guess one"), "{error:.200}");
}

/// tool_catalog lowercased every description again for each query term:
/// a query of 32,000 terms took 0.2 s. Matching is unchanged.
#[test]
fn tool_catalog_matches_each_term_against_each_description_once() {
    let (_dir, mut s) = document("# Doc\n");
    let started = Instant::now();
    let query = format!("{} section", "zq ".repeat(32_000));
    let result = tools::execute(&mut s, "tool_catalog", json!({"query":query})).unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    let names: Vec<_> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"document_edit"), "{names:?}");
    let none = tools::execute(&mut s, "tool_catalog", json!({"query":"zq zq"})).unwrap();
    assert!(none["tools"].as_array().unwrap().is_empty(), "{none}");
}

/// A history search prepared its search for the whole query against every
/// string of every retained message, which a long query made slow even
/// over a small history.
#[test]
fn a_long_history_query_is_not_prepared_per_message() {
    let (_dir, mut s) = document("# Doc\n");
    for i in 0..300 {
        let content = json!({"status":"ok","data":{"items":(0..20).map(|j| format!("item {i} {j}")).collect::<Vec<_>>()}});
        s.history.push(
            vec![
                json!({"role":"tool","tool_call_id":format!("c{i}"),"content":content.to_string()}),
            ],
            true,
        );
    }
    let query = "zq ".repeat(100_000);
    let started = Instant::now();
    let result = s.history.search(&query, 0, 10);
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert!(result["items"].as_array().unwrap().is_empty(), "{result}");
    let found = s.history.search("ITEM 7 3", 0, 10);
    assert_eq!(found["items"].as_array().unwrap().len(), 1, "{found}");
}

/// Counting matches searched again after each one: a 2000-character
/// old_text in a 4 MiB file of one repeated character took 28 s to be
/// refused as ambiguous, and replacing every character with 200 allocated
/// 800 MB before the size check refused it.
#[test]
fn file_edit_checks_ambiguity_and_result_size_without_rescanning() {
    let dir = tempfile::tempdir().unwrap();
    let body = "a".repeat(4 << 20);
    write(&dir, "a.txt", &body);
    write(&dir, "b.txt", "xaay");
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("out.md"),
            ..Default::default()
        },
        support::compact_config(),
    );
    let digest = tools::hash(body.as_bytes());
    let started = Instant::now();
    let error = tools::execute(
        &mut s,
        "file_edit",
        json!({"path":"a.txt","old_text":"a".repeat(2000),"new_text":"b","expected_hash":digest}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("ambiguous_text: 2097 matches"), "{error}");
    let error = tools::execute(
        &mut s,
        "file_edit",
        json!({"path":"a.txt","old_text":"a","new_text":"b".repeat(200),"expected_hash":digest,"replace_all":true}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("unsupported_large_file"), "{error}");
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        body
    );
    // Overlapping occurrences are still ambiguous; a single one is replaced.
    let digest = tools::hash(b"xaay");
    let error = tools::execute(
        &mut s,
        "file_edit",
        json!({"path":"b.txt","old_text":"a","new_text":"b","expected_hash":digest}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("ambiguous_text: 2 matches"), "{error}");
    write(&dir, "b.txt", "xaaay");
    let digest = tools::hash(b"xaaay");
    let error = tools::execute(
        &mut s,
        "file_edit",
        json!({"path":"b.txt","old_text":"aa","new_text":"b","expected_hash":digest}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("ambiguous_text: 2 matches"), "{error}");
    tools::execute(
        &mut s,
        "file_edit",
        json!({"path":"b.txt","old_text":"aa","new_text":"b","expected_hash":digest,"replace_all":true}),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "xbay"
    );
}

/// Each appended heading was compared with every heading of the document
/// three times: 2000 headings appended to 40,000 took 6 s. The checks still
/// refuse a duplicate, a reused number and an out-of-order number.
#[test]
fn append_checks_headings_against_indexes_of_the_document() {
    let mut doc = String::from("# Title\n\n");
    for i in 0..40_000 {
        doc.push_str(&format!("## {i}. Section {i}\n\ntext\n\n"));
    }
    let (_dir, mut s) = document(&doc);
    let text: String = (0..2000)
        .map(|j| format!("## {}. New {j}\n\nx\n\n", 40_000 + j))
        .collect();
    let started = Instant::now();
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","text":text,"expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    let digest = saved["hash"].as_str().unwrap().to_owned();
    for (text, refusal) in [
        ("## 7. Section 7\n\ny\n", "which the document already has"),
        ("## 7. Other\n\ny\n", "would share that number"),
        ("## 39. Late\n\ny\n", "would share that number"),
        (
            "## 50000. Last\n\n## 3.5 Early\n\ny\n",
            "would be out of order",
        ),
    ] {
        let error = tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"append","text":text,"expected_hash":digest}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(refusal), "{text:?}: {error}");
    }
}

/// A missing section's closest headings were ranked by matching each
/// bigram of the requested name against each heading's: a 3000-character
/// name took 1.8 s against 40,000 headings.
#[test]
fn closest_headings_for_a_long_missing_section_are_ranked_in_linear_time() {
    let mut doc = String::from("# Title\n\n");
    for i in 0..40_000 {
        doc.push_str(&format!("## {i}. Section about topic {i}\n\ntext\n\n"));
    }
    doc.push_str("## Overview of the parser\n\ntext\n");
    let (_dir, mut s) = document(&doc);
    let section = format!("Overview of the parsr {}", "q".repeat(30_000));
    let started = Instant::now();
    let error = tools::execute(&mut s, "document_inspect", json!({"section":section}))
        .unwrap_err()
        .to_string();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert!(error.starts_with("section_not_found"), "{error}");
    let error = tools::execute(
        &mut s,
        "document_inspect",
        json!({"section":"Overview of the parsr"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("\"heading\":\"## Overview of the parser\""),
        "{error}"
    );
}

/// fix_word widens its anchor line by line until it is unique. Each width
/// scanned the document (2.9 s for a word among 20,000 repeated lines), and
/// uniqueness was judged without overlaps or the final line break, so the
/// anchor it chose occurred twice and replace_text refused the fix.
#[test]
fn fix_word_anchors_a_word_among_repeated_lines() {
    for lines in [20, 20_000] {
        let doc: String = std::iter::once("# Title\n\n".to_owned())
            .chain((0..lines).map(|_| "The 설정д here\n".to_owned()))
            .collect();
        let (_dir, mut s) = document(&doc);
        let audit = tools::execute(&mut s, "document_audit", json!({"limit":1})).unwrap();
        let first = audit["issues"][0]["fix_id"].as_str().unwrap().to_owned();
        let middle = lines / 2;
        let started = Instant::now();
        let saved = tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"fix_word","fix_id":format!("{first}-{middle}"),"text":"설정","expected_hash":tools::hash(doc.as_bytes())}),
        )
        .unwrap();
        assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
        let text = std::fs::read_to_string(s.project.output.clone()).unwrap();
        let fixed: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| *line == "The 설정 here")
            .map(|(index, _)| index + 1)
            .collect();
        // Line 3 holds the first copy.
        assert_eq!(fixed, [2 + middle], "{}", saved["hash"]);
        assert_eq!(text.len(), doc.len() - "д".len());
    }
}

/// Each citation on a line looked back to the previous word separator and
/// ahead to the next bracket: a 256 KB line of 20,000 citations took 2.8 s
/// to audit and 3.5 s to save, 16 s and 20 s with no space between them.
#[test]
fn citations_on_one_long_line_are_scanned_in_linear_time() {
    for separator in [", ", ","] {
        let cited: Vec<String> = (0..20_000)
            .map(|i| format!("src/a.rs:{}", 1 + i % 50))
            .collect();
        let doc = format!(
            "# Title\n\nClaims {}. See https://example.com/src/a.rs:3 and [a.rs:2](src/a.rs:2).\n",
            cited.join(separator)
        );
        let (dir, mut s) = document(&doc);
        write(
            &dir,
            "src/a.rs",
            &(0..50)
                .map(|i| format!("fn f{i}() {{}}\n"))
                .collect::<String>(),
        );
        let started = Instant::now();
        let audit = tools::execute(&mut s, "document_audit", json!({"limit":5})).unwrap();
        assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
        // The URL and the link label are not citations; the link target is.
        assert_eq!(audit["citations_checked"], 20_001, "{audit}");
    }
}

/// Each heading in a Markdown code block was looked up among all real
/// headings, and each block searched them for the one above it: 40,000
/// numbered headings and a block repeating the numbering took 2.75 s per
/// audit (release build).
#[test]
fn headings_in_a_code_block_are_checked_against_an_index() {
    let n = 40_000;
    let mut doc = String::from("# Title\n\n");
    for i in 0..n {
        doc.push_str(&format!("## 1.{i} H\n\nx\n\n"));
    }
    doc.push_str("```markdown\n");
    for i in 0..n {
        doc.push_str(&format!("## 1.{} H\n", n + i));
    }
    doc.push_str("```\n");
    let (_dir, mut s) = document(&doc);
    let started = Instant::now();
    let audit = tools::execute(&mut s, "document_audit", json!({"limit":5})).unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(
        audit["issue_kinds"]["headings_inside_code_block"], 1,
        "{audit}"
    );
}

/// Every line of every section was tested against every cited footnote
/// label: 16,000 footnotes took 12 s (release build) to find sections
/// showing uncited code.
#[test]
fn sections_citing_by_footnote_are_found_by_label() {
    let n = 16_000;
    let mut doc = String::from("# Title\n\n");
    for i in 0..n {
        // The first section's reference has no cited note (the last one's
        // text runs on through the notes, which cite).
        let note = if i == 0 { "other" } else { "n" };
        doc.push_str(&format!(
            "## {i}. Sec\n\nText [^{note}{i}].\n\n```rust\nfn x() {{}}\n```\n\n"
        ));
    }
    for i in 0..n {
        doc.push_str(&format!("[^n{i}]: src/a.rs:{}\n", 1 + i % 5));
    }
    let (dir, s) = document(&doc);
    write(
        &dir,
        "src/a.rs",
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n",
    );
    let started = Instant::now();
    let check = tools::uncited_code(&s).unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(check["flagged"], 1, "{check}");
    assert_eq!(check["items"][0]["heading"], "## 0. Sec");
}

/// Each missing citation path went through every project file and every
/// delivered source: 1000 of them in a 20,000-file project took a save 5 s
/// (release build).
#[test]
fn missing_citation_paths_look_up_project_files_by_name() {
    let distinct = 1000;
    let mut doc = String::from("# Title\n\nThe helper is in f7.rs:1 and deep/f8.rs:1.\n\n");
    for i in 0..distinct {
        doc.push_str(&format!("Claim {i} (lib/nofile_{i}.rs:1).\n\n"));
    }
    let (dir, mut s) = document(&doc);
    for i in 0..5000 {
        write(
            &dir,
            &format!("src/d{}/deep/f{i}.rs", i % 10),
            "fn f() {}\n",
        );
    }
    let started = Instant::now();
    let audit = tools::execute(&mut s, "document_audit", json!({"limit":5})).unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(
        audit["issue_kinds"]["citation_path"],
        distinct + 2,
        "{audit}"
    );
    let suggested: Vec<&str> = audit["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|issue| issue["suggested"].as_str())
        .collect();
    assert_eq!(suggested, ["src/d7/deep/f7.rs:1", "src/d8/deep/f8.rs:1"]);
}

/// The tokenizer joins letters and combining marks (o200k) into one piece:
/// Japanese in decomposed form never formed a long run of one kind, so a
/// 120 KB line went to the tokenizer whole and took 2.2 s (release build).
#[test]
fn text_alternating_letters_and_marks_is_counted_in_parts() {
    let text = "\u{304b}\u{3099}".repeat(100_000);
    let started = Instant::now();
    let count = mnemoarc::context::tokens(&text, "gpt-4o");
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(count, 300_000);
}

/// A section replacement compared each new numbered heading with every
/// heading of the document: 2000 numbered sections replaced into a
/// 40,000-heading document took 2 s (optimized). A number its siblings
/// already have is still refused.
#[test]
fn section_replacement_checks_numbered_siblings_against_an_index() {
    let sections = 40_000;
    let mut doc = String::from("# Title\n\n");
    for i in 0..sections {
        doc.push_str(&format!("## {i}. Section {i}\n\ntext\n\n"));
    }
    let (_dir, mut s) = document(&doc);
    let mut text = String::from("## 0. Section 0\n\nnew\n\n");
    for j in 0..2000 {
        text.push_str(&format!("## {}. New {j}\n\nx\n\n", sections + j));
    }
    let clash = format!("{text}## 7. Another seven\n\nx\n");
    let started = Instant::now();
    let refused = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## 0. Section 0","text":clash,"expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("already has section 7 as \"## 7. Section 7\""),
        "{refused}"
    );
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## 0. Section 0","text":text,"expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap();
    assert_eq!(saved["inserted_sections"].as_array().unwrap().len(), 2000);
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
}

/// A batch kept a copy of the document per operation and hashed it three
/// times per operation (32 edits of a 14 MB document held 1 GB, optimized),
/// and a section hash that did not match hashed the whole document again
/// per edit. Failures are now explained by replaying the applied edits:
/// the hint still names the earlier edit that changed an old_text, and the
/// per-operation hashes are those of the documents in between.
#[test]
fn batch_failures_are_explained_by_replaying_the_applied_edits() {
    let filler = "A paragraph of background text for the section. ".repeat(40);
    let mut doc = String::from("# Title\n\n");
    for i in 0..1500 {
        doc.push_str(&format!(
            "## {i}. Section {i}\n\nmarker {i} here\n\n{filler}\n\n"
        ));
    }
    let (dir, mut s) = document(&doc);
    s.document_written = true;
    let mut edits =
        vec![json!({"action":"replace_text","old_text":"marker 0 here","text":"changed"})];
    edits.push(json!({"action":"replace_text","old_text":"marker 0 here","text":"again"}));
    for i in 1..31 {
        edits.push(json!({"action":"section","section":format!("## {i}. Section {i}"),"text":format!("## {i}. Section {i}\n\nnew\n"),"expected_section_hash":tools::hash(b"stale")}));
    }
    let started = Instant::now();
    let refused = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"edits":edits,"expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("index=1; action=replace_text")
            && refused.contains("edits[0] in this same batch already changed it"),
        "{}",
        &refused[..refused.len().min(2000)]
    );
    assert!(refused.contains("index=30; action=section; cause=section_revision_conflict"));
    let edits: Vec<_> = (0..32)
        .map(|i| json!({"action":"replace_text","old_text":format!("marker {i} here"),"text":format!("changed {i}")}))
        .collect();
    let saved = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"edits":edits,"expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap();
    let saved_doc = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
    assert_eq!(saved["hash"], tools::hash(saved_doc.as_bytes()));
    assert_eq!(saved["operations"][31]["hash"], saved["hash"]);
    assert_eq!(saved["operations"][5]["changed"], true);
    let mut after_six = doc.clone();
    for i in 0..6 {
        after_six = after_six.replacen(&format!("marker {i} here"), &format!("changed {i}"), 1);
    }
    assert_eq!(
        saved["operations"][5]["hash"],
        tools::hash(after_six.as_bytes())
    );
    assert!(saved_doc.contains("changed 31"));
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
}

/// Models repeat the file_read hash on every file_patch operation; the
/// original and the edited text were hashed again for each one: 32
/// operations on a 15 MB file took 2 s in release builds (0.27 s without
/// the hash). The original's hash is computed once, and a later hash that
/// names neither version is still refused.
#[test]
fn file_patch_hashes_the_original_once_for_repeated_hashes() {
    let (dir, mut s) = document("# Doc\n");
    // Project files are written outside the source_document workflow.
    s.task.workflow = String::new();
    s.workflow_mode = String::new();
    let body: String = (0..400_000).map(|j| format!("line {j} value\n")).collect();
    write(&dir, "big.txt", &body);
    let digest = tools::hash(body.as_bytes());
    let ops: Vec<_> = (0..32)
        .map(|j| json!({"action":"update","path":"big.txt","old_text":format!("line {} value\n", j * 1000 + 7),"new_text":format!("edited {j}\n"),"expected_hash":digest}))
        .collect();
    let started = Instant::now();
    let patched = tools::execute(&mut s, "file_patch", json!({"operations":ops})).unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    let text = std::fs::read_to_string(dir.path().join("big.txt")).unwrap();
    assert!(text.contains("edited 0\n") && text.contains("edited 31\n"));
    assert!(!text.contains("line 31007 value"));
    assert_eq!(patched["hash"], tools::hash(text.as_bytes()));
    let current = tools::hash(text.as_bytes());
    let refused = tools::execute(
        &mut s,
        "file_patch",
        json!({"operations":[
            {"action":"update","path":"big.txt","expected_hash":current,"old_text":"edited 0\n","new_text":"again\n"},
            {"action":"update","path":"big.txt","expected_hash":digest,"old_text":"edited 1\n","new_text":"again 1\n"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(refused.starts_with("file_revision_conflict"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("big.txt")).unwrap(),
        text
    );
}

/// A complete whose earlier items the same batch finishes waits and is
/// tried again after every applied operation, each try on a copy of the
/// task state with its details (up to memory_bytes): 16 completes sent in
/// reverse order took 2.4 s with 22 MB of details in release builds. The
/// tries copy only the plan; the details stay as they were.
#[test]
fn waiting_plan_operations_are_tried_on_the_plan_alone() {
    let (_dir, mut s) = document("# Doc\n");
    // The default memory_bytes, which bounds the serialized task state.
    s.config.memory_bytes = 32 * 1024 * 1024;
    s.task.details = (0..100_000)
        .map(|i| json!({"note":format!("detail {i} {}", "x".repeat(200))}))
        .collect();
    let texts: Vec<String> = (1..=16)
        .map(|i| format!("Step number {i} of the work"))
        .collect();
    let planned = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":0,"operations":[{"op":"insert","texts":texts}]}),
    )
    .unwrap();
    let operations: Vec<_> = (1..=16)
        .rev()
        .map(|i| json!({"op":"complete","id":format!("T{i}"),"result":"done"}))
        .collect();
    let started = Instant::now();
    let applied = tools::execute(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":planned["plan"]["revision"],"operations":operations}),
    )
    .unwrap();
    assert!(started.elapsed() < LIMIT, "{:?}", started.elapsed());
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(applied["plan"]["pending_count"], 0);
    assert_eq!(s.task.todos_completed_total, 16);
    assert_eq!(s.task.details.len(), 100_000);
    assert_eq!(s.task.details[7]["note"].as_str().unwrap().len(), 209);
}

/// Tool review 2026-10-23: a read the batch share cut from 120 lines to a
/// few kept the observation of all 120 lines, so a citation of a line the
/// model never received passed the unread check and counted as progress.
/// A view delivered whole before stays evidence.
#[test]
fn a_read_cut_after_it_ran_attests_only_the_delivered_lines() {
    let (dir, mut s) = document("# T\n");
    let body: String = (0..120)
        .map(|i| format!("    let value_{i} = compute_something(argument_{i}); // comment {i}\n"))
        .collect();
    write(&dir, "src/a.rs", &body);
    let read = |s: &mut Session, id: &str| {
        let call = mnemoarc::llm::ToolCall {
            id: id.into(),
            name: "file_read".into(),
            arguments: json!({"path":"src/a.rs","max_lines":120,"force_read":true}).to_string(),
        };
        let full = tools::run_call(s, &call);
        (call, full)
    };
    let (call, full) = read(&mut s, "r1");
    assert_eq!(full["data"]["content"]["line_end"], 120, "{full}");
    let cut = tools::limit_result(&mut s, &call, full, 400);
    tools::record_delivered_read(&mut s, &call, &cut);
    let delivered = cut["data"]["content"]["line_end"].as_u64().unwrap();
    assert!(delivered < 100, "{cut}");
    assert!(cut["archive_id"].is_null(), "{cut}");
    assert_eq!(cut["next_cursor"]["tool"], "file_read");
    assert!(
        s.sources
            .values()
            .all(|source| source.end_line <= Some(delivered as usize))
    );
    std::fs::write(
        dir.path().join("summary.md"),
        "# T\n\n## A\n\nThe value is computed (src/a.rs:100-100).\n",
    )
    .unwrap();
    assert_eq!(tools::unread_citations(&s).unwrap().len(), 1);
    // Delivered whole once, the view stays evidence when a later identical
    // read is cut.
    let (call, full) = read(&mut s, "r2");
    tools::record_delivered_read(&mut s, &call, &full);
    let whole = full["data"]["source"]["id"].as_str().unwrap().to_owned();
    assert!(tools::unread_citations(&s).unwrap().is_empty());
    let (call, again) = read(&mut s, "r3");
    assert_eq!(again["data"]["source"]["id"], whole.as_str());
    tools::limit_result(&mut s, &call, again, 400);
    assert!(s.sources.contains_key(&whole));
    assert!(tools::unread_citations(&s).unwrap().is_empty());
}

/// Tool review 2026-10-23: a page shortened to fit kept an archive of the
/// whole page and pointed to it although its own cursor continued it;
/// shortened search pages filled the history with copies of themselves.
#[test]
fn a_shortened_page_with_its_own_cursor_keeps_no_archive() {
    let (dir, mut s) = document("# T\n");
    let body: String = (0..300)
        .map(|i| format!("    let value_{i} = compute(argument_{i}); // needle {i}\n"))
        .collect();
    write(&dir, "src/a.rs", &body);
    let bundles = s.history.bundles.len();
    let call = mnemoarc::llm::ToolCall {
        id: "s1".into(),
        name: "source_search".into(),
        arguments: json!({"query":"needle","limit":100}).to_string(),
    };
    let full = tools::run_call(&mut s, &call);
    let page = tools::limit_result(&mut s, &call, full, 800);
    let rows = page["data"]["matches"].as_array().unwrap().len();
    assert!(rows > 0 && rows < 100, "{rows}");
    assert!(page["data"]["next_cursor"].is_string(), "{page}");
    assert!(
        page["archive_id"].is_null() && page["next_cursor"].is_null(),
        "{page}"
    );
    assert_eq!(s.history.bundles.len(), bundles);
}
