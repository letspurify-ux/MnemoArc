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
