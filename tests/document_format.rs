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
            output: dir.path().join("guide.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128_000),
            ..Default::default()
        },
    );
    session.select_workflow("source_document").unwrap();
    session.active_tools = ToolRegistry::optional_names();
    (dir, session)
}

fn run(s: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(s, name, args).unwrap()
}

fn create(s: &mut Session, text: &str) -> Value {
    run(s, "document_edit", json!({"action":"create","text":text}))
}

#[test]
fn write_reports_padded_and_discarded_table_cells_without_losing_the_draft() {
    let (_dir, mut s) = setup();
    let doc = "# Guide\n\n| A | B |\n| --- | --- |\n| one |\n| one | two | three |\n";
    let written = create(&mut s, doc);
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), doc);
    let format = &written["format_check"];
    assert_eq!(format["ok"], false, "{format}");
    assert_eq!(format["markdown_tables_checked"], 1);
    assert_eq!(format["issue_count"], 2);
    assert_eq!(format["issues"][0]["line"], 5);
    assert_eq!(format["issues"][0]["actual_columns"], 1);
    assert_eq!(format["issues"][1]["line"], 6);
    assert_eq!(format["issues"][1]["actual_columns"], 3);
    assert_eq!(format["issues"][1]["expected_columns"], 2);
}

#[test]
fn detects_header_delimiter_mismatch_and_malformed_delimiters() {
    for (table, kind) in [
        ("| A | B |\n| --- |\n", "markdown_table_header_columns"),
        ("| A | B |\n| --- | :: |\n", "markdown_table_delimiter"),
    ] {
        let (_dir, mut s) = setup();
        let format = create(&mut s, &format!("# Guide\n\n{table}"))["format_check"].clone();
        assert_eq!(format["issue_count"], 1, "{format}");
        assert_eq!(format["issues"][0]["kind"], kind);
        assert_eq!(format["issues"][0]["line"], 4);
    }
}

#[test]
fn valid_tables_accept_empty_cells_escaped_pipes_containers_and_line_endings() {
    let doc = "# 안내\n\n| 이름 | 값 |\n| :-- | --: |\n| 빈 값 | |\n| 코드 | `a\\|b` |\n| 문자 | a\\|b |\n\n> | A | B |\n> | --- | --- |\n> | x | y |\n\n- 항목\n\n  | A | B |\n  | --- | --- |\n  | x | y |\n";
    for doc in [
        doc.to_owned(),
        doc.replace('\n', "\r\n"),
        doc.replace('\n', "\r"),
    ] {
        let (_dir, mut s) = setup();
        let format = create(&mut s, &doc)["format_check"].clone();
        assert_eq!(format["ok"], true, "{format}");
        assert_eq!(format["markdown_tables_checked"], 3, "{format}");
        assert_eq!(format["checks_complete"], true);
        assert_eq!(format["ui_render_verified"], false);
    }
}

#[test]
fn literal_examples_and_comments_do_not_run_diagram_or_table_checks() {
    let (_dir, mut s) = setup();
    let doc = "# Guide\n\n````markdown\n```mermaid\nflowchart LR\nA[broken\n```\n| A | B |\n| --- |\n````\n\n<!--\n```mermaid\nbroken\n```\n| A | B |\n| --- |\n-->\n\n`| A | B |\n| --- |`\n";
    let format = create(&mut s, doc)["format_check"].clone();
    assert_eq!(format["ok"], true, "{format}");
    assert_eq!(format["mermaid_blocks"], 0);
    assert_eq!(format["markdown_tables_checked"], 0);
    assert_eq!(format["warning_count"], 0);
}

#[test]
fn checks_common_valid_mermaid_grammars_including_implicit_participants() {
    let diagrams = [
        "flowchart LR; A[요청] --> B{기억}; B -->|Yes| C[완료]",
        "sequenceDiagram\nparticipant Alice\nAlice->>Bob: Hello\nBob-->>Alice: Hi",
        "classDiagram\nclass User {\n+String name\n}\nUser --> Account",
        "stateDiagram-v2\n[*] --> Ready\nReady --> [*]",
        "erDiagram\nUSER ||--o{ ORDER : places",
        "pie title Usage\n\"Memory\" : 60\n\"Plan\" : 40",
        "---\ntitle: Example\n---\nflowchart TD\nA --> B",
        "%%{init: {'theme': 'neutral'}}%%\nflowchart LR\nA --> B",
    ];
    let (_dir, mut s) = setup();
    let doc = format!(
        "# Guide\n\n{}",
        diagrams
            .iter()
            .map(|text| format!("```mermaid\n{text}\n```\n\n"))
            .collect::<String>()
    );
    let format = create(&mut s, &doc)["format_check"].clone();
    assert_eq!(format["ok"], true, "{format}");
    assert_eq!(format["mermaid_blocks_checked"], diagrams.len());
    assert_eq!(format["checks_complete"], true);
}

#[test]
fn malformed_mermaid_reports_the_document_block_and_parser_diagnostic() {
    for diagram in [
        "flowchart LR\nA[broken --> B",
        "flowchart LR\nsubgraph Group\nA --> B",
        "flowchart LR\nA -->",
        "sequenceDiagram\nAlice-->: missing participant",
        "",
    ] {
        let (_dir, mut s) = setup();
        let format =
            create(&mut s, &format!("# 안내\n\n~~~mermaid\n{diagram}\n~~~\n"))["format_check"]
                .clone();
        assert_eq!(format["ok"], false, "diagram={diagram:?}, {format}");
        assert_eq!(format["issue_count"], 1);
        assert_eq!(format["issues"][0]["kind"], "mermaid_syntax_error");
        assert_eq!(format["issues"][0]["line"], 3);
        assert_eq!(format["issues"][0]["diagram_start_line"], 4);
        assert!(!format["issues"][0]["message"].as_str().unwrap().is_empty());
    }
}

#[test]
fn checks_nested_mermaid_fences_and_deduplicates_unclosed_fences_in_audit() {
    let (_dir, mut s) = setup();
    let format = create(
        &mut s,
        "# Guide\n\n> ```mermaid\n> flowchart LR\n> A -->\n> ```\n\n- ```rust\n  fn example() {}\n",
    )["format_check"]
        .clone();
    assert_eq!(format["issue_count"], 2, "{format}");
    assert_eq!(format["issues"][0]["kind"], "mermaid_syntax_error");
    assert_eq!(format["issues"][0]["line"], 3);
    assert_eq!(format["issues"][1]["kind"], "unclosed_code_fence");
    assert_eq!(format["issues"][1]["line"], 8);
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], false);
    assert_eq!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|issue| issue["kind"] == "unclosed_code_fence")
            .count(),
        1,
        "{audit}"
    );
}

#[test]
fn ui_extensions_and_check_limits_are_explicitly_unchecked() {
    let (_dir, mut s) = setup();
    let doc = format!(
        "# Guide\n\n| A | B |\n| --- | --- |\n| math | $a|b$ |\n\n| A | B |\n| --- | --- |\n| diagram | `mermaid\\nflowchart LR\\nA -->|yes| B` |\n\n```mermaid\nflowchart LR\\nA --> B\n```\n\n```mermaid\nflowchart LR\nA[\"$$x^2$$\"] --> B\n```\n\n```mermaid\nflowchart LR\n%% {}\nA --> B\n```\n",
        "x".repeat(64 * 1024)
    );
    let format = create(&mut s, &doc)["format_check"].clone();
    assert_eq!(format["ok"], true, "{format}");
    assert_eq!(format["checks_complete"], false);
    assert_eq!(format["mermaid_blocks"], 3);
    assert_eq!(format["mermaid_blocks_unchecked"], 3);
    assert_eq!(format["markdown_tables_checked"], 0);
    assert_eq!(format["warning_count"], 5, "{format}");
}

#[test]
fn tolerated_mermaid_directives_warn_instead_of_blocking_the_document() {
    let (_dir, mut s) = setup();
    let format = create(
        &mut s,
        "# Guide\n\n```mermaid\n%%{init: {broken} }%%\nflowchart LR\nA --> B\n```\n",
    )["format_check"]
        .clone();
    assert_eq!(format["ok"], true, "{format}");
    assert_eq!(format["mermaid_blocks_checked"], 0);
    assert_eq!(format["mermaid_blocks_unchecked"], 1);
    assert_eq!(format["checks_complete"], false);
    assert_eq!(format["warnings"][0]["kind"], "mermaid_directive_unchecked");
}

#[test]
fn rich_table_headers_and_display_math_are_not_mistaken_for_broken_gfm() {
    for header in [
        "$a|b$",
        "$$a|b$$",
        "\\(a|b\\)",
        "\\[a|b\\]",
        "`mermaid\\nflowchart LR\\nA -->|yes| B`",
    ] {
        let (_dir, mut s) = setup();
        let format = create(
            &mut s,
            &format!("# Guide\n\n| {header} | B |\n| --- | --- |\n| x | y |\n"),
        )["format_check"]
            .clone();
        assert_eq!(format["ok"], true, "header={header}, {format}");
        assert_eq!(
            format["checks_complete"], false,
            "header={header}, {format}"
        );
        assert_eq!(
            format["warnings"][0]["kind"],
            "markdown_ui_extension_unchecked"
        );
    }
}

#[test]
fn serialized_carriage_returns_are_left_to_the_ui_instead_of_false_syntax_errors() {
    let (_dir, mut s) = setup();
    let format = create(
        &mut s,
        "# Guide\n\n```mermaid\nflowchart LR\\r A --> B\n```\n",
    )["format_check"]
        .clone();
    assert_eq!(format["ok"], true, "{format}");
    assert_eq!(format["checks_complete"], false);
    assert_eq!(format["mermaid_blocks_unchecked"], 1);
    assert_eq!(
        format["warnings"][0]["kind"],
        "mermaid_ui_extension_unchecked"
    );
}

fn verify(s: &mut Session, source: &Value) {
    run(
        s,
        "investigation",
        json!({"action":"verify","id":"guide","source_ids":[source],"verification_note":"Compared the guide with source.rs:1"}),
    );
}

#[test]
fn format_errors_block_final_preflight_and_batch_repairs_clear_them() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    let source = run(&mut s, "file_read", json!({"path":"source.rs"}))["source"]["id"].clone();
    let written = create(
        &mut s,
        "# Guide\nSee source.rs:1.\n\n| A | B |\n| --- | --- |\n| one |\n\n```mermaid\nflowchart LR\nA -->\n```\n",
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"guide","title":"Guide","section":"# Guide","status":"written"}),
    );
    verify(&mut s, &source);
    let preflight = run(&mut s, "investigation", json!({"action":"final_check"}));
    assert_eq!(preflight["complete"], false, "{preflight}");
    assert_eq!(preflight["audit"]["issue_count"], 2);
    assert_eq!(preflight["audit"]["format_check"]["ok"], false);
    let edited = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":written["hash"],"edits":[
            {"action":"replace_text","old_text":"| one |","text":"| one | two |"},
            {"action":"replace_text","old_text":"A -->\n","text":"A --> B\n"}
        ]}),
    );
    assert_eq!(edited["format_check"]["ok"], true, "{edited}");
    verify(&mut s, &source);
    let preflight = run(&mut s, "investigation", json!({"action":"final_check"}));
    assert_eq!(preflight["complete"], true, "{preflight}");
}

#[test]
fn audit_paginates_all_format_errors_and_rejects_a_changed_document_revision() {
    let (_dir, mut s) = setup();
    let doc = format!(
        "# Guide\n\n| A | B |\n| --- | --- |\n{}",
        "| x |\n".repeat(12)
    );
    let written = create(&mut s, &doc);
    assert_eq!(
        written["format_check"]["issues"].as_array().unwrap().len(),
        8
    );
    assert_eq!(written["format_check"]["issues_truncated"], true);
    let mut offset = 0;
    let mut revision = Value::Null;
    let mut table_lines = Vec::new();
    loop {
        let page = run(
            &mut s,
            "document_audit",
            json!({"offset":offset,"limit":3,"expected_revision":revision}),
        );
        revision = page["revision"].clone();
        for issue in page["issues"].as_array().unwrap() {
            if issue["kind"] == "markdown_table_columns" {
                table_lines.push(issue["line"].as_u64().unwrap());
            }
        }
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        offset = next;
    }
    assert_eq!(table_lines, (5..17).collect::<Vec<_>>());
    std::fs::write(&s.project.output, doc.replace("| x |", "| x | y |")).unwrap();
    let error = tools::execute(
        &mut s,
        "document_audit",
        json!({"offset":3,"expected_revision":revision}),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("document_audit_revision_conflict:")
    );
}

#[test]
fn answer_workflow_keeps_the_plain_write_contract() {
    let (_dir, mut s) = setup();
    s.select_workflow("answer").unwrap();
    let written = create(&mut s, "# Guide\n```mermaid\nbroken\n```\n");
    assert!(written.get("format_check").is_none(), "{written}");
    assert!(written.get("citation_check").is_none());
}
