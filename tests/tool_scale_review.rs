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
