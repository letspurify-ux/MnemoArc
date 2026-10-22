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
    assert_eq!(audit["citations_checked"], 1, "{audit}");
    assert!(
        !audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "citation_path"),
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
}

fn outline(s: &mut Session, path: &str) -> Vec<String> {
    let result = tools::execute(s, "code_outline", json!({"path":path,"view":"compact"})).unwrap();
    result["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| {
            format!(
                "{} {}",
                symbol["qualified_name"].as_str().unwrap(),
                symbol["symbol_kind"].as_str().unwrap()
            )
        })
        .collect()
}

/// TypeScript enum members were missing from the outline, a Rust union was
/// not listed (its fields stood at the top level), and the constructor of
/// an anonymous class was a method.
#[test]
fn enum_members_unions_and_anonymous_class_constructors_are_outlined() {
    let (dir, mut s) = setup();
    write(&dir, "e.ts", "enum Color {\n  Red,\n  Green = 2,\n}\n");
    write(&dir, "u.rs", "union U {\n    a: u32,\n    b: f32,\n}\n");
    write(
        &dir,
        "k.js",
        "const A = class {\n  constructor() {}\n  run() {}\n};\nexport default class {\n  constructor() {}\n}\n",
    );
    assert_eq!(
        outline(&mut s, "e.ts"),
        [
            "Color enum",
            "Color::Red enum_member",
            "Color::Green enum_member"
        ]
    );
    assert_eq!(
        outline(&mut s, "u.rs"),
        ["U struct", "U::a field", "U::b field"]
    );
    assert_eq!(
        outline(&mut s, "k.js"),
        [
            "A class",
            "A::constructor constructor",
            "A::run method",
            "constructor constructor"
        ]
    );
}

fn symbol_id(s: &mut Session, path: &str, name: &str) -> String {
    let result = tools::execute(
        s,
        "code_outline",
        json!({"path":path,"view":"compact","query":name,"match":"exact"}),
    )
    .unwrap();
    result["symbols"][0]["symbol_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A JSX attribute name was resolved as a use of the function of that name.
#[test]
fn a_jsx_attribute_name_is_no_reference() {
    let (dir, mut s) = setup();
    write(
        &dir,
        "app.jsx",
        "function onClick() { return 1; }\nfunction App() {\n  return <Button onClick={() => 1} />;\n}\nonClick();\n",
    );
    let id = symbol_id(&mut s, "app.jsx", "onClick");
    let result = tools::execute(
        &mut s,
        "symbol_relations",
        json!({"path":"app.jsx","symbol_id":id,"relation":"references"}),
    )
    .unwrap();
    let lines: Vec<_> = result["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|relation| relation["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, [5], "{result}");
}

/// A symbol ID sent with another file's path was reported as that file
/// having changed.
#[test]
fn a_symbol_id_of_another_file_says_so() {
    let (dir, mut s) = setup();
    write(&dir, "a.js", "function first() {}\n");
    write(&dir, "b.js", "function other() {}\n");
    let id = symbol_id(&mut s, "b.js", "other");
    for (tool, args) in [
        ("symbol_read", json!({"path":"a.js","symbol_id":id})),
        (
            "symbol_relations",
            json!({"path":"a.js","symbol_id":id,"relation":"callers"}),
        ),
    ] {
        let error = tools::execute(&mut s, tool, args).unwrap_err().to_string();
        assert!(error.starts_with("invalid_symbol_id:"), "{tool}: {error}");
        assert!(error.contains("another file"), "{tool}: {error}");
    }
}
