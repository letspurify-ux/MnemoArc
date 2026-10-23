//! Regressions from the 2026-10-22 review of the model-facing tools: each
//! test reproduces a defect that was confirmed against the tools before it
//! was fixed.

use crate::support;
use mnemoarc::{
    config::Project,
    llm::ToolCall,
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};

fn setup() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("summary.md"),
            ..Default::default()
        },
        support::compact_config(),
    );
    session.active_tools = ToolRegistry::optional_names();
    (dir, session)
}

fn document(doc: &str) -> (tempfile::TempDir, Session) {
    let (dir, mut s) = setup();
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

fn call(s: &mut Session, id: &str, name: &str, args: Value) -> Value {
    tools::run_call(
        s,
        &ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        },
    )
}

fn issues(result: &Value) -> Vec<Value> {
    result["data"]["issues"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// tool_catalog called basic tools selectable, and tool_select refused them.
#[test]
fn the_catalog_marks_only_optional_tools_selectable() {
    let (_dir, mut s) = setup();
    let catalog = tools::execute(&mut s, "tool_catalog", json!({})).unwrap();
    for tool in catalog["tools"].as_array().unwrap() {
        if tool["basic"] == true {
            assert_eq!(tool["selectable"], false, "{tool}");
        }
    }
    let history = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "history")
        .unwrap();
    assert_eq!(history["selectable"], false);
}

/// memory_write reads expected_revision 0 on a new key as omitted; the same
/// replacement under memory_manage was refused.
#[test]
fn a_replacement_under_a_new_key_accepts_revision_zero() {
    let (_dir, mut s) = setup();
    let old = tools::execute(
        &mut s,
        "memory_write",
        json!({"key":"old","title":"Old","summary":"s","body":"b","kind":"decision"}),
    )
    .unwrap();
    let replaced = tools::execute(
        &mut s,
        "memory_manage",
        json!({"action":"replace","ids":[old["id"]],"replacement":{"key":"new","expected_revision":0,"title":"New","summary":"s","body":"b","kind":"decision"}}),
    )
    .unwrap();
    assert_eq!(replaced["key"], "new", "{replaced}");
}

/// A whole number written as 2.0 was refused as no integer.
#[test]
fn whole_numbers_written_as_floats_are_integers() {
    let (dir, mut s) = setup();
    write(&dir, "a.rs", "l1\nl2\nl3\n");
    let result = call(
        &mut s,
        "c1",
        "file_read",
        json!({"path":"a.rs","start_line":2.0,"max_lines":1.0}),
    );
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["data"]["content"]["text"], "l2");
    let fraction = call(
        &mut s,
        "c2",
        "file_read",
        json!({"path":"a.rs","start_line":1.5}),
    );
    assert_eq!(fraction["status"], "error", "{fraction}");
}

/// Tool results are stored as JSON text, so quotes and line breaks of a file
/// were escaped twice and a search for the text as read found nothing.
#[test]
fn history_search_finds_text_with_quotes_and_line_breaks() {
    let (dir, mut s) = setup();
    write(&dir, "q.rs", "let x = \"quoted value\";\nsecond line\n");
    let read = call(&mut s, "c1", "file_read", json!({"path":"q.rs"}));
    s.history.push(
        vec![json!({"role":"tool","tool_call_id":"c1","content":read.to_string()})],
        true,
    );
    for query in ["\"quoted value\"", "\";\nsecond", "QUOTED VALUE"] {
        let found = call(
            &mut s,
            &format!("search-{}", query.len()),
            "history",
            json!({"action":"search","query":query}),
        );
        assert_eq!(
            found["data"]["items"].as_array().unwrap().len(),
            1,
            "{query:?}: {found}"
        );
    }
}

/// A bounded history read said so only inside data; the envelope had no
/// continuation.
#[test]
fn a_bounded_history_read_continues_from_the_envelope() {
    let (_dir, mut s) = setup();
    let id = s.history.push(
        vec![json!({"role":"user","content":"word ".repeat(40_000)})],
        true,
    );
    let result = call(&mut s, "c1", "history", json!({"action":"read","id":id}));
    assert_eq!(result["data"]["truncated"], true, "{result}");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["next_cursor"]["tool"], "history");
    assert_eq!(result["next_cursor"]["id"], id);
    assert_eq!(
        result["next_cursor"]["offset"],
        result["data"]["next_offset"]
    );
}

/// A byte order mark hid the first heading from the outline.
#[test]
fn a_byte_order_mark_does_not_hide_the_first_heading() {
    let doc = "\u{feff}# First\n\nbody\n\n## Second\n\nx\n";
    let (dir, mut s) = document(doc);
    let outline = tools::execute(&mut s, "document_inspect", json!({})).unwrap();
    let headings: Vec<_> = outline["outline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["heading"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(headings, ["# First", "## Second"]);
    let saved = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"# First","text":"# First\n\nnew body\n\n## Second\n\nx\n","expected_hash":tools::hash(doc.as_bytes())}),
    )
    .unwrap();
    assert!(saved["hash"].is_string(), "{saved}");
    let text = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
    assert!(text.contains("new body"), "{text}");
    assert_eq!(text.matches("# First").count(), 1, "{text}");
}

/// A host and port matched the citation pattern and was reported as a
/// missing file, which held the final answer back.
#[test]
fn a_host_and_port_is_no_citation() {
    let (dir, mut s) = document(
        "# T\n\nThe client connects to api.example.com:443 and redis.internal:6379; see src/lib.rs:1.\n",
    );
    write(&dir, "src/lib.rs", "fn main() {}\n");
    let audit = tools::execute(&mut s, "document_audit", json!({})).unwrap();
    assert!(
        !audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "citation_path"),
        "{audit}"
    );
    // A file whose name ends like a host is still cited and checked.
    let (dir, mut s) =
        document("# T\n\nThe image builds in Dockerfile.dev:9 and reads .env.example:1.\n");
    write(&dir, "Dockerfile.dev", "FROM rust\nRUN cargo build\n");
    write(&dir, ".env.example", "KEY=value\n");
    let audit = tools::execute(&mut s, "document_audit", json!({})).unwrap();
    let issues = audit["issues"].as_array().unwrap();
    assert!(
        issues
            .iter()
            .any(|issue| issue["kind"] == "citation_range"
                && issue["citation"] == "Dockerfile.dev:9"),
        "{audit}"
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue["kind"] == "unread_citation" && issue["path"] == ".env.example"),
        "{audit}"
    );
}

/// A path holding a space could not be cited at all; %20 now names it, and
/// a citation cut at the space says how to write it.
#[test]
fn a_path_with_a_space_is_cited_with_percent_twenty() {
    let (dir, mut s) = document("# T\n\nSee src/my%20file.rs:1.\n");
    write(&dir, "src/my file.rs", "fn a() {}\n");
    let audit = call(&mut s, "c1", "document_audit", json!({}));
    assert!(
        !issues(&audit)
            .iter()
            .any(|issue| issue["kind"] == "citation_path"),
        "{audit}"
    );
    assert!(
        issues(&audit)
            .iter()
            .any(|issue| issue["kind"] == "unread_citation" && issue["path"] == "src/my file.rs"),
        "{audit}"
    );
    let (dir, mut s) = document("# T\n\nSee src/my file.rs:1.\n");
    write(&dir, "src/my file.rs", "fn a() {}\n");
    let audit = call(&mut s, "c2", "document_audit", json!({}));
    let cut = issues(&audit)
        .into_iter()
        .find(|issue| issue["kind"] == "citation_path")
        .unwrap();
    assert!(
        cut["guidance"]
            .as_str()
            .unwrap()
            .contains("src/my%20file.rs:1"),
        "{cut}"
    );
}

/// Edits sent as JSON text kept blank placeholder fields that edits sent as
/// an array lose, and the batch was refused.
#[test]
fn edits_sent_as_text_lose_blank_placeholders_too() {
    let doc = "# T\n\n## A\n\nalpha\n";
    let (dir, mut s) = document(doc);
    let edits = json!([{"action":"replace_text","old_text":"alpha","text":"beta","section":"","fix_id":""}]).to_string();
    let result = call(
        &mut s,
        "c1",
        "document_edit_batch",
        json!({"expected_hash":tools::hash(doc.as_bytes()),"edits":edits}),
    );
    assert_eq!(result["status"], "ok", "{result}");
    let text = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
    assert!(text.contains("beta"));
}

/// New files kept the private mode of the temporary file they were written
/// to (0600), unlike a file any editor creates.
#[cfg(unix)]
#[test]
fn new_files_get_the_ordinary_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut s) = setup();
    let ordinary = dir.path().join("ordinary.txt");
    std::fs::write(&ordinary, "x").unwrap();
    let expected = std::fs::metadata(&ordinary).unwrap().permissions().mode() & 0o777;
    tools::execute(
        &mut s,
        "file_write",
        json!({"path":"created.txt","content":"x"}),
    )
    .unwrap();
    let mode = |path: &str| {
        std::fs::metadata(dir.path().join(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("created.txt"), expected);
    let (dir, mut s) = setup();
    s.task.workflow = "source_document".into();
    s.workflow_mode = "source_document".into();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Report\n"}),
    )
    .unwrap();
    let created = std::fs::metadata(dir.path().join("summary.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(created, expected);
}

/// fix_word anchors its edit on the word's whole line; a line that also
/// begins another line occurred twice and the fix was refused.
#[test]
fn fix_word_on_a_line_that_begins_another_line() {
    let doc = "# T\n\n설정を 확인\n\n설정を 확인 후 저장\n";
    let (dir, mut s) = document(doc);
    let audit = tools::execute(&mut s, "document_audit", json!({})).unwrap();
    let text = audit.to_string();
    let start = text.find("\"fix_id\":\"").unwrap() + 10;
    let fix_id = &text[start..start + text[start..].find('"').unwrap()];
    let fixed = call(
        &mut s,
        "c1",
        "document_edit",
        json!({"action":"fix_word","fix_id":fix_id,"text":"설정을"}),
    );
    assert_eq!(fixed["status"], "ok", "{fixed}");
    let saved = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
    assert_eq!(saved.matches("설정을").count(), 1, "{saved}");
    assert_eq!(saved.matches("설정を").count(), 1, "{saved}");
}

/// A Korean query word with its particle or ending found nothing.
#[test]
fn memory_find_matches_korean_words_without_their_endings() {
    let (_dir, mut s) = setup();
    tools::execute(
        &mut s,
        "memory_write",
        json!({"title":"설정 저장 방식","summary":"설정은 파일에 저장된다","body":"설정 파일은 config.toml 이다","kind":"decision"}),
    )
    .unwrap();
    for query in ["설정을", "저장하는"] {
        let found = tools::execute(&mut s, "memory_find", json!({"query":query})).unwrap();
        assert_eq!(
            found["items"].as_array().unwrap().len(),
            1,
            "{query}: {found}"
        );
    }
    // A single syllable is no stem: it would match nearly every memory.
    let found = tools::execute(&mut s, "memory_find", json!({"query":"설"})).unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 0, "{found}");
}

/// Discovery hidden by document progress recovery still ran when called.
#[test]
fn discovery_withheld_by_progress_recovery_is_refused() {
    let (dir, mut s) = document("# T\n\nbody\n");
    write(&dir, "a.rs", "fn hit() {}\n");
    s.document_written = true;
    s.progress_recovery.rounds_since_best = s.config.stall_round_limit * 2;
    let offered: Vec<String> = ToolRegistry::definitions(&s)
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!offered.iter().any(|name| name == "source_search"));
    let result = call(&mut s, "c1", "source_search", json!({"query":"hit"}));
    assert_eq!(result["recovery"]["code"], "progress_recovery", "{result}");
    assert!(tools::recovery::correctable_document_error(&result));
    let read = call(&mut s, "c2", "file_read", json!({"path":"a.rs"}));
    assert_eq!(read["status"], "ok", "{read}");
}

/// The hint for path plus path_glob prefixed a glob already written through
/// the directory again (src/src/*.rs), which matched nothing.
#[test]
fn the_path_and_glob_hint_does_not_repeat_the_directory() {
    let (dir, mut s) = setup();
    write(&dir, "src/a.rs", "fn hit() {}\n");
    let result = call(
        &mut s,
        "c1",
        "file_list",
        json!({"path":"src","path_glob":"src/*.rs"}),
    );
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("path_glob:\"src/*.rs\""), "{error}");
    let result = call(
        &mut s,
        "c2",
        "file_list",
        json!({"path":"src","path_glob":"*.rs"}),
    );
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("path_glob:\"src/**/*.rs\""), "{error}");
    // A folder name holding glob syntax is escaped in the glob to send.
    write(&dir, "[draft]/a.rs", "fn hit() {}\n");
    let result = call(
        &mut s,
        "c3",
        "file_list",
        json!({"path":"[draft]","path_glob":"[draft]/*.rs"}),
    );
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("path_glob:\"[[]draft[]]/*.rs\""), "{error}");
    let listed = call(
        &mut s,
        "c4",
        "file_list",
        json!({"path_glob":"[[]draft[]]/*.rs"}),
    );
    assert_eq!(listed["data"]["paths"], json!(["[draft]/a.rs"]), "{listed}");
}

/// A continuation names its tool; a model that sent next_cursor whole as
/// the call's arguments was refused for an unknown argument `tool`.
#[test]
fn a_continuation_sent_whole_as_arguments_continues() {
    let (_dir, mut s) = setup();
    let id = s.history.push(
        vec![json!({"role":"user","content":"word ".repeat(40_000)})],
        true,
    );
    let first = call(&mut s, "c1", "history", json!({"action":"read","id":id}));
    let next = first["next_cursor"].clone();
    assert_eq!(next["tool"], "history");
    let second = call(&mut s, "c2", "history", next.clone());
    assert_eq!(second["status"], "ok", "{second}");
    let by_cursor = call(&mut s, "c3", "history", json!({"cursor":next}));
    assert_eq!(second["data"], by_cursor["data"]);
}

/// A host and port or a URL beside code is no citation of it.
#[test]
fn code_beside_a_host_and_port_is_still_uncited() {
    let doc = "# T\n\n## Setup\n\nConnect to api.example.com:443 or https://example.com/a.rs:3.\n\n```rust\nfn main() {}\n```\n";
    let (_dir, mut s) = document("");
    std::fs::remove_file(s.project.output.clone()).unwrap();
    tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":doc}),
    )
    .unwrap();
    let advice = tools::uncited_code(&s).expect("the code section is flagged");
    assert!(advice.to_string().contains("## Setup"), "{advice}");
}

/// A line longer than one page is delivered in partial pieces, none of
/// which attests the line alone; its citation stayed unread even after a
/// cursor had delivered every piece.
#[test]
fn a_line_read_whole_across_cursor_pages_is_read() {
    let long: String = (0..9000).map(|i| format!("w{i} ")).collect();
    let (dir, mut s) = document("# T\n\nThe string is long. a.rs:2\n");
    write(
        &dir,
        "a.rs",
        &format!("fn a() {{}}\nlet s = \"{long}\";\nfn b() {{}}\n"),
    );
    let deliver = |s: &mut Session, id: &str, args: Value| {
        let call = ToolCall {
            id: id.into(),
            name: "file_read".into(),
            arguments: args.to_string(),
        };
        let result = tools::run_call(s, &call);
        let limit = s.config.result_budget();
        let result = tools::limit_result(s, &call, result, limit);
        tools::record_delivered_read(s, &call, &result);
        result
    };
    let unread = |s: &Session| {
        tools::unread_citations(s)
            .unwrap()
            .iter()
            .any(|issue| issue["path"] == "a.rs" && issue["start_line"] == 2)
    };
    let mut page = deliver(&mut s, "p0", json!({"path":"a.rs"}));
    assert_eq!(page["truncated"], true, "the line spans pages");
    assert!(unread(&s), "one piece is not the line");
    let mut pages = 1;
    while let Some(cursor) = page["next_cursor"]["cursor"].as_str().map(str::to_owned) {
        page = deliver(&mut s, &format!("p{pages}"), json!({"cursor":cursor}));
        pages += 1;
        assert!(pages < 50);
    }
    assert!(pages > 2, "{pages}");
    assert!(!unread(&s), "every piece was delivered");
}
