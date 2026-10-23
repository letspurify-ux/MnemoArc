use crate::support;
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
            ..support::compact_config()
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
        // Fixed values (parser names, never-verified flags) stay out of every
        // save result, and a clean save carries no fix guidance.
        assert!(format.get("ui_render_verified").is_none(), "{format}");
        assert!(format.get("guidance").is_none(), "{format}");
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

// Live run 2026-10-13: an example block opened in 5.2 was never closed, so
// the fence meant to open the closing diagram closed it, and sections 6-8
// rendered as code. The fences paired up and the format check passed.
#[test]
fn numbered_sections_inside_an_example_block_are_reported() {
    let (_dir, mut s) = setup();
    let doc = "# Guide\n\n## 5. Drafting\n\n### 5.2 Structure\n\n```\n# Example\n## Entry Point\n\n## 6. Audit\n\n### 6.1 Check\n\n## 7. End\n\n```\nUser\n  ->\nDone\n";
    let format = create(&mut s, doc)["format_check"].clone();
    assert_eq!(format["issue_count"], 1, "{format}");
    let issue = &format["issues"][0];
    assert_eq!(issue["kind"], "headings_inside_code_block", "{format}");
    assert_eq!(issue["line"], 7);
    assert_eq!(issue["end_line"], 17);
    assert_eq!(issue["first_heading_line"], 11);
    assert_eq!(issue["heading_count"], 3);
    assert_eq!(
        issue["headings"],
        json!(["## 6. Audit", "### 6.1 Check", "## 7. End"])
    );
    // Examples that do not continue the numbering stay examples: numbers
    // real headings use, other languages' comments, and level-1 headings.
    let (_dir, mut s) = setup();
    let doc = "# Guide\n\n## 1. Intro\n\n## 2. Usage\n\n```markdown\n## 1. Intro\n## 2. Usage\n```\n\n```python\n## 3. load the data\n```\n\n```\n# 3. run it\n```\n";
    let format = create(&mut s, doc)["format_check"].clone();
    assert_eq!(format["issue_count"], 0, "{format}");
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

#[test]
fn format_errors_block_final_preflight_and_batch_repairs_clear_them() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    run(&mut s, "file_read", json!({"path":"source.rs"}));
    let written = create(
        &mut s,
        "# Guide\nSee source.rs:1.\n\n| A | B |\n| --- | --- |\n| one |\n\n```mermaid\nflowchart LR\nA -->\n```\n",
    );
    let preflight = run(&mut s, "document_audit", json!({}));
    assert_eq!(preflight["structural_ok"], false, "{preflight}");
    assert_eq!(preflight["issue_count"], 2);
    assert_eq!(preflight["format_check"]["ok"], false);
    let edited = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":written["hash"],"edits":[
            {"action":"replace_text","old_text":"| one |","text":"| one | two |"},
            {"action":"replace_text","old_text":"A -->\n","text":"A --> B\n"}
        ]}),
    );
    assert_eq!(edited["format_check"]["ok"], true, "{edited}");
    let preflight = run(&mut s, "document_audit", json!({}));
    assert_eq!(preflight["structural_ok"], true, "{preflight}");
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

// Live runs 2026-10-06..11: Korean documents kept garbled words such as
// "배치末尾로", "인프รา" and "있으면それを", and nothing flagged them.
#[test]
fn korean_words_running_into_another_script_block_the_final_until_rewritten() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    run(&mut s, "file_read", json!({"path":"source.rs"}));
    let doc = "# 가이드\n\n호출이 배치末尾로 정렬된다. source.rs:1\n\n## 도구 워커 인프รา\n\n값이 있으면それを 쓴다.\n\n\
API를 호출하고 2단계로 나눈다. 일본어(日本語)를 지원하고 「漢字」를 표시한다. `한국語` 표기는 코드다.\n\n\
```text\n배치末尾로\n```\n";
    let written = create(&mut s, doc);
    let format = &written["format_check"];
    assert_eq!(format["ok"], false, "{format}");
    let issues = format["issues"].as_array().unwrap();
    let found: Vec<_> = issues
        .iter()
        .map(|issue| {
            (
                issue["word"].as_str().unwrap(),
                issue["line"].as_u64().unwrap(),
                issue["scripts"].clone(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("배치末尾로", 3, json!(["Han"])),
            ("인프รา", 5, json!(["Thai"])),
            ("있으면それを", 7, json!(["Kana"])),
        ],
        "{format}"
    );
    assert!(
        issues
            .iter()
            .all(|issue| issue["kind"] == "mixed_script_word")
    );
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], false, "{audit}");
    assert_eq!(audit["issue_kinds"]["mixed_script_word"], 3, "{audit}");
    let edited = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":written["hash"],"edits":[
            {"action":"replace_text","old_text":"호출이 배치末尾로","text":"호출이 배치 끝으로"},
            {"action":"replace_text","old_text":"인프รา","text":"인프라"},
            {"action":"replace_text","old_text":"있으면それを","text":"있으면 그것을"}
        ]}),
    );
    assert_eq!(edited["format_check"]["ok"], true, "{edited}");
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
}

// Live run 2026-10-11: the model sent "인프라", "인ฟรา" and other spellings
// seven times for the document's "인프รา" and never matched it. A garbled
// word is fixed by the fix_id of its issue instead of by retyping it.
#[test]
fn garbled_words_are_fixed_by_fix_id_without_retyping_them() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    run(&mut s, "file_read", json!({"path":"source.rs"}));
    let doc = "# 가이드\n\n호출이 배치末尾로 정렬된다. source.rs:1\n\n## 도구 워커 인프รา\n\n값이 있으면それを 쓰고 다시 있으면それを 쓴다.\n\n```text\n배치末尾로\n```\n";
    let written = create(&mut s, doc);
    let ids = |result: &Value| -> Vec<(String, String)> {
        result["format_check"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|issue| {
                (
                    issue["word"].as_str().unwrap().to_owned(),
                    issue["fix_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let first = ids(&written);
    assert_eq!(first.len(), 4, "{written}");
    let id_of = |word: &str| first.iter().find(|(w, _)| w == word).unwrap().1.clone();
    let thai = id_of("인프รา");
    assert!(thai.starts_with('W') && thai.len() == 9, "{thai}");
    // A repeated word gets its own fix_id per copy.
    assert_eq!(first[3].1, format!("{}-2", first[2].1), "{first:?}");
    // Edits elsewhere move the lines but keep the fix_ids.
    let moved = run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"# 가이드","text":"# 가이드\n\n머리말."}),
    );
    assert_eq!(ids(&moved), first);
    // The Thai heading is fixed without copying "รา".
    let fixed = run(
        &mut s,
        "document_edit",
        json!({"action":"fix_word","fix_id":thai,"text":"인프라"}),
    );
    let saved = std::fs::read_to_string(&s.project.output).unwrap();
    assert!(saved.contains("\n## 도구 워커 인프라\n"), "{saved}");
    assert_eq!(fixed["format_check"]["issue_count"], 3, "{fixed}");
    // A stale fix_id names the ones left.
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"fix_word","fix_id":thai,"text":"인프라"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("names no garbled word"), "{error}");
    assert!(error.contains(&id_of("배치末尾로")), "{error}");
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"fix_word","text":"인프라"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("fix_id"), "{error}");
    // A batch fixes the rest; the code block keeps its text.
    let rest = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":fixed["hash"],"edits":[
            {"action":"fix_word","fix_id":id_of("배치末尾로"),"text":"배치 끝으로"},
            {"action":"fix_word","fix_id":first[2].1,"text":"있으면 그것을"},
            {"action":"fix_word","fix_id":first[3].1,"text":"있으면 그것을"}
        ]}),
    );
    assert_eq!(rest["format_check"]["ok"], true, "{rest}");
    let saved = std::fs::read_to_string(&s.project.output).unwrap();
    assert!(saved.contains("호출이 배치 끝으로 정렬된다."), "{saved}");
    assert!(
        saved.contains("값이 있으면 그것을 쓰고 다시 있으면 그것을 쓴다."),
        "{saved}"
    );
    assert!(saved.contains("```text\n배치末尾로\n```"), "{saved}");
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
}

// The format check reads a lone CR as a line break, as CommonMark does; the
// fix_id it gives a word below one must still find that word.
#[test]
fn a_garbled_word_below_a_lone_carriage_return_is_fixed_by_its_fix_id() {
    let (dir, mut s) = setup();
    let output = dir.path().join("guide.md");
    std::fs::write(&output, "# 가이드\r\r도구 워커 인프รา 설명.\n").unwrap();
    let audit = run(&mut s, "document_audit", json!({}));
    let issue = audit["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|issue| issue["kind"] == "mixed_script_word")
        .unwrap_or_else(|| panic!("{audit}"))
        .clone();
    let fixed = run(
        &mut s,
        "document_edit",
        json!({"action":"fix_word","fix_id":issue["fix_id"],"text":"인프라","expected_hash":audit["hash"]}),
    );
    assert_eq!(fixed["format_check"]["ok"], true, "{fixed}");
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "# 가이드\r\r도구 워커 인프라 설명.\n"
    );
}

// Live run 2026-10-11: a correction turned Option\<Checkpoint\> into raw
// Option<Checkpoint>, which Markdown reads as an HTML tag the page drops.
#[test]
fn a_type_written_as_raw_html_blocks_the_final_until_it_is_code() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    run(&mut s, "file_read", json!({"path":"source.rs"}));
    let doc = "# Guide\n\nThe field is Option<Checkpoint>. source.rs:1\n\n\
Lengths are Vec<u64> and close with </Checkpoint>.\n\n\
| A | B |\n|---|---|\n| one<br>two | `Option<Checkpoint>` |\n\n\
Escaped Option\\<Checkpoint\\> and <!-- note --> and <https://example.com> and <BR> pass.\n\n\
```rust\nlet cp: Option<Checkpoint> = None;\n```\n";
    let written = create(&mut s, doc);
    let format = &written["format_check"];
    assert_eq!(format["ok"], false, "{format}");
    let found: Vec<_> = format["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| {
            assert_eq!(issue["kind"], "raw_html_tag", "{issue}");
            (
                issue["line"].as_u64().unwrap(),
                issue["tag"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![(3, "<Checkpoint>"), (5, "<u64>"), (5, "</Checkpoint>")],
        "{format}"
    );
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], false, "{audit}");
    let hash = written["hash"].as_str().unwrap();
    let fixed = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[
            {"action":"replace_text","old_text":"Option<Checkpoint>.","text":"`Option<Checkpoint>`."},
            {"action":"replace_text","old_text":"Vec<u64> and close with </Checkpoint>","text":"`Vec<u64>` and close with `</Checkpoint>`"}]}),
    );
    assert_eq!(fixed["format_check"]["ok"], true, "{fixed}");
}

// A garbled line that also begins another line is anchored with its
// neighbours, joined by the line breaks the document has between them: lone
// CRs, or LF and CRLF mixed. Joined by one break for the whole document,
// the anchor was not in the document and the fix was refused.
#[test]
fn a_widened_fix_word_anchor_keeps_the_documents_own_line_breaks() {
    for doc in [
        "# 가이드\r\r인프รา 설명\r\r인프รา 설명 추가\r",
        "# 가이드\r\n\r\n인프รา 설명\n\n인프รา 설명 추가\r\n",
    ] {
        let (dir, mut s) = setup();
        let output = dir.path().join("guide.md");
        std::fs::write(&output, doc).unwrap();
        let audit = run(&mut s, "document_audit", json!({}));
        let issue = audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|issue| issue["kind"] == "mixed_script_word")
            .unwrap_or_else(|| panic!("{audit}"))
            .clone();
        assert_eq!(issue["line"], 3, "{issue}");
        run(
            &mut s,
            "document_edit",
            json!({"action":"fix_word","fix_id":issue["fix_id"],"text":"인프라","expected_hash":audit["hash"]}),
        );
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            doc.replacen("인프รา 설명", "인프라 설명", 1),
            "{doc:?}"
        );
    }
}

// A document saved by a Windows editor breaks its lines with CRLF; models
// send old_text with LF breaks. A passage of several lines was never found
// there. Spelled with CRLF it matches, and the new text keeps CRLF breaks.
#[test]
fn old_text_with_lf_breaks_matches_a_crlf_document() {
    let (dir, mut s) = setup();
    let output = dir.path().join("guide.md");
    let doc = "# T\r\n\r\n## A\r\n\r\na text\r\nmore a\r\n\r\n## B\r\n\r\na text\r\nmore b\r\n";
    std::fs::write(&output, doc).unwrap();
    let hash = tools::hash(doc.as_bytes());
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"a text\nmore a","text":"new a\nsecond line","expected_hash":hash}),
    );
    let saved = std::fs::read_to_string(&output).unwrap();
    assert_eq!(
        saved,
        doc.replace("a text\r\nmore a", "new a\r\nsecond line")
    );
    // In a batch too; a passage that occurs twice is still refused.
    let saved_hash = tools::hash(saved.as_bytes());
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":saved_hash,"edits":[
            {"action":"delete_text","old_text":"a text\nmore b\n"},
            {"action":"insert_after_text","old_text":"## B\n","text":"\nadded\n"}
        ]}),
    );
    let batch = std::fs::read_to_string(&output).unwrap();
    assert_eq!(
        batch,
        "# T\r\n\r\n## A\r\n\r\nnew a\r\nsecond line\r\n\r\n## B\r\n\r\nadded\r\n\r\n"
    );
    std::fs::write(&output, doc).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"\na text\n","text":"x","expected_hash":hash}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("occurs 2 times"), "{error}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), doc);
}

// Text inserted into a document whose lines all end in CRLF was saved with
// the LF breaks the model sent: "second\n\r\n## B" was left behind, and an
// old_text across it matched with neither break. The document keeps CRLF
// breaks; a whole-document write keeps the breaks it was sent with.
#[test]
fn edits_keep_a_crlf_document_crlf() {
    let (dir, mut s) = setup();
    let output = dir.path().join("guide.md");
    let doc = "# T\r\n\r\n## A\r\n\r\na text\r\n\r\n## B\r\n\r\nb text\r\n";
    std::fs::write(&output, doc).unwrap();
    let inserted = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"## A","text":"### A1\n\nchild text\nsecond\n","expected_hash":tools::hash(doc.as_bytes())}),
    );
    let saved = std::fs::read_to_string(&output).unwrap();
    assert_eq!(
        saved,
        "# T\r\n\r\n## A\r\n\r\na text\r\n\r\n### A1\r\n\r\nchild text\r\nsecond\r\n\r\n## B\r\n\r\nb text\r\n"
    );
    assert_eq!(inserted["hash"], tools::hash(saved.as_bytes()));
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"second\n\n## B","text":"second line\n\n## B"}),
    );
    assert!(
        std::fs::read_to_string(&output)
            .unwrap()
            .contains("second line\r\n\r\n## B\r\n")
    );
    let current = std::fs::read_to_string(&output).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"write","text":"# T\n\nnew\n","expected_hash":tools::hash(current.as_bytes())}),
    );
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "# T\n\nnew\n");
}
