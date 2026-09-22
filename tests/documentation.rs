use mnemoarc::{
    config::{Config, Project},
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::{Value, json};
fn setup() -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(
        Project {
            root: dir.path().into(),
            output: dir.path().join("summary.md"),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            model_context: Some(128000),
            ..Default::default()
        },
    );
    s.active_tools = ToolRegistry::optional_names();
    (dir, s)
}
fn run(s: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(s, name, args).unwrap()
}

#[test]
fn new_sections_can_be_inserted_in_outline_order() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Overview\nStart.\n## Errors\nFailures.\n## Appendix\nExtra.\n"}),
    );
    let inserted = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_before","section":"## Errors","expected_hash":created["hash"],"text":"## Flow\nSteps."}),
    );
    let batched = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":inserted["hash"],"edits":[{"action":"insert_before","section":"## Appendix","text":"## Limits\nBounds."}]}),
    );
    let expected = "# Guide\n## Overview\nStart.\n## Flow\nSteps.\n## Errors\nFailures.\n## Limits\nBounds.\n## Appendix\nExtra.\n";
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected
    );
    assert_eq!(batched["hash"], tools::hash(expected.as_bytes()));

    for text in [
        "### Wrong level\nBody.",
        "## Flow\nDuplicate.",
        "## One\n## Two\n",
    ] {
        assert!(tools::execute(&mut s, "document_edit", json!({"action":"insert_before","section":"## Errors","expected_hash":batched["hash"],"text":text})).is_err());
    }
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected
    );
}

#[test]
fn inserting_after_last_child_stays_inside_its_parent() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Part\n### First\nOne.\n## Next\nLater.\n"}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"### First","expected_hash":created["hash"],"text":"### Second\nTwo."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## Part\n### First\nOne.\n### Second\nTwo.\n## Next\nLater.\n"
    );
}

#[test]
fn section_insertion_uses_the_local_line_ending_in_mixed_documents() {
    let original = "# Doc\r\n## CRLF\r\nA\r\n## LF\nB\n## Tail\nC\n";
    let expected = "# Doc\r\n## CRLF\r\nA\r\n## LF\nB\n## New\nN\n## Tail\nC\n";
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, original).unwrap();
        let edit = json!({"action":"insert_after","section":"## LF","text":"## New\nN"});
        if batch {
            run(
                &mut s,
                "document_edit_batch",
                json!({"expected_hash":tools::hash(original.as_bytes()),"edits":[edit]}),
            );
        } else {
            let mut edit = edit;
            edit["expected_hash"] = json!(tools::hash(original.as_bytes()));
            run(&mut s, "document_edit", edit);
        }
        assert_eq!(
            std::fs::read_to_string(&s.project.output).unwrap(),
            expected
        );
    }
}

#[test]
fn nested_outline_paths_select_repeated_titles_and_scope_text_edits() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Alpha\n### Shared\ncommon\n## Beta\n### Shared\ncommon\n"}),
    );
    let alpha = "# Guide\n## Alpha\n### Shared";
    let beta = "# Guide\n## Beta\n### Shared";
    let outline = run(&mut s, "document_inspect", json!({}));
    assert_eq!(outline["outline"][3]["section_path"], "# Guide\n## Beta");
    assert_eq!(outline["outline"][4]["section_path"], beta);
    assert_eq!(outline["outline"][4]["level"], 3);
    assert_eq!(
        run(&mut s, "document_inspect", json!({"section":alpha}))["start_line"],
        3
    );
    let beta_page = run(&mut s, "document_inspect", json!({"section":beta}));
    assert_eq!(beta_page["start_line"], 6);
    assert_eq!(beta_page["section_path"], beta);
    let changed = run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","section":beta,"old_text":"common","text":"beta detail","expected_hash":created["hash"]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## Alpha\n### Shared\ncommon\n## Beta\n### Shared\nbeta detail\n"
    );
    let beta_page = run(&mut s, "document_inspect", json!({"section":beta}));
    let changed = run(
        &mut s,
        "document_edit",
        json!({"action":"section","section":beta,"expected_hash":changed["hash"],"expected_section_hash":beta_page["section_hash"],"text":"### Shared\nbeta detail\nand more\n"}),
    );
    let registered = run(
        &mut s,
        "investigation",
        json!({"action":"upsert","title":"Beta detail","status":"written","section":beta}),
    );
    assert_eq!(registered["section"], beta);
    assert_eq!(
        changed["hash"],
        tools::hash(std::fs::read(&s.project.output).unwrap().as_slice())
    );
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":changed["hash"],"edits":[{"action":"delete_text","section":alpha,"old_text":"common"}]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## Alpha\n### Shared\n\n## Beta\n### Shared\nbeta detail\nand more\n"
    );
}

#[test]
fn child_insertions_handle_first_last_empty_and_repeated_leaf_titles() {
    let (_dir, mut s) = setup();
    for name in ["document_edit", "document_edit_batch"] {
        let spec = ToolRegistry::definitions(&s)
            .into_iter()
            .find(|spec| spec["function"]["name"] == name)
            .unwrap();
        let schema = spec["function"]["parameters"].to_string();
        assert!(schema.contains("insert_first_child"));
        assert!(schema.contains("insert_last_child"));
    }
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Alpha\nIntro.\n### Existing\nBody.\n## Beta\nBeta intro.\n### Existing\nBeta body.\n## Empty\nEmpty intro.\n"}),
    );
    let alpha = "# Guide\n## Alpha";
    let beta = "# Guide\n## Beta";
    let empty = "# Guide\n## Empty";
    let first = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_first_child","section":alpha,"expected_hash":created["hash"],"text":"### First\nFirst body."}),
    );
    let last = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":beta,"expected_hash":first["hash"],"text":"### Last\nLast body."}),
    );
    let final_edit = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":last["hash"],"edits":[
            {"action":"insert_first_child","section":empty,"text":"### Only\nOnly body."},
            {"action":"insert_after","section":"# Guide\n## Beta\n### Existing","text":"### First\nBeta first."}
        ]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## Alpha\nIntro.\n### First\nFirst body.\n### Existing\nBody.\n## Beta\nBeta intro.\n### Existing\nBeta body.\n### First\nBeta first.\n### Last\nLast body.\n## Empty\nEmpty intro.\n### Only\nOnly body.\n"
    );
    let before = std::fs::read_to_string(&s.project.output).unwrap();
    for text in [
        "## Wrong\nBody.",
        "### First\nDuplicate.",
        "### One\n### Two\n",
    ] {
        assert!(tools::execute(&mut s, "document_edit", json!({"action":"insert_last_child","section":alpha,"expected_hash":final_edit["hash"],"text":text})).is_err());
    }
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), before);
}

#[test]
fn first_child_does_not_adopt_headings_that_skip_a_level() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Parent\nIntro.\n#### Existing\nOld.\n## Next\nLater.\n"}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_first_child","section":"## Parent","expected_hash":created["hash"],"text":"### New\nNew body."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## Parent\nIntro.\n#### Existing\nOld.\n### New\nNew body.\n## Next\nLater.\n"
    );
}

#[test]
fn partial_text_edits_insert_replace_and_delete_without_rewriting_sections() {
    let (_dir, mut s) = setup();
    let definitions = ToolRegistry::definitions(&s);
    for name in ["document_edit", "document_edit_batch"] {
        let spec = definitions
            .iter()
            .find(|spec| spec["function"]["name"] == name)
            .unwrap();
        let schema = spec["function"]["parameters"].to_string();
        for action in [
            "replace_text",
            "delete_text",
            "insert_before_text",
            "insert_after_text",
        ] {
            assert!(schema.contains(action), "{name} does not expose {action}");
        }
    }
    let mut result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Notes\nfirst line\nlast line\n"}),
    );
    for edit in [
        json!({"action":"insert_before_text","old_text":"last line","text":"middle line\n"}),
        json!({"action":"insert_after_text","old_text":"first line\n","text":"detail line\n"}),
        json!({"action":"replace_text","old_text":"middle line","text":"revised line"}),
        json!({"action":"delete_text","old_text":"detail line\n"}),
    ] {
        let mut edit = edit;
        edit["expected_hash"] = result["hash"].clone();
        result = run(&mut s, "document_edit", edit);
    }
    let expected = "# Notes\nfirst line\nrevised line\nlast line\n";
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected
    );
    assert_eq!(result["hash"], tools::hash(expected.as_bytes()));
    assert!(
        tools::execute(
            &mut s,
            "document_edit",
            json!({"action":"delete_text","expected_hash":result["hash"],"old_text":"line"})
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected
    );
}

#[test]
fn batch_partial_text_edits_are_ordered_and_atomic() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"start\nold\nend\n"}),
    );
    let failed = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":created["hash"],"edits":[
            {"action":"insert_after_text","old_text":"start\n","text":"new\n"},
            {"action":"delete_text","old_text":"missing\n"}
        ]}),
    );
    assert!(failed.is_err());
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "start\nold\nend\n"
    );
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":created["hash"],"edits":[
            {"action":"insert_after_text","old_text":"start\n","text":"new\n"},
            {"action":"delete_text","old_text":"old\n"},
            {"action":"replace_text","old_text":"end","text":"finish"}
        ]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "start\nnew\nfinish\n"
    );
}

#[test]
fn investigation_updates_preserve_title_but_new_items_still_require_it() {
    let (_dir, mut s) = setup();
    std::fs::write(
        &s.project.output,
        "# Updated entry\nbody\n# New section\nbody\n",
    )
    .unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"Entry point","section":"# Entry"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","status":"written","section":"# Updated entry"}),
    );
    assert_eq!(s.investigations.len(), 1);
    assert_eq!(s.investigations[0].title, "Entry point");
    assert_eq!(s.investigations[0].status, "written");
    assert_eq!(s.investigations[0].section, "# Updated entry");
    let before = serde_json::to_value(&s.investigations).unwrap();
    for args in [
        json!({"action":"upsert","id":"unknown","status":"written"}),
        json!({"action":"upsert","status":"written"}),
        json!({"action":"upsert","id":"entry","title":"  "}),
        json!({"action":"upsert","id":"entry","title":null}),
    ] {
        assert!(tools::execute(&mut s, "investigation", args).is_err());
        assert_eq!(serde_json::to_value(&s.investigations).unwrap(), before);
    }
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"Renamed"}),
    );
    assert_eq!(s.investigations[0].title, "Renamed");
    // Editing a verified item must still invalidate its prior verification.
    s.investigations[0].status = "verified".into();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","section":"# New section"}),
    );
    assert_eq!(s.investigations[0].status, "written");
    assert!(s.investigations[0].document_hash.is_none());
}
#[test]
fn outline_ignores_code_fences_and_section_edits_are_conflict_checked() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# 개요\nintro\n## 흐름\n```md\n## 가짜\n```\nbody\n## 검증\npending\n"}),
    );
    let outline = run(&mut s, "document_inspect", json!({}));
    assert_eq!(outline["total_lines"], 9);
    assert_eq!(outline["outline"].as_array().unwrap().len(), 3);
    let section = run(&mut s, "document_inspect", json!({"section":"## 흐름"}));
    assert!(
        section["content"]["text"]
            .as_str()
            .unwrap()
            .contains("## 가짜")
    );
    let args = json!({"action":"section","section":"## 흐름","expected_hash":outline["hash"],"expected_section_hash":section["section_hash"],"text":"## 흐름\n수정된 설명\n"});
    run(&mut s, "document_edit", args.clone());
    assert!(
        tools::execute(&mut s, "document_edit", args)
            .unwrap_err()
            .to_string()
            .contains("conflict")
    );
    let current = run(&mut s, "document_inspect", json!({"section":"## 검증"}));
    assert_eq!(current["content"]["text"], "## 검증\npending\n");
}
#[test]
fn exact_line_map_and_repeat_suppression_allow_forced_verification_and_changes() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "// 한글\nfn main() {}\n// 끝\n").unwrap();
    let args = json!({"path":"main.rs","start_line":2,"max_lines":2});
    for _ in 0..2 {
        let r = run(&mut s, "file_read", args.clone());
        assert_eq!(r["total_lines"], 3);
        assert_eq!(r["content"]["line_start"], 2);
        assert_eq!(r["content"]["line_offsets"], json!([0, 13]));
        s.history.push(
            vec![json!({"role":"tool","content":tools::envelope(Ok(r)).to_string()})],
            true,
        );
    }
    assert_eq!(run(&mut s, "file_read", args.clone())["suppressed"], true);
    assert!(
        run(
            &mut s,
            "file_read",
            json!({"path":"main.rs","start_line":2,"max_lines":2,"force_read":true})
        )["content"]
            .is_object()
    );
    std::fs::write(
        dir.path().join("main.rs"),
        "// changed\nfn main() { go(); }\n",
    )
    .unwrap();
    assert!(run(&mut s, "file_read", args)["content"].is_object());
}
#[test]
fn symbols_page_and_expire_on_source_change() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("main.ts"),
        "export async function load() {}\nexport class Store {}\nconst value = 1;\n",
    )
    .unwrap();
    let p = run(&mut s, "symbol_search", json!({"limit":1}));
    assert_eq!(p["symbols"][0]["name"], "load");
    assert_eq!(
        run(
            &mut s,
            "symbol_search",
            json!({"limit":1,"cursor":p["next_cursor"]})
        )["symbols"][0]["name"],
        "Store"
    );
    std::fs::write(dir.path().join("main.ts"), "export function other() {}\n").unwrap();
    assert!(tools::execute(&mut s, "symbol_search", json!({"cursor":p["next_cursor"]})).is_err());
}
#[test]
fn batch_verification_reports_each_failure_and_source_changes_invalidate_success() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let source = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n# Errors\nmain.rs:999\n"}),
    );
    for (id, section) in [("entry", "# Entry"), ("errors", "# Errors")] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":id,"status":"written","section":section,"source_ids":[source]}),
        );
    }
    for _ in 0..5 {
        assert_eq!(
            run(&mut s, "investigation", json!({"action":"final_check"}))["complete"],
            false
        );
    }
    assert_eq!(s.reviews, 0);
    let r = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{"entry":{"source_ids":[source],"verification_note":"Compared main declaration with entry."},"errors":{"source_ids":[],"verification_note":"not read"}}}),
    );
    assert_eq!(r["results"][0]["result"]["status"], "ok");
    assert_eq!(r["results"][1]["result"]["status"], "error");
    let retry = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{"entry":{"source_ids":[source],"verification_note":"Retry after partial failure."}}}),
    );
    assert_eq!(retry["results"][0]["result"]["status"], "ok");
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["citations_checked"], 2);
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "citation_range")
    );
    std::fs::write(dir.path().join("main.rs"), "fn changed() {}\n").unwrap();
    let audit = run(&mut s, "document_audit", json!({}));
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "stale_source")
    );
    assert_eq!(s.investigations[0].status, "written");
}
#[test]
fn documentation_tools_respect_permissions_and_reserve() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("summary.md"),
        "# Title\n../../private.rs:1\n",
    )
    .unwrap();
    let audit = run(&mut s, "document_audit", json!({}));
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "citation_path")
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","title":"Title","section":"# Title"}),
    );
    s.run_guidance = json!({"phase":"verify"});
    assert!(
        tools::execute(&mut s, "file_list", json!({}))
            .unwrap_err()
            .to_string()
            .contains("verification_reserve")
    );
    s.active_tools.remove("document_inspect");
    assert!(tools::execute(&mut s, "document_inspect", json!({})).is_err());
}

#[test]
fn long_korean_section_survives_result_limiting_and_resumes_exactly() {
    let (_dir, mut s) = setup();
    let body = format!(
        "# 긴 문서\n{}",
        "한글 근거 문장과 코드 foo_bar.\n".repeat(500)
    );
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":body}),
    );
    let mut offset = 0;
    let mut reconstructed = String::new();
    for i in 0..200 {
        let call = mnemoarc::llm::ToolCall {
            id: format!("section-{i}"),
            name: "document_inspect".into(),
            arguments:
                json!({"section":"# 긴 문서","offset":offset,"expected_hash":created["hash"]})
                    .to_string(),
        };
        let result = tools::run_call(&mut s, &call);
        let result = tools::limit_result(&mut s, &call, result, 500);
        let text = result["data"]["content"]["text"].as_str().unwrap();
        assert!(!text.is_empty());
        reconstructed.push_str(text);
        if result["data"]["content"]["truncated"] != true {
            break;
        }
        let next = result["data"]["content"]["next_offset"].as_u64().unwrap();
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(reconstructed, body);
}

#[test]
fn section_lookup_trims_heading_and_rejects_changes_between_pages() {
    let (_dir, mut s) = setup();
    let first = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# 개요\n처음\n"}),
    );
    let page = run(
        &mut s,
        "document_inspect",
        json!({"section":"  # 개요  ","expected_hash":first["hash"]}),
    );
    assert_eq!(page["content"]["text"], "# 개요\n처음\n");
    run(
        &mut s,
        "document_edit",
        json!({"action":"append","text":"추가\n","expected_hash":first["hash"]}),
    );
    assert!(
        tools::execute(
            &mut s,
            "document_inspect",
            json!({"section":"# 개요","offset":2,"expected_hash":first["hash"]})
        )
        .unwrap_err()
        .to_string()
        .contains("document_revision_conflict")
    );
}

#[test]
fn audit_ignores_example_citations_inside_fenced_code() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n```text\nexample.rs:999\n```\n"}),
    );
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["citations_checked"], 1);
    assert!(
        !audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "citation_path")
    );
}

#[test]
fn final_check_rejects_invalid_citations_even_after_agent_attestation() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let id = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:99\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","status":"written","section":"# Entry","source_ids":[id]}),
    );
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[id],"verification_note":"Agent claims comparison."}),
    ).unwrap_err();
    assert!(error.to_string().starts_with("source_coverage_missing:"));
    // Even an injected legacy attestation cannot bypass final structural audit.
    s.investigations[0].status = "verified".into();
    s.investigations[0].document_hash = Some(tools::hash(b"# Entry\nmain.rs:99\n"));
    assert_eq!(
        run(&mut s, "investigation", json!({"action":"final_check"}))["complete"],
        false
    );
    assert_eq!(s.reviews, 0);
}

#[test]
fn out_of_range_read_is_empty_and_cannot_supply_verification_evidence() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "\nfn main() {}\n").unwrap();
    let eof = run(
        &mut s,
        "file_read",
        json!({"path":"main.rs","start_line":99}),
    );
    assert_eq!(eof["source"], Value::Null);
    assert_eq!(eof["eof"], true);
    assert_eq!(eof["total_lines"], 2);
    assert!(
        tools::execute(
            &mut s,
            "file_read",
            json!({"path":"main.rs","start_line":1,"offset":50})
        )
        .is_err()
    );
    let blank = run(
        &mut s,
        "file_read",
        json!({"path":"main.rs","start_line":1,"max_lines":1}),
    );
    let id = blank["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:2\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","section":"# Entry","status":"written"}),
    );
    assert!(
        tools::execute(
            &mut s,
            "investigation",
            json!({"action":"verify","id":"entry","source_ids":[id],"verification_note":"blank"})
        )
        .unwrap_err()
        .to_string()
        .contains("non-empty")
    );
}

#[test]
fn settings_validate_reserves_and_patch_keeps_existing_references() {
    let (dir, mut s) = setup();
    std::fs::write(&s.project.output, "# Entry\nbody\n").unwrap();
    s.config.writing_reserve_ratio = 0.2;
    assert!(s.config.validate().is_err());
    s.config.writing_reserve_ratio = 0.5;
    assert!(s.config.validate().is_ok());
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let id = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","section":"# Entry","source_ids":[id]}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","status":"written"}),
    );
    assert_eq!(s.investigations[0].sources.len(), 1);
    assert_eq!(s.investigations[0].section, "# Entry");
    assert!(
        tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","title":"entry"})
        )
        .is_err()
    );
}

#[test]
fn bare_section_title_reads_and_edits_unique_heading_without_renaming_it() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# 문서\n## 1. 시스템 개요\n처음\n```md\n## 1. 시스템 개요\n```\n## 다른 제목\n나머지\n"}),
    );
    let page = run(
        &mut s,
        "document_inspect",
        json!({"section":" 1. 시스템 개요 "}),
    );
    assert_eq!(page["section"], "## 1. 시스템 개요");
    assert_eq!(page["start_line"], 2);
    assert!(
        page["content"]["text"]
            .as_str()
            .unwrap()
            .starts_with("## 1. 시스템 개요\n처음")
    );
    let modified = run(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"1. 시스템 개요","expected_hash":page["hash"],"expected_section_hash":page["section_hash"],"text":"## 1. 시스템 개요\n수정\n"}),
    );
    let exact = run(
        &mut s,
        "document_inspect",
        json!({"section":"## 1. 시스템 개요"}),
    );
    assert_eq!(exact["hash"], modified["hash"]);
    assert_eq!(exact["content"]["text"], "## 1. 시스템 개요\n수정\n");
    assert!(tools::execute(&mut s, "document_edit", json!({"action":"section","section":"1. 시스템 개요","expected_hash":exact["hash"],"expected_section_hash":exact["section_hash"],"text":"1. 시스템 개요\n제목 기호 제거\n"})).unwrap_err().to_string().contains("retain its heading"));
}

#[test]
fn tab_separated_markdown_headings_can_be_inspected_and_edited() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n##\tPart ###\nBody.\n## Next\nLater.\n"}),
    );
    let inspected = run(&mut s, "document_inspect", json!({"section":"Part"}));
    assert_eq!(inspected["section"], "##\tPart ###");
    let edited = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"Part","expected_hash":created["hash"],"text":"### Child\nDetail."}),
    );
    let expected = "# Guide\n##\tPart ###\nBody.\n### Child\nDetail.\n## Next\nLater.\n";
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected
    );
    assert_eq!(edited["hash"], tools::hash(expected.as_bytes()));
}

#[test]
fn section_replacement_cannot_insert_peer_or_ancestor_headings() {
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        let original = "# Guide\n## Part\nOriginal.\n## Next\nLater.\n";
        let created = run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":original}),
        );
        let inspected = run(&mut s, "document_inspect", json!({"section":"## Part"}));
        for heading in ["## Sibling", "# Ancestor"] {
            let edit = json!({"action":"section","section":"## Part","expected_section_hash":inspected["section_hash"],"text":format!("## Part\nUpdated.\n{heading}\nUnexpected.\n")});
            let (name, args) = if batch {
                (
                    "document_edit_batch",
                    json!({"expected_hash":created["hash"],"edits":[edit]}),
                )
            } else {
                let mut edit = edit;
                edit["expected_hash"] = created["hash"].clone();
                ("document_edit", edit)
            };
            let error = tools::execute(&mut s, name, args).unwrap_err().to_string();
            assert!(error.contains("sibling or ancestor heading"), "{error}");
            assert_eq!(
                std::fs::read_to_string(&s.project.output).unwrap(),
                original
            );
        }
    }
}

#[test]
fn bare_section_title_never_guesses_between_duplicate_titles() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# 문서\n## 개요\n상위\n### 개요\n하위\n## 반복\n하나\n## 반복\n둘\n"}),
    );
    let ambiguous = tools::execute(&mut s, "document_inspect", json!({"section":"개요"}))
        .unwrap_err()
        .to_string();
    assert!(
        ambiguous.contains("ambiguous_section")
            && ambiguous.contains("## 개요")
            && ambiguous.contains("### 개요")
    );
    let explicit = run(&mut s, "document_inspect", json!({"section":"### 개요"}));
    assert_eq!(explicit["start_line"], 4);
    for section in ["반복", "## 반복"] {
        assert!(
            tools::execute(&mut s, "document_inspect", json!({"section":section}))
                .unwrap_err()
                .to_string()
                .contains("ambiguous_section")
        );
    }
    let missing = tools::execute(&mut s, "document_inspect", json!({"section":"없는 제목"}))
        .unwrap_err()
        .to_string();
    assert!(missing.contains("section_not_found") && missing.contains("document_inspect"));
}

fn deliver(s: &mut Session, name: &str, args: Value, budget: usize) -> Value {
    let call = mnemoarc::llm::ToolCall {
        id: mnemoarc::memory::id(),
        name: name.into(),
        arguments: args.to_string(),
    };
    let result = tools::run_call(s, &call);
    let result = tools::limit_result(s, &call, result, budget);
    tools::record_delivered_read(s, &call, &result);
    result
}

#[test]
fn nested_outline_stays_pageable_when_result_budget_is_small() {
    let (_dir, mut s) = setup();
    let mut doc = String::from("# Guide\n");
    for index in 0..35 {
        doc.push_str(&format!(
            "## Section {index} with a descriptive heading\n### Detail {index}\nBody.\n"
        ));
    }
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":doc}),
    );
    let page = deliver(&mut s, "document_inspect", json!({"limit":100}), 1200);
    let outline = page["data"]["outline"]
        .as_array()
        .expect("bounded outline page");
    assert!(!outline.is_empty());
    assert!(outline.len() < 100);
    assert_eq!(page["data"]["next_offset"], outline.len());
    assert_eq!(page["next_cursor"]["tool"], "document_inspect");
    assert_eq!(page["next_cursor"]["offset"], outline.len());
    let mut next_args = page["next_cursor"].clone();
    next_args.as_object_mut().unwrap().remove("tool");
    let next = deliver(&mut s, "document_inspect", next_args, 1200);
    assert!(
        next["data"]["outline"][0]["start_line"].as_u64().unwrap()
            > outline.last().unwrap()["start_line"].as_u64().unwrap()
    );
}

#[test]
fn input_document_path_and_coverage_respect_actual_delivery_and_revision() {
    let (dir, mut s) = setup();
    let path = dir.path().join("input.md");
    std::fs::write(&path, "# 제목\n소개\n## 내용\n본문\n").unwrap();
    let outline = deliver(&mut s, "document_inspect", json!({"path":"input.md"}), 4000);
    assert_eq!(outline["data"]["outline"].as_array().unwrap().len(), 2);
    assert!(s.read_coverage.is_empty());
    // Execution alone is not delivery; it can still be truncated by the batch.
    run(&mut s, "file_read", json!({"path":"input.md"}));
    assert!(s.read_coverage.is_empty());
    deliver(
        &mut s,
        "file_read",
        json!({"path":"input.md","max_lines":2}),
        4000,
    );
    let coverage = run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"].clone();
    assert_eq!(coverage["fully_read_lines"], 2);
    assert_eq!(
        coverage["missing_ranges"],
        json!([{"start_line":3,"end_line":4}])
    );
    deliver(
        &mut s,
        "document_inspect",
        json!({"path":"input.md","section":"내용"}),
        4000,
    );
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":path}))["coverage"]["complete"],
        true
    );
    std::fs::write(&path, "# 변경\n새 내용\n").unwrap();
    let changed = run(&mut s, "document_inspect", json!({"path":"input.md"}));
    assert_eq!(changed["coverage"]["fully_read_lines"], 0);
    assert_eq!(changed["coverage"]["previous_revision_ignored"], true);
    deliver(&mut s, "file_read", json!({"path":"input.md"}), 4000);
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"]["complete"],
        true
    );
}

#[test]
fn input_inspection_enforces_file_read_path_rules() {
    let (dir, mut s) = setup();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("external.md");
    std::fs::write(&external, "# 외부\n").unwrap();
    assert!(tools::execute(&mut s, "document_inspect", json!({"path":external})).is_err());
    std::fs::write(dir.path().join("private.md"), "# 비공개\n").unwrap();
    s.project.exclude = vec!["private.md".into()];
    assert!(tools::execute(&mut s, "document_inspect", json!({"path":"private.md"})).is_err());
    assert!(tools::execute(&mut s, "document_inspect", json!({"path":"."})).is_err());
    s.project.output = external.clone();
    assert_eq!(
        run(&mut s, "document_inspect", json!({}))["path"],
        json!(external)
    );
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":external}))["total_lines"],
        1
    );
    #[cfg(unix)]
    {
        let other = outside.path().join("other.md");
        std::fs::write(&other, "# outside\n").unwrap();
        std::os::unix::fs::symlink(&other, dir.path().join("escape.md")).unwrap();
        assert!(tools::execute(&mut s, "document_inspect", json!({"path":"escape.md"})).is_err());
    }
}

#[test]
fn final_budget_partial_lines_and_archive_only_do_not_overstate_coverage() {
    let (dir, mut s) = setup();
    let body = format!("# 긴 문서\n{}\n끝\n", "아주 긴 한글 문장 ".repeat(800));
    std::fs::write(dir.path().join("input.md"), &body).unwrap();
    deliver(&mut s, "file_read", json!({"path":"input.md"}), 100);
    assert!(s.read_coverage.is_empty());
    let mut page = deliver(&mut s, "file_read", json!({"path":"input.md"}), 700);
    assert_eq!(page["data"]["content"]["last_line_complete"], false);
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"]["fully_read_lines"],
        1
    );
    for _ in 0..200 {
        if page["data"]["content"]["truncated"] != true {
            break;
        }
        page = deliver(
            &mut s,
            "file_read",
            json!({"cursor":page["next_cursor"]["cursor"]}),
            700,
        );
    }
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"]["complete"],
        true
    );
}

#[test]
fn section_cursor_keeps_input_path_and_coverage_merges_crlf_pages() {
    let (dir, mut s) = setup();
    let body = format!("# 문서\r\n{}", "한글🙂\r\n\r\n".repeat(50));
    std::fs::write(dir.path().join("input.md"), &body).unwrap();
    let mut args = json!({"path":"input.md","section":"문서"});
    let mut reconstructed = String::new();
    for _ in 0..200 {
        let page = deliver(&mut s, "document_inspect", args, 500);
        reconstructed.push_str(page["data"]["content"]["text"].as_str().unwrap());
        if page["data"]["content"]["truncated"] != true {
            break;
        }
        args = page["next_cursor"].clone();
        assert_eq!(
            args["path"],
            json!(dir.path().join("input.md").canonicalize().unwrap())
        );
        args.as_object_mut().unwrap().remove("tool");
    }
    assert_eq!(reconstructed, body);
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"]["complete"],
        true
    );
}

#[test]
fn coverage_missing_ranges_page_and_empty_lines_are_counted() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("input.md"), "a\n\nb\nc\nd\n").unwrap();
    for line in [2, 4] {
        deliver(
            &mut s,
            "file_read",
            json!({"path":"input.md","start_line":line,"max_lines":1}),
            4000,
        );
    }
    let first = run(
        &mut s,
        "document_inspect",
        json!({"path":"input.md","limit":1}),
    );
    assert_eq!(first["coverage"]["fully_read_lines"], 2);
    assert_eq!(first["coverage"]["missing_range_count"], 3);
    assert_eq!(first["coverage"]["next_offset"], 1);
    let second = run(
        &mut s,
        "document_inspect",
        json!({"path":"input.md","limit":1,"coverage_offset":1,"expected_hash":first["hash"]}),
    );
    assert_eq!(
        second["coverage"]["missing_ranges"],
        json!([{"start_line":3,"end_line":3}])
    );
}

#[test]
fn crlf_split_before_lf_cannot_count_the_next_line_body() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("input.md"), "# H\r\nX\r\n").unwrap();
    let call = mnemoarc::llm::ToolCall {
        id: "split".into(),
        name: "document_inspect".into(),
        arguments: json!({"path":"input.md","section":"H"}).to_string(),
    };
    let mut result = tools::run_call(&mut s, &call);
    result["data"]["content"]["text"] = json!("# H\r");
    tools::record_delivered_read(&mut s, &call, &result);
    let call = mnemoarc::llm::ToolCall { id: "split-next".into(), arguments:json!({"path":"input.md","section":"H","offset":4,"expected_hash":result["data"]["hash"]}).to_string(), ..call };
    let mut next = tools::run_call(&mut s, &call);
    next["data"]["content"]["text"] = json!("\n");
    tools::record_delivered_read(&mut s, &call, &next);
    let outline = run(&mut s, "document_inspect", json!({"path":"input.md"}));
    assert_eq!(outline["coverage"]["fully_read_lines"], 1);
    assert_eq!(outline["outline"][0]["fully_read"], false);
    tools::record_delivered_read(
        &mut s,
        &call,
        &tools::envelope(Ok(
            json!({"path":dir.path().join("input.md"),"hash":result["data"]["hash"],"section":"# H","read_offset":4,"content":{"text":"\nX\r\n"}}),
        )),
    );
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["outline"][0]["fully_read"],
        true
    );
}

#[test]
fn quoted_section_offsets_survive_relimiting_and_record_the_right_range() {
    let (dir, mut s) = setup();
    let body = format!("# H\n{}\n", "긴 본문 ".repeat(600));
    std::fs::write(dir.path().join("input.md"), &body).unwrap();
    let digest = tools::hash(body.as_bytes());
    let start = 50usize;
    let page = deliver(
        &mut s,
        "document_inspect",
        json!({"path":"input.md","section":"H","offset":start.to_string(),"expected_hash":digest}),
        500,
    );
    let shown = page["data"]["content"]["text"].as_str().unwrap();
    assert_eq!(page["next_cursor"]["offset"], start + shown.chars().count());
    assert_eq!(s.read_coverage.values().next().unwrap().ranges[0].0, start);
}

#[test]
fn truncated_read_at_newline_does_not_mark_the_following_blank_line_read() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("input.md"), "a\n\nb\n").unwrap();
    let call = mnemoarc::llm::ToolCall {
        id: "newline-cut".into(),
        name: "file_read".into(),
        arguments: json!({"path":"input.md"}).to_string(),
    };
    let mut result = tools::run_call(&mut s, &call);
    result["data"]["content"]["text"] = json!("a\n");
    result["data"]["content"]["truncated"] = json!(true);
    tools::record_delivered_read(&mut s, &call, &result);
    let coverage = run(&mut s, "document_inspect", json!({"path":"input.md"}));
    assert_eq!(coverage["coverage"]["fully_read_lines"], 1);
}

#[test]
fn eof_read_with_small_result_budget_does_not_panic() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("input.md"), "a\n").unwrap();
    let page = deliver(
        &mut s,
        "file_read",
        json!({"path":"input.md","start_line":100}),
        100,
    );
    assert_eq!(page["status"], "ok");
    assert!(s.read_coverage.is_empty());
}

#[test]
fn complete_range_including_a_blank_last_line_still_counts_it() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("input.md"), "a\n\nb\n").unwrap();
    deliver(
        &mut s,
        "file_read",
        json!({"path":"input.md","max_lines":2}),
        4000,
    );
    let result = run(&mut s, "document_inspect", json!({"path":"input.md"}));
    assert_eq!(result["coverage"]["fully_read_lines"], 2);
    assert_eq!(
        result["coverage"]["missing_ranges"],
        json!([{"start_line":3,"end_line":3}])
    );
}

#[test]
fn changed_file_between_execution_and_delivery_does_not_gain_coverage() {
    let (dir, mut s) = setup();
    let path = dir.path().join("input.md");
    std::fs::write(&path, "# H\nold\n").unwrap();
    let call = mnemoarc::llm::ToolCall {
        id: "changed-before-delivery".into(),
        name: "document_inspect".into(),
        arguments: json!({"path":"input.md","section":"H"}).to_string(),
    };
    let result = tools::run_call(&mut s, &call);
    std::fs::write(&path, "# H\nnew\n").unwrap();
    tools::record_delivered_read(&mut s, &call, &result);
    assert!(s.read_coverage.is_empty());
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"]["complete"],
        false
    );
}

#[test]
fn persisted_sections_become_written_but_unrelated_evidence_cannot_verify_them() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("other.rs"), "fn other() {}\n").unwrap();
    for (id, section) in [("entry", "# Entry"), ("missing", "# Missing")] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":id,"section":section}),
        );
    }
    let write = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n"}),
    );
    assert_eq!(write["written_items"], json!(["entry"]));
    assert_eq!(s.investigations[0].status, "written");
    assert_eq!(s.investigations[1].status, "uninvestigated");
    let wrong = run(&mut s, "file_read", json!({"path":"other.rs"}))["source"]["id"].clone();
    assert!(tools::execute(&mut s,"investigation",json!({"action":"verify","id":"entry","source_ids":[wrong],"verification_note":"Claims a match"})).unwrap_err().to_string().starts_with("source_coverage_missing:"));
    let right = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[right],"verification_note":"Compared the entry"}),
    );
    assert_eq!(s.investigations[0].status, "verified");
}

#[test]
fn verification_reports_all_gaps_and_one_repair_completes_verification() {
    let (dir, mut s) = setup();
    std::fs::write(
        dir.path().join("a.rs"),
        (1..=12)
            .map(|n| format!("// line {n}\n"))
            .collect::<String>(),
    )
    .unwrap();
    std::fs::write(dir.path().join("b.rs"), "// one\n// two\n// three\n").unwrap();
    // Repeated overlapping citations must not duplicate missing reads.
    std::fs::write(&s.project.output, "# Gaps\na.rs:1-12 a.rs:3-9 b.rs:1-3\n").unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"gaps","title":"Gaps","status":"written","section":"Gaps"}),
    );
    let mut sources = vec![];
    for (start, count) in [(2, 2), (6, 2), (10, 1)] {
        sources.push(
            run(
                &mut s,
                "file_read",
                json!({"path":"a.rs","start_line":start,"max_lines":count}),
            )["source"]["id"]
                .clone(),
        );
    }
    let result = tools::envelope(tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"gaps","source_ids":sources,"verification_note":"Compare both files"}),
    ));
    let expected = json!([
        {"path":"a.rs","start_line":1,"end_line":1},
        {"path":"a.rs","start_line":4,"end_line":5},
        {"path":"a.rs","start_line":8,"end_line":9},
        {"path":"a.rs","start_line":11,"end_line":12},
        {"path":"b.rs","start_line":1,"end_line":3}
    ]);
    assert_eq!(result["data"]["missing_ranges"], expected);
    assert_eq!(result["data"]["missing_range_count"], 5);
    assert_eq!(s.investigations[0].status, "written");
    for gap in expected.as_array().unwrap() {
        sources.push(run(&mut s, "file_read", json!({"path":gap["path"],"start_line":gap["start_line"],"max_lines":gap["end_line"].as_u64().unwrap()-gap["start_line"].as_u64().unwrap()+1}))["source"]["id"].clone());
    }
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"gaps","source_ids":sources,"verification_note":"Compared all cited ranges"}),
    );
    assert_eq!(s.investigations[0].status, "verified");
}

#[test]
fn written_registration_resolves_headings_and_rejects_bad_updates_atomically() {
    let (_dir, mut s) = setup();
    // Planning a future section does not require a document.
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"chat","title":"Chat","status":"in_progress","section":"4. 채팅 흐름 (Chat.jsx)"}),
    );
    std::fs::write(
        &s.project.output,
        "## 4. 채팅 흐름 (Chat.jsx)\nbody\n# Duplicate\none\n## Duplicate\ntwo\n",
    )
    .unwrap();
    let registered = run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"chat","status":"written"}),
    );
    assert_eq!(registered["section"], "## 4. 채팅 흐름 (Chat.jsx)");
    let before = serde_json::to_value(&s.investigations).unwrap();
    for (heading, code) in [
        ("Missing", "section_not_found"),
        ("Duplicate", "ambiguous_section"),
    ] {
        let err = tools::execute(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":"chat","section":heading}),
        )
        .unwrap_err();
        assert!(err.to_string().starts_with(code));
        assert_eq!(serde_json::to_value(&s.investigations).unwrap(), before);
    }
}

#[test]
fn verification_checks_written_prerequisite_before_sources_or_document() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"draft","title":"Draft"}),
    );
    for sources in [json!([]), json!(["unknown-source"])] {
        let err = tools::execute(&mut s, "investigation", json!({"action":"verify","id":"draft","source_ids":sources,"verification_note":"Compare"})).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("item_must_be_written_before_verification:")
        );
    }
}

#[test]
fn batch_item_contract_rejects_extra_fields_without_losing_siblings() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "// source\n").unwrap();
    std::fs::write(&s.project.output, "# A\na.rs:1\n").unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"ready","title":"Ready","section":"A","status":"written"}),
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{
            "extra":{"id":"ready","source_ids":[source],"verification_note":"Compare"},
            "malformed":false,
            "ready":{"source_ids":[source],"verification_note":"Compared source"}
        }}),
    );
    assert_eq!(result["retry_ids"], json!(["extra", "malformed"]));
    assert_eq!(result["succeeded_ids"], json!(["ready"]));
    assert_eq!(result["summary"]["failed"], 2);
    assert_eq!(
        result["summary"]["failures_by_code"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(s.investigations[0].status, "verified");
}

#[test]
fn batch_reuse_still_requires_the_declared_item_fields() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "// source\n").unwrap();
    std::fs::write(&s.project.output, "# A\na.rs:1\n").unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"ready","title":"Ready","section":"A","status":"written"}),
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"ready","source_ids":[source],"verification_note":"Compared source"}),
    );
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{"ready":{}}}),
    );
    assert_eq!(result["reused_ids"], json!([]));
    assert_eq!(result["retry_ids"], json!(["ready"]));
    assert_eq!(
        result["results"][0]["result"]["recovery"]["code"],
        "missing_argument"
    );
}

#[test]
fn local_edit_preserves_unrelated_verification_and_batch_reuses_it() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let source = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    let doc = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nEntry main.rs:1\n# Other\nOther main.rs:1\n"}),
    );
    for (id, section) in [("entry", "# Entry"), ("other", "# Other")] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":id,"status":"written","section":section}),
        );
        run(
            &mut s,
            "investigation",
            json!({"action":"verify","id":id,"source_ids":[source],"verification_note":"Compared source and section"}),
        );
    }
    let original = serde_json::to_value(&s.investigations[0]).unwrap();
    let edit = run(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":doc["hash"],"old_text":"Other main.rs:1","text":"Updated other main.rs:1"}),
    );
    assert_eq!(edit["verification_required_ids"], json!(["other"]));
    assert_eq!(edit["preserved_verified_ids"], json!(["entry"]));
    // A redundant request with missing evidence cannot destroy an unchanged attestation.
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{
            "entry":{"source_ids":["unknown"],"verification_note":"Redundant"},
            "other":{"source_ids":[],"verification_note":"Missing evidence"}
        }}),
    );
    assert_eq!(result["reused_ids"], json!(["entry"]));
    assert_eq!(result["retry_ids"], json!(["other"]));
    assert_eq!(
        serde_json::to_value(&s.investigations[0]).unwrap(),
        original
    );
    // A changed source must invalidate the cache and require real verification.
    std::fs::write(dir.path().join("main.rs"), "fn changed() {}\n").unwrap();
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{
            "entry":{"source_ids":[source],"verification_note":"Stale source"}
        }}),
    );
    assert_eq!(result["reused_ids"], json!([]));
    assert_eq!(result["retry_ids"], json!(["entry"]));
    assert_eq!(s.investigations[0].status, "written");
}

#[test]
fn patch_inside_h2_handles_blockquote_and_two_line_text() {
    let (_dir, mut s) = setup();
    let document = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Root\n## H2 block\nintro\n> blockquote\n# heading\n>\nRemoved_marker\n## Next\nend\n"}),
    );
    let first = run(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":document["hash"],"old_text":"> blockquote\n# heading","text":"> changed\n### heading"}),
    );
    let second = run(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":first["hash"],"old_text":">\nRemoved_marker","text":"kept"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Root\n## H2 block\nintro\n> changed\n### heading\nkept\n## Next\nend\n"
    );
    assert_eq!(second["total_lines"], 8);
}

#[test]
fn multiline_patch_round_trips_file_read_line_endings() {
    for target in [
        ">\nRemoved_marker",
        ">\r\nRemoved_marker",
        "> quote\r\n> # heading",
        "한글🙂\r\n\r\n\t본문  \n끝",
    ] {
        for batch in [false, true] {
            let (_dir, mut s) = setup();
            let body = format!("# Root\r\n## H2\r\n{target}\r\n## Next\r\nend\r\n");
            std::fs::write(&s.project.output, &body).unwrap();
            let read = run(
                &mut s,
                "file_read",
                json!({"path":"summary.md","start_line":3,"max_lines":target.lines().count()}),
            );
            assert_eq!(read["content"]["text"], target);
            // Copy the model-visible text without its line-number labels.
            let shown = tools::model_result(&tools::envelope(Ok(read.clone())));
            let copied: String = shown["data"]["content"]["numbered_text"]
                .as_str()
                .unwrap()
                .split_inclusive('\n')
                .map(|line| line.split_once('|').unwrap().1)
                .collect();
            assert_eq!(copied, target);
            let mut edit = json!({"action":"patch","old_text":copied,"text":"updated\r\n한글"});
            let (name, args) = if batch {
                (
                    "document_edit_batch",
                    json!({"expected_hash":read["hash"],"edits":[edit]}),
                )
            } else {
                edit["expected_hash"] = read["hash"].clone();
                ("document_edit", edit)
            };
            let result = tools::run_call(
                &mut s,
                &mnemoarc::llm::ToolCall {
                    id: "patch-round-trip".into(),
                    name: name.into(),
                    arguments: args.to_string(),
                },
            );
            assert_eq!(result["status"], "ok", "{result}");
            assert_eq!(
                std::fs::read_to_string(&s.project.output).unwrap(),
                body.replacen(target, "updated\r\n한글", 1)
            );
        }
    }
}

#[test]
fn multiline_patch_accepts_unique_whitespace_but_rejects_empty_targets() {
    for target in ["\n\n\n", "\r\n\r\n", " \n\t", ""] {
        for batch in [false, true] {
            let (_dir, mut s) = setup();
            let body = format!("# Root\n## H2\nbefore{target}after\n");
            std::fs::write(&s.project.output, &body).unwrap();
            let mut edit = json!({"action":"patch","old_text":target,"text":"\n"});
            let (name, args) = if batch {
                (
                    "document_edit_batch",
                    json!({"expected_hash":tools::hash(body.as_bytes()),"edits":[edit]}),
                )
            } else {
                edit["expected_hash"] = json!(tools::hash(body.as_bytes()));
                ("document_edit", edit)
            };
            let result = tools::execute(&mut s, name, args);
            let expected = if target.is_empty() {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("must not be empty")
                );
                body
            } else {
                result.unwrap();
                body.replacen(target, "\n", 1)
            };
            assert_eq!(
                std::fs::read_to_string(&s.project.output).unwrap(),
                expected
            );
        }
    }
}

#[test]
fn raw_file_cursor_preserves_mixed_line_endings_and_coverage() {
    let (_dir, mut s) = setup();
    let target = format!("{}끝", "한글🙂\r\n\r\n본문\n".repeat(30));
    let body = format!("# Root\r\n## H2\r\n{target}\r\n## Next\r\nend\r\n");
    std::fs::write(&s.project.output, &body).unwrap();
    let mut args = json!({"path":"summary.md","start_line":3,"max_lines":target.lines().count()});
    let mut reconstructed = String::new();
    let mut pages = 0;
    for _ in 0..100 {
        let page = deliver(&mut s, "file_read", args, 700);
        let text = page["data"]["content"]["text"].as_str().unwrap();
        assert!(!text.is_empty());
        assert_eq!(page["data"]["read_offset"], reconstructed.chars().count());
        reconstructed.push_str(text);
        assert!(target.starts_with(&reconstructed));
        pages += 1;
        if page["data"]["content"]["truncated"] != true {
            break;
        }
        args = json!({"cursor":page["next_cursor"]["cursor"]});
    }
    assert!(pages > 1);
    assert_eq!(reconstructed, target);
    let inspected = run(&mut s, "document_inspect", json!({}));
    assert_eq!(
        inspected["coverage"]["fully_read_lines"],
        target.lines().count()
    );
    assert_eq!(
        inspected["coverage"]["missing_ranges"],
        json!([
            {"start_line":1,"end_line":2},
            {"start_line":target.lines().count()+3,"end_line":target.lines().count()+4}
        ])
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":inspected["hash"],"old_text":reconstructed,"text":"updated"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        body.replacen(&target, "updated", 1)
    );
}

#[test]
fn file_read_split_crlf_does_not_claim_the_next_line() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("input.md"), "# H\r\nX\r\n").unwrap();
    for (offset, text) in [(0, "# H\r"), (4, "\n")] {
        let call = mnemoarc::llm::ToolCall {
            id: format!("split-{offset}"),
            name: "file_read".into(),
            arguments: json!({"path":"input.md","offset":offset}).to_string(),
        };
        let mut result = tools::run_call(&mut s, &call);
        assert!(
            result["data"]["content"]["text"]
                .as_str()
                .unwrap()
                .starts_with(text)
        );
        // Simulate a delivery budget ending between CR and LF, then after LF.
        result["data"]["content"]["text"] = json!(text);
        result["data"]["content"]["truncated"] = json!(true);
        result["data"]["content"]["last_line_complete"] = json!(offset != 0);
        tools::record_delivered_read(&mut s, &call, &result);
        let outline = run(&mut s, "document_inspect", json!({"path":"input.md"}));
        assert_eq!(outline["coverage"]["fully_read_lines"], 1);
        assert_eq!(
            outline["coverage"]["missing_ranges"],
            json!([{"start_line":2,"end_line":2}])
        );
    }
    deliver(
        &mut s,
        "file_read",
        json!({"path":"input.md","offset":5}),
        4000,
    );
    assert_eq!(
        run(&mut s, "document_inspect", json!({"path":"input.md"}))["coverage"]["complete"],
        true
    );
}

#[test]
fn section_edit_preserves_replacement_whitespace_and_line_endings() {
    for batch in [false, true] {
        for (original, replacement, expected) in [
            (
                "# Doc\r\n## Target\r\nOld\r\n\r\n## Next\r\nRest\r\n",
                "## Target\r\nNew  \r\n\r\n",
                "# Doc\r\n## Target\r\nNew  \r\n\r\n## Next\r\nRest\r\n",
            ),
            (
                "# Doc\n## Target\nOld\n## Next\nRest\n",
                "## Target\nNew\n\n",
                "# Doc\n## Target\nNew\n\n## Next\nRest\n",
            ),
            (
                "# Doc\n## Target\nOld",
                "## Target\nNew  ",
                "# Doc\n## Target\nNew  ",
            ),
            (
                "# Doc\r\n## Target\r\nOld\r\n## Next\r\nRest\r\n",
                "## Target\r\nNew",
                "# Doc\r\n## Target\r\nNew\r\n## Next\r\nRest\r\n",
            ),
        ] {
            let (_dir, mut s) = setup();
            std::fs::write(&s.project.output, original).unwrap();
            let target = run(&mut s, "document_inspect", json!({"section":"## Target"}));
            let edit = json!({"action":"section","section":"## Target","expected_section_hash":target["section_hash"],"text":replacement});
            let result = if batch {
                run(
                    &mut s,
                    "document_edit_batch",
                    json!({"expected_hash":target["hash"],"edits":[edit]}),
                )
            } else {
                let mut edit = edit;
                edit["expected_hash"] = target["hash"].clone();
                run(&mut s, "document_edit", edit)
            };
            assert_eq!(
                std::fs::read_to_string(&s.project.output).unwrap(),
                expected
            );
            assert_eq!(result["hash"], tools::hash(expected.as_bytes()));
        }
    }
}

#[test]
fn batch_failure_after_successful_edit_leaves_file_unchanged() {
    let (_dir, mut s) = setup();
    let original = "# Doc\n## Target\nold\n## Next\nrest\n";
    std::fs::write(&s.project.output, original).unwrap();
    let inspected = run(&mut s, "document_inspect", json!({"section":"## Target"}));
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":inspected["hash"],"edits":[
            {"action":"patch","old_text":"old","text":"changed"},
            {"action":"section","section":"## Target","expected_section_hash":inspected["section_hash"],"text":"## Target\nfinal\n"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("index=1") && error.contains("section_revision_conflict"));
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        original
    );
    assert!(!s.document_written);
}

#[test]
fn patch_rejects_overlapping_matches_and_handles_large_unique_text() {
    for (body, target) in [("aaa", "aa"), ("ééé", "éé"), ("ababa", "aba")] {
        for batch in [false, true] {
            let (_dir, mut s) = setup();
            std::fs::write(&s.project.output, body).unwrap();
            let edit = json!({"action":"patch","old_text":target,"text":"x"});
            let error = if batch {
                tools::execute(
                    &mut s,
                    "document_edit_batch",
                    json!({"expected_hash":tools::hash(body.as_bytes()),"edits":[edit]}),
                )
            } else {
                let mut edit = edit;
                edit["expected_hash"] = json!(tools::hash(body.as_bytes()));
                tools::execute(&mut s, "document_edit", edit)
            }
            .unwrap_err()
            .to_string();
            assert!(error.contains("patch_target_must_match_once"));
            assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
        }
    }

    let (_dir, mut s) = setup();
    let target = format!("{}b", "a".repeat(256 * 1024));
    let body = format!("# Doc\n{target}\n");
    std::fs::write(&s.project.output, &body).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":tools::hash(body.as_bytes()),"old_text":target,"text":"updated"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Doc\nupdated\n"
    );
}

#[test]
fn document_edits_reject_outputs_that_cannot_be_read_back() {
    const LIMIT: usize = 16 * 1024 * 1024;
    let original = format!("# H\n{}", "x".repeat(LIMIT - 5));
    assert_eq!(original.len(), LIMIT - 1);
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, &original).unwrap();
    let original_hash = tools::hash(original.as_bytes());

    for (name, args) in [
        (
            "document_edit",
            json!({"action":"append","expected_hash":original_hash,"text":"ab"}),
        ),
        (
            "document_edit_batch",
            json!({"expected_hash":original_hash,"edits":[
                {"action":"append","text":"a"},
                {"action":"append","text":"b"}
            ]}),
        ),
    ] {
        let error = tools::execute(&mut s, name, args).unwrap_err().to_string();
        assert!(error.contains("unsupported_large_file"), "{name}: {error}");
        assert_eq!(
            std::fs::metadata(&s.project.output).unwrap().len(),
            (LIMIT - 1) as u64
        );
        assert_eq!(
            tools::hash(&std::fs::read(&s.project.output).unwrap()),
            original_hash
        );
        assert!(!s.document_written);
    }

    let at_limit = run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":original_hash,"text":"a"}),
    );
    assert_eq!(at_limit["bytes"], LIMIT);
    assert_eq!(
        run(&mut s, "document_inspect", json!({}))["hash"],
        at_limit["hash"]
    );

    let (_dir, mut s) = setup();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"x".repeat(LIMIT + 1)}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("unsupported_large_file"));
    assert!(!s.project.output.exists());
}

#[test]
fn document_edits_reject_embedded_nul_bytes() {
    let (_dir, mut s) = setup();
    let original = "# Doc\nbody\n";
    std::fs::write(&s.project.output, original).unwrap();
    let original_hash = tools::hash(original.as_bytes());
    for (name, args) in [
        (
            "document_edit",
            json!({"action":"append","expected_hash":original_hash,"text":"bad\u{0}text"}),
        ),
        (
            "document_edit_batch",
            json!({"expected_hash":original_hash,"edits":[
                {"action":"append","text":"ok"},
                {"action":"append","text":"bad\u{0}text"}
            ]}),
        ),
    ] {
        let error = tools::execute(&mut s, name, args).unwrap_err().to_string();
        assert!(error.contains("unsupported_binary_file"), "{name}: {error}");
        assert_eq!(
            std::fs::read_to_string(&s.project.output).unwrap(),
            original
        );
        assert!(!s.document_written);
    }

    let (_dir, mut s) = setup();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Doc\nbad\u{0}text"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("unsupported_binary_file"));
    assert!(!s.project.output.exists());
}
