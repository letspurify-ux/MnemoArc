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
    // Lines of a path may mix full headings and bare titles.
    for mixed in ["Guide\n## Beta\nShared", "# Guide\nBeta\n### Shared"] {
        assert_eq!(
            run(&mut s, "document_inspect", json!({"section":mixed}))["start_line"],
            6
        );
    }
    let error = tools::execute(
        &mut s,
        "document_inspect",
        json!({"section":"Guide\n## Gamma"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("section_not_found:"), "{error}");

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
fn verification_recovery_can_locate_and_register_missing_required_coverage() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("helper.rs"), "fn normalize() {}\n").unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"existing","title":"Existing flow","section":"# Flow"}),
    );
    s.run_guidance = json!({"phase":"verify","progress_recovery":{"active":true}});
    let definitions = tools::ToolRegistry::definitions(&s);
    for name in ["file_list", "source_search", "code_outline"] {
        assert!(
            definitions
                .iter()
                .any(|definition| definition["function"]["name"] == name)
        );
    }
    assert!(tools::execute(&mut s, "file_list", json!({})).is_err());
    let found = run(
        &mut s,
        "file_list",
        json!({"mode":"paths","path_glob":"**/helper.rs"}),
    );
    assert!(found.to_string().contains("helper.rs"));
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"missing","title":"Required normalization","section":"# Normalization"}),
    );
    assert_eq!(s.investigations.len(), 2);
    assert!(
        s.investigations
            .iter()
            .all(|item| item.status != "verified")
    );
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
fn verify_drops_non_file_ids_beside_file_evidence() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    s.add_user("Document main.rs".into());
    let user = s
        .sources
        .values()
        .find(|source| source.origin == "user")
        .unwrap()
        .id
        .clone();
    let file = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","status":"written","section":"# Entry"}),
    );
    // Alone, the request's ID is still named and refused.
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[user],"verification_note":"Compared main.rs:1."}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("verification_sources_required:"),
        "{error}"
    );
    assert!(error.contains(&format!("{user} (origin=user)")), "{error}");
    // Beside file evidence it is dropped and reported (the live shape).
    let verified = run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[file, user],"verification_note":"Compared main.rs:1."}),
    );
    assert_eq!(s.investigations[0].status, "verified", "{verified}");
    assert_eq!(verified["ignored_source_ids"], json!([user]));
    assert!(
        s.investigations[0]
            .sources
            .iter()
            .all(|source| source.origin == "file")
    );
}

#[test]
fn verify_batch_explains_fields_sent_as_items_and_paths_sent_as_ids() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","status":"written","section":"# Entry"}),
    );
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{"section":"# Entry","source_ids":["main.rs"],"verification_note":"Compared."}}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("must map each investigation ID"), "{error}");
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":["src/missing.css"],"verification_note":"Compared."}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("unknown_source: src/missing.css is a path"),
        "{error}"
    );
}

#[test]
fn the_output_file_name_alone_names_the_output() {
    let (dir, mut s) = setup();
    // The live shape: the output lives outside the project root.
    let outside = tempfile::tempdir().unwrap();
    s.project.output = outside.path().join("generated.md");
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nText.\n"}),
    );
    let inspected = run(&mut s, "document_inspect", json!({"path":"generated.md"}));
    assert!(inspected.to_string().contains("Guide"), "{inspected}");
    let read = run(&mut s, "file_read", json!({"path":"generated.md"}));
    assert!(read.to_string().contains("Text."), "{read}");
    // A project file with that name still wins.
    std::fs::write(dir.path().join("generated.md"), "project copy\n").unwrap();
    let read = run(&mut s, "file_read", json!({"path":"generated.md"}));
    assert!(read.to_string().contains("project copy"), "{read}");
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
fn empty_atx_heading_remains_addressable_for_section_insertion() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n##\nUntitled body.\n## Next\nLater.\n"}),
    );
    let outline = run(&mut s, "document_inspect", json!({}));
    assert!(
        outline["outline"]
            .as_array()
            .unwrap()
            .iter()
            .any(|heading| heading["heading"] == "##"),
        "{outline}"
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"##","expected_hash":created["hash"],"text":"## Inserted\nNew body."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n##\nUntitled body.\n## Inserted\nNew body.\n## Next\nLater.\n"
    );
}

#[test]
fn html_comment_headings_do_not_enter_the_editable_outline() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n<!--\n```md\n## Template\nDo not edit.\n-->\n## Real\nEditable.\n"}),
    );
    let outline = run(&mut s, "document_inspect", json!({}));
    let headings = outline["outline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|heading| heading["heading"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(headings, ["# Guide", "## Real"]);
    assert!(tools::execute(&mut s, "document_inspect", json!({"section":"## Template"})).is_err());
}

#[test]
fn document_edit_ignores_citation_examples_inside_html_comments() {
    let (_dir, mut s) = setup();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n<!-- Example citation: missing.rs:9999 -->\n<!--\n```md\nmissing.rs:9999\n-->\n## Real\nText.\n"}),
    );
    assert_eq!(result["citation_check"]["citations_checked"], 0, "{result}");
    assert_eq!(result["citation_check"]["issue_count"], 0);
}

#[test]
fn inline_code_comment_marker_does_not_hide_following_citation() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n`<!--`\n## Real\nSee source.rs:1 <!-- missing.rs:9999 --> and source.rs:1.\n"}),
    );
    assert_eq!(result["citation_check"]["citations_checked"], 2, "{result}");
    assert_eq!(result["citation_check"]["issue_count"], 0);
    assert_eq!(
        run(&mut s, "document_inspect", json!({"section":"## Real"}))["section"],
        "## Real"
    );
}

#[test]
fn indented_code_comment_marker_does_not_hide_following_document_content() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n    missing.rs:9999\n    <!--\n## Real\nSee source.rs:1.\n"}),
    );
    assert_eq!(result["citation_check"]["citations_checked"], 1, "{result}");
    assert_eq!(result["citation_check"]["issue_count"], 0);
    assert_eq!(
        run(&mut s, "document_inspect", json!({"section":"## Real"}))["section"],
        "## Real"
    );
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
    // The earlier operation of the same batch is named as the cause.
    assert!(
        error.contains("edits[0] in this same batch already changed it"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        original
    );
    // Outside a batch the message says how to get the current section hash.
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## Target","expected_hash":inspected["hash"],"expected_section_hash":"0".repeat(64),"text":"## Target\nfinal\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("read that section again with document_inspect"),
        "{error}"
    );
    // The live shape: an item ID sent as the section hash.
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## Target","expected_hash":inspected["hash"],"expected_section_hash":"1eeacd1d-fb48-4d6b-b909-2c3c275cebcc","text":"## Target\nfinal\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("is not a section hash"), "{error}");
    assert!(!s.document_written);
}

#[test]
fn patch_names_html_entities_in_old_text() {
    let body = "# Doc\n`onClick={() => ask(x)}` & <b>\n";
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let edit = json!({"action":"insert_after_text","old_text":"() =&gt; ask(x)}` &amp; &lt;b&gt;","text":" more"});
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
        assert!(error.contains("patch_target_must_match_once"), "{error}");
        assert!(
            error.contains("HTML entities (&lt;, &gt;, &amp;)"),
            "{error}"
        );
        assert!(!error.contains("copy it exactly"), "{error}");
        assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    }

    // A document that really contains the entity text still matches as written,
    // and an unrelated miss keeps the generic message.
    let (_dir, mut s) = setup();
    let body = "# Doc\nuse &gt; here\n";
    std::fs::write(&s.project.output, body).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":tools::hash(body.as_bytes()),"old_text":"use &gt; here","text":"done"}),
    );
    let body = std::fs::read_to_string(&s.project.output).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"patch","expected_hash":tools::hash(body.as_bytes()),"old_text":"missing &gt;","text":"x"}),
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("HTML entities"), "{error}");
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

fn numbered_source(lines: usize) -> String {
    (1..=lines)
        .map(|i| format!("let value_{i} = {i};\n"))
        .collect()
}

#[test]
fn verification_reuses_delivered_evidence_when_source_ids_are_lost() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(20)).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# A\nValues are assigned in order. a.rs:1-20\n\n# B\nThe same values again. a.rs:1-20\n"}),
    );
    for (id, section) in [("a", "# A"), ("b", "# B")] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":id,"section":section,"status":"written"}),
        );
    }
    let first = run(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":1,"max_lines":10}),
    )["source"]["id"]
        .clone();
    let second = run(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":11,"max_lines":10}),
    )["source"]["id"]
        .clone();
    // Only one of the two delivered ranges is named: the other is added.
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"a","source_ids":[first],"verification_note":"Compared all twenty assignments with section A"}),
    );
    assert_eq!(result["supplemented_source_ids"], json!([second]));
    let item = s.investigations.iter().find(|i| i.id == "a").unwrap();
    assert_eq!(item.status, "verified");
    assert_eq!(item.sources.len(), 2);
    // A project path stands in for the evidence delivered from that file.
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"b","source_ids":["a.rs"],"verification_note":"Compared all twenty assignments with section B"}),
    );
    assert_eq!(result["verified"], "b");
    assert_eq!(result["supplemented_source_ids"], json!([first, second]));
}

#[test]
fn delivered_evidence_of_an_older_file_version_is_not_reused() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(20)).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# A\nValues are assigned in order. a.rs:1-20\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"a","title":"a","section":"# A","status":"written"}),
    );
    run(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":1,"max_lines":20}),
    );
    std::fs::write(
        dir.path().join("a.rs"),
        format!("{}// changed\n", numbered_source(20)),
    )
    .unwrap();
    let fresh = run(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":1,"max_lines":10}),
    )["source"]["id"]
        .clone();
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"a","source_ids":[fresh],"verification_note":"Compared the assignments"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("source_coverage_missing"), "{error}");
    assert!(error.contains("\"start_line\":11"), "{error}");
    assert_ne!(
        s.investigations
            .iter()
            .find(|i| i.id == "a")
            .unwrap()
            .status,
        "verified"
    );
}

#[test]
fn planned_section_names_rebind_to_the_written_heading_by_title() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(3)).unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"flow","title":"2. Agent branches and termination","section":"## 2. Agent branches","status":"in_progress"}),
    );
    let edit = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\n## 2. Agent branches and termination\nThree values are assigned. a.rs:1-3\n"}),
    );
    assert_eq!(edit["written_items"], json!(["flow"]));
    let item = s.investigations.iter().find(|i| i.id == "flow").unwrap();
    assert_eq!(item.status, "written");
    assert_eq!(
        item.section,
        "# Guide\n## 2. Agent branches and termination"
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"flow","source_ids":[source],"verification_note":"Compared the three assignments"}),
    );
}

#[test]
fn planned_sections_rebind_by_level_free_title_or_section_number() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(3)).unwrap();
    // The shapes from a live run: planned as level-1 headings, written as
    // level-2 headings, and the first one also reworded.
    for (id, title, section) in [
        (
            "first",
            "Start screen",
            "# 1. First screen, streaming progress and stop",
        ),
        ("second", "Reading answers", "# 2. Reading answers"),
        ("dup_a", "Admin A", "# 3. Admin"),
        ("dup_b", "Admin B", "# 3. Admin panel"),
        ("year", "Release", "# 2024 release"),
    ] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":title,"section":section,"status":"in_progress"}),
        );
    }
    let edit = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Manual\n\n## 1. First screen, progress and stop\nBody a.rs:1\n\n## 2. Reading answers\nBody a.rs:2\n\n## 3. Admin settings\nBody a.rs:3\n\n## 2025 release\nBody\n"}),
    );
    assert_eq!(edit["written_items"], json!(["first", "second"]));
    let section = |id: &str| {
        s.investigations
            .iter()
            .find(|item| item.id == id)
            .unwrap()
            .section
            .clone()
    };
    assert_eq!(
        section("first"),
        "# Manual\n## 1. First screen, progress and stop"
    );
    assert_eq!(section("second"), "# Manual\n## 2. Reading answers");
    // Two items numbered 3 and a year are left for an explicit section.
    assert_eq!(section("dup_a"), "# 3. Admin");
    assert_eq!(section("dup_b"), "# 3. Admin panel");
    assert_eq!(section("year"), "# 2024 release");

    // Settling one duplicate by hand lets status=written alone bind nothing
    // already claimed, and the reported error still lists the headings.
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"dup_a","section":"## 3. Admin settings","status":"written"}),
    );
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"dup_b","status":"written"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("section_not_found:"), "{error}");
}

#[test]
fn written_status_alone_rebinds_a_planned_section() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"flow","title":"Flow","section":"# 4. Admin panel and binding edits","status":"in_progress"}),
    );
    // Written by hand so no document edit rebinds it first.
    std::fs::write(&s.project.output, "# Manual\n## 4. Admin panel\nBody\n").unwrap();
    let registered = run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"flow","status":"written"}),
    );
    assert_eq!(registered["section"], "# Manual\n## 4. Admin panel");
}

#[test]
fn section_insert_errors_name_the_heading_that_breaks_the_rule() {
    let body = "# Manual\n## 1. Start\nBody\n";
    let cases = [
        // The live-run shape: starts at the right level, then a sibling.
        (
            "### 1-1. Overview\ntext\n### 1-2. Details\nmore\n",
            "line 3 of text starts another level-3 section \"### 1-2. Details\"",
        ),
        (
            "#### Too deep\ntext\n",
            "starts with level-4 \"#### Too deep\"",
        ),
        (
            "Intro\n### Late\ntext\n",
            "must start on its first line with a level-3 heading",
        ),
    ];
    for (text, expected) in cases {
        for batch in [false, true] {
            let (_dir, mut s) = setup();
            std::fs::write(&s.project.output, body).unwrap();
            let edit = json!({"action":"insert_last_child","section":"## 1. Start","text":text});
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
            assert!(error.contains("invalid_argument_value"), "{error}");
            assert!(error.contains(expected), "{error}");
            assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
        }
    }
    // The live shape: re-inserting an existing heading beside itself to
    // expand it names the duplicate instead of an ambiguous section.
    for (action, section) in [
        ("insert_after", "## 1. Start"),
        ("insert_last_child", "# Manual"),
    ] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let error = tools::execute(
            &mut s,
            "document_edit",
            json!({"action":action,"section":section,"expected_hash":tools::hash(body.as_bytes()),"text":"## 1. Start\nMore\n"}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with(&format!("invalid_argument_value: {action} text starts with \"## 1. Start\", which already exists")),
            "{error}"
        );
        assert!(error.contains("level-3 headings"), "{error}");
    }
    // Leading blank lines are dropped, and deeper nested headings inside the
    // one section remain accepted.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"## 1. Start","expected_hash":tools::hash(body.as_bytes()),"text":"\n  \n### 1-1. Blank first\ntext\n"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Manual\n## 1. Start\nBody\n### 1-1. Blank first\ntext\n"
    );
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"## 1. Start","expected_hash":tools::hash(body.as_bytes()),"text":"### 1-1. Overview\n#### Step\ntext\n"}),
    );
}

#[test]
fn audit_reports_cited_sections_without_an_investigation_item() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(6)).unwrap();
    let doc = "# Manual\nIntro.\n## 1. Start\nBody a.rs:1-2\n## 2. Answers\nBody a.rs:3-4\n### 2-1. Tables\nMore a.rs:5\n## Notes\nNo citation here.\n";
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":doc}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"start","title":"Start","section":"## 1. Start","status":"written"}),
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"start","source_ids":[source],"verification_note":"Compared a.rs:1-2"}),
    );
    let uncovered = |s: &mut Session| -> Vec<Value> {
        run(s, "document_audit", json!({}))["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|issue| issue["kind"] == "uncovered_section")
            .cloned()
            .collect()
    };
    // Each cited section names its innermost heading; the uncited one is
    // not reported, and every item being settled does not hide the gap.
    let issues = uncovered(&mut s);
    assert_eq!(issues.len(), 2, "{issues:?}");
    assert_eq!(issues[0]["section"], "# Manual\n## 2. Answers");
    assert_eq!(
        issues[1]["section"],
        "# Manual\n## 2. Answers\n### 2-1. Tables"
    );
    assert_eq!(issues[1]["citations"], 1);
    let check = run(&mut s, "investigation", json!({"action":"final_check"}));
    assert_eq!(check["complete"], false, "{check}");
    assert_eq!(check["audit"]["structural_ok"], false);

    // An item on the parent section covers its child as well.
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"answers","title":"Answers","section":"## 2. Answers","status":"written"}),
    );
    assert!(uncovered(&mut s).is_empty());
}

#[test]
fn revision_conflict_names_a_malformed_hash() {
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        let body = "# A\nOne.\n";
        std::fs::write(&s.project.output, body).unwrap();
        let real = tools::hash(body.as_bytes());
        // The live shape: a retyped 65-character hash.
        for (expected, message) in [
            (format!("{real}c"), "got 65"),
            (
                "0".repeat(64),
                "the document changed since this expected_hash",
            ),
        ] {
            let edit = json!({"action":"replace_text","old_text":"One.","text":"Two."});
            let error = if batch {
                tools::execute(
                    &mut s,
                    "document_edit_batch",
                    json!({"expected_hash":expected,"edits":[edit]}),
                )
            } else {
                let mut edit = edit;
                edit["expected_hash"] = json!(expected);
                tools::execute(&mut s, "document_edit", edit)
            }
            .unwrap_err()
            .to_string();
            assert!(error.starts_with("document_revision_conflict:"), "{error}");
            assert!(error.contains(message), "{error}");
            assert!(
                !error.contains(&real),
                "the current hash is not handed out: {error}"
            );
        }
    }
}

#[test]
fn array_arguments_sent_as_json_text_are_decoded() {
    let (_dir, mut s) = setup();
    let body = "# A\nOne.\n";
    std::fs::write(&s.project.output, body).unwrap();
    // The live shape: edits as the JSON text of the array.
    let edits = json!([{"action":"replace_text","old_text":"One.","text":"Two."}]).to_string();
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":tools::hash(body.as_bytes()),"edits":edits}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# A\nTwo.\n"
    );
    // Text that is not an array of that shape is still rejected.
    let body = std::fs::read_to_string(&s.project.output).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":tools::hash(body.as_bytes()),"edits":"{\"action\":\"append\"}"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("invalid_argument_type"), "{error}");
}

#[test]
fn source_changed_names_the_source_and_output_document_reads() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(3)).unwrap();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nBody a.rs:1\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"guide","title":"Guide","section":"# Guide","status":"written"}),
    );
    // The live shape: a read of the output document passed as evidence
    // after the document was edited again.
    let doc_read = run(&mut s, "file_read", json!({"path":"summary.md"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":created["hash"],"old_text":"Body","text":"Text"}),
    );
    let verify = |s: &mut Session, id: &Value| {
        tools::execute(
            s,
            "investigation",
            json!({"action":"verify","id":"guide","source_ids":[id],"verification_note":"Compared a.rs:1"}),
        )
        .unwrap_err()
        .to_string()
    };
    let error = verify(&mut s, &doc_read);
    assert!(error.starts_with("source_changed:"), "{error}");
    assert!(
        error.contains("is a read of the output document"),
        "{error}"
    );
    assert!(error.contains(doc_read.as_str().unwrap()), "{error}");
    // A project file that changed names its ID and path.
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    std::fs::write(dir.path().join("a.rs"), numbered_source(4)).unwrap();
    let error = verify(&mut s, &source);
    assert!(
        error.starts_with(&format!("source_changed: {} (", source.as_str().unwrap())),
        "{error}"
    );
    assert!(error.contains("a.rs) changed since it was read"), "{error}");
}

#[test]
fn audit_flags_the_output_path_written_into_the_document() {
    let (_dir, mut s) = setup();
    let output = s
        .project
        .output
        .canonicalize()
        .unwrap_or(s.project.output.clone());
    // The live shape: a closing "document info" line with the output path.
    for path in [
        s.project.output.display().to_string(),
        output.display().to_string(),
    ] {
        std::fs::write(
            &s.project.output,
            format!("# Guide\nBody.\n\n**문서 정보**: 생성 경로는 {path}.\n"),
        )
        .unwrap();
        let audit = run(&mut s, "document_audit", json!({}));
        let issue = audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|issue| issue["kind"] == "output_path_in_document")
            .cloned();
        assert_eq!(issue.unwrap()["line"], 4, "{audit}");
        assert_eq!(audit["structural_ok"], false);
    }
    // Naming the file relatively is ordinary content.
    std::fs::write(&s.project.output, "# Guide\nSee summary.md for details.\n").unwrap();
    let audit = run(&mut s, "document_audit", json!({}));
    assert!(
        !audit.to_string().contains("output_path_in_document"),
        "{audit}"
    );
}

#[test]
fn verify_binds_the_section_of_an_item_registered_without_one() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(4)).unwrap();
    // The live shape: items planned without a section, then verified with
    // verify_batch right after writing.
    for (id, title) in [
        ("start", "Start"),
        ("answers", "Answers"),
        ("other", "Other"),
    ] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","id":id,"title":title,"status":"in_progress"}),
        );
    }
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Manual\n## 1. 첫 화면\nBody a.rs:1-2\n## 2. 답변 읽기\nBody a.rs:3-4\n"}),
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    let result = run(
        &mut s,
        "investigation",
        json!({"action":"verify_batch","items":{
            "start":{"source_ids":[source],"verification_note":"Compared a.rs:1-2","section":"## 1. 첫 화면"},
            "answers":{"source_ids":[source],"verification_note":"Compared a.rs:3-4","section":"## 2. 답변 읽기"}
        }}),
    );
    assert_eq!(
        result["succeeded_ids"],
        json!(["answers", "start"]),
        "{result}"
    );
    let item = |s: &Session, id: &str| {
        s.investigations
            .iter()
            .find(|i| i.id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(item(&s, "start").status, "verified");
    assert_eq!(item(&s, "start").section, "# Manual\n## 1. 첫 화면");

    // Another item's section is refused, and so is moving a bound item.
    let verify = |s: &mut Session, id: &str, section: &str| {
        tools::execute(
            s,
            "investigation",
            json!({"action":"verify","id":id,"source_ids":[source],"verification_note":"Compared","section":section}),
        )
        .unwrap_err()
        .to_string()
    };
    let error = verify(&mut s, "other", "## 1. 첫 화면");
    assert!(error.contains("already belongs to item start"), "{error}");
    assert!(item(&s, "other").section.is_empty());
    let error = verify(&mut s, "start", "## 2. 답변 읽기");
    assert!(error.contains("is registered to section"), "{error}");
    assert_eq!(item(&s, "start").section, "# Manual\n## 1. 첫 화면");
}

#[test]
fn a_named_section_falls_back_to_the_unique_numbered_heading() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), numbered_source(4)).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Manual\n## 1. 첫 화면과 질문 입력·전송\nBody a.rs:1\n## 2. 답변 읽기\nBody a.rs:2\n"}),
    );
    // The live shape: the section named as in the request, not as written.
    let registered = run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"one","title":"One","section":"1. 첫 화면과 질문 입력·전송, 스트리밍 중 진행 표시와 중단","status":"written"}),
    );
    assert_eq!(
        registered["section"],
        "# Manual\n## 1. 첫 화면과 질문 입력·전송"
    );
    // verify takes the same fallback for an item without a section.
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"two","title":"Two","status":"in_progress"}),
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"two","section":"2. 답변 읽기: 표와 차트","source_ids":[source],"verification_note":"Compared a.rs:2"}),
    );
    let two = s.investigations.iter().find(|i| i.id == "two").unwrap();
    assert_eq!(
        (two.status.as_str(), two.section.as_str()),
        ("verified", "# Manual\n## 2. 답변 읽기")
    );
    // A heading already used by another item is not taken; the original error remains.
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"three","title":"Three","section":"1. 다른 이름","status":"written"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("section_not_found"), "{error}");
}

#[test]
fn a_retyped_citation_names_the_exact_document_passage() {
    let body = "# Manual\n## 1. Setup\nOpen settings (`frontend/src/App.jsx:306-308`). Then save.\nSee `a.rs:1` and `a.rs:1`.\n";
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let hash = tools::hash(body.as_bytes());
        let edit = |old_text: &str| {
            let edit = json!({"action":"replace_text","old_text":old_text,"text":"x","section":"## 1. Setup"});
            if batch {
                json!({"expected_hash":hash,"edits":[edit]})
            } else {
                let mut edit = edit;
                edit["expected_hash"] = json!(hash);
                edit
            }
        };
        let tool = if batch {
            "document_edit_batch"
        } else {
            "document_edit"
        };
        // The live shape: backtick moved and an en dash for the hyphen.
        let error = tools::execute(&mut s, tool, edit("frontend/src/App.jsx`:306–308"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                r#"only in backticks, dashes, quotes or spacing: "frontend/src/App.jsx:306-308""#
            ),
            "{error}"
        );
        // A repeated target and a missing one are explained on single edits too.
        let error = tools::execute(&mut s, tool, edit("`a.rs:1`"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("old_text occurs 2 times"), "{error}");
        let error = tools::execute(&mut s, tool, edit("nothing like this at all"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("old_text is not in the current document"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
        // The named passage works as old_text.
        let mut args = edit("frontend/src/App.jsx:306-308");
        let fix = json!("frontend/src/App.jsx:305-307");
        if batch {
            args["edits"][0]["text"] = fix;
        } else {
            args["text"] = fix;
        }
        run(&mut s, tool, args);
        assert!(
            std::fs::read_to_string(&s.project.output)
                .unwrap()
                .contains("(`frontend/src/App.jsx:305-307`)")
        );
    }
}

#[test]
fn a_short_heading_name_resolves_only_when_unique() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Manual\n## 1. 처음 설정: 모델 연결 정보 입력\nBody\n## 2. 채팅: 요청 보내기\nBody\n## 3. 채팅: 작업 제어\nBody\n"}),
    );
    // The live shapes: the title before the colon, with or without its number.
    for section in ["## 1. 처음 설정", "처음 설정", "1. 처음 설정: 다른 설명"] {
        let page = run(&mut s, "document_inspect", json!({"section":section}));
        assert_eq!(page["start_line"], 2, "{section}");
    }
    let registered = run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"setup","title":"Setup","section":"처음 설정","status":"written"}),
    );
    assert_eq!(
        registered["section"],
        "# Manual\n## 1. 처음 설정: 모델 연결 정보 입력"
    );
    // Two headings share the short title, or the section number disagrees.
    assert_eq!(
        run(&mut s, "document_inspect", json!({"section":"## 3. 채팅"}))["start_line"],
        6
    );
    for section in ["채팅", "## 5. 처음 설정"] {
        let error = tools::execute(&mut s, "document_inspect", json!({"section":section}))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("section_not_found"), "{section}: {error}");
    }
}

#[test]
fn a_misremembered_old_text_names_where_it_diverges() {
    // The live shape: old_text copied correctly, then a sentence that the
    // document no longer has.
    let body = "# 설정\n- 전체 범위에서 저장하면 세션이 열려 있을 때 같은 내용이 현재 세션에도 함께 저장됩니다(Settings.jsx:250-260).\n## 다음\n본문\n";
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let hash = tools::hash(body.as_bytes());
        let edit = json!({"action":"replace_text","old_text":"전체 범위에서 저장하면 세션이 열려 있을 때 같은 내용이 현재 세션에도 함께 저장됩니다(Settings.jsx:250-260). 해당 옵션의 화면 문구는 미확인입니다.","text":"x"});
        let error = if batch {
            tools::execute(
                &mut s,
                "document_edit_batch",
                json!({"expected_hash":hash,"edits":[edit]}),
            )
        } else {
            let mut edit = edit;
            edit["expected_hash"] = json!(hash);
            tools::execute(&mut s, "document_edit", edit)
        }
        .unwrap_err()
        .to_string();
        assert!(error.contains(r#"matches the document up to "#), "{error}");
        assert!(
            error.contains(r#"the document continues with "\n## 다음\n본문\n""#),
            "{error}"
        );
        assert!(
            error.contains(r#"old_text continues with " 해당 옵션의 화면 문구는 미확인입니다.""#),
            "{error}"
        );
    }
    // A short shared beginning gives the plain guidance instead.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":tools::hash(body.as_bytes()),"old_text":"- 전체 이야기는 다릅니다","text":"x"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("copy it exactly"), "{error}");
}

#[test]
fn an_append_after_the_models_own_write_may_omit_the_hash() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nOne.\n"}),
    );
    // The live shape: append straight after the model's own write.
    run(
        &mut s,
        "document_edit",
        json!({"action":"append","text":"## Two\nTwo.\n"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\nOne.\n## Two\nTwo.\n"
    );
    // After an outside change the model's last write is stale: still required.
    std::fs::write(&s.project.output, "# Guide\nChanged elsewhere.\n").unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"append","text":"## Three\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("expected_hash"), "{error}");
    // Other actions keep requiring it even right after a write.
    let hash = tools::hash(b"# Guide\nChanged elsewhere.\n");
    run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"## Three\n"}),
    );
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"Three","text":"3"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("expected_hash"), "{error}");
}

#[test]
fn an_insertion_that_repeats_its_anchor_is_refused() {
    let body = "# 사용법\n**전송**: Enter 키 또는 ➤ 전송 단추로 보냅니다 (`App.jsx:1325-1327`).\n";
    let anchor = "**전송**: Enter 키 또는 ➤ 전송 단추로 보냅니다 (`App.jsx:1325-1327`).";
    for (batch, action) in [(false, "insert_after_text"), (true, "insert_before_text")] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        // The live shape: the "inserted" text restates the anchor to reword it.
        let edit = json!({"action":action,"old_text":anchor,"text":format!("\n{anchor} 빈 입력은 보낼 수 없습니다.")});
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
        assert!(error.contains("the passage would appear twice"), "{error}");
        assert!(error.contains("use replace_text"), "{error}");
        assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    }
    // New text next to the anchor, and a short anchor that recurs, still work.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","expected_hash":tools::hash(body.as_bytes()),"old_text":anchor,"text":"\n빈 입력은 보낼 수 없습니다."}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","expected_hash":result["hash"],"old_text":"# 사용법","text":"\n# 사용법 요약"}),
    );
}

#[test]
fn reading_the_output_before_it_exists_says_to_create_it() {
    let (_dir, mut s) = setup();
    // The live shape: reads and an audit of the output before any write.
    let output = s.project.output.display().to_string();
    for (tool, args) in [
        ("file_read", json!({"path":output,"max_lines":10})),
        ("document_inspect", json!({"path":output})),
    ] {
        let error = tools::execute(&mut s, tool, args).unwrap_err().to_string();
        assert!(error.starts_with("file_not_found"), "{tool}: {error}");
        assert!(
            error.contains("The configured output does not exist yet"),
            "{tool}: {error}"
        );
    }
    let error = tools::execute(&mut s, "document_audit", json!({}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("document_missing"), "{error}");
    // A missing project file keeps the ordinary guidance.
    let error = tools::execute(&mut s, "file_read", json!({"path":"nope.rs"}))
        .unwrap_err()
        .to_string();
    assert!(
        !error.contains("configured output does not exist yet"),
        "{error}"
    );
}

#[test]
fn appended_heading_starts_a_new_line() {
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, "# A\nFirst section ends here.").unwrap();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"## B\nSecond section.\n"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# A\nFirst section ends here.\n## B\nSecond section.\n"
    );
    let outline = run(&mut s, "document_inspect", json!({}));
    assert!(outline.to_string().contains("## B"));
}

#[test]
fn batch_accepts_a_repeated_nested_expected_hash_and_rejects_conflicts() {
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, "# A\nOne.\n\n# B\nTwo.\n").unwrap();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    // The hash repeated inside every edit, as the live model sent it.
    let result = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[
            {"action":"replace_text","expected_hash":hash,"old_text":"One.","text":"First."},
            {"action":"replace_text","expected_hash":hash,"old_text":"Two.","text":"Second."}
        ]}),
    );
    let hash = result["hash"].as_str().unwrap().to_owned();
    // Only nested copies: the batch hash is taken from them.
    run(
        &mut s,
        "document_edit_batch",
        json!({"edits":[{"action":"replace_text","expected_hash":hash,"old_text":"First.","text":"1st."}]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# A\n1st.\n\n# B\nSecond.\n"
    );
    let current = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":current,"edits":[
            {"action":"replace_text","expected_hash":"stale","old_text":"1st.","text":"x"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("conflicting_arguments"), "{error}");
}

#[test]
fn batch_target_failures_say_why_old_text_did_not_match() {
    let (_dir, mut s) = setup();
    std::fs::write(
        &s.project.output,
        "# A\nThe loop runs five times.\nIt stops.\n",
    )
    .unwrap();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    // edits[0] rewrites the sentence edits[1] was copied from.
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[
            {"action":"replace_text","old_text":"The loop runs five times.","text":"The for loop runs exactly five times."},
            {"action":"replace_text","old_text":"loop runs five","text":"loop runs 5"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("edits[0] in this same batch already changed it"),
        "{error}"
    );
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[
            {"action":"replace_text","old_text":"never written","text":"x"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not in the current document"), "{error}");
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[
            {"action":"replace_text","old_text":"t","text":"x"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("occurs") && error.contains("times"),
        "{error}"
    );
    // Nothing was persisted by the failed batches.
    assert_eq!(
        tools::hash(&std::fs::read(&s.project.output).unwrap()),
        hash
    );
}

#[test]
fn common_argument_aliases_are_accepted_and_conflicts_rejected() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("a.rs"), "let a = 1;\n").unwrap();
    std::fs::write(&s.project.output, "# A\nOne. a.rs:1\n").unwrap();
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    // new_text is taken as text, alone and inside batch edits.
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"One.","new_text":"First."}),
    );
    let hash = result["hash"].as_str().unwrap().to_owned();
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[{"action":"replace_text","old_text":"First.","new_text":"Value one."}]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# A\nValue one. a.rs:1\n"
    );
    let hash = tools::hash(&std::fs::read(&s.project.output).unwrap());
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":hash,"old_text":"Value one.","text":"x","new_text":"y"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("conflicting_arguments"), "{error}");
    // max_issues is the audit page size.
    let audit = run(&mut s, "document_audit", json!({"max_issues":1}));
    assert!(audit["issues"].as_array().unwrap().len() <= 1);
    // verify ignores a restated registered section but rejects another one.
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"a","title":"A","section":"# A","status":"written"}),
    );
    let source = run(&mut s, "file_read", json!({"path":"a.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"a","section":"# A","source_ids":[source],"verification_note":"Compared the assignment"}),
    );
    let error = tools::execute(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"a","section":"# Other","source_ids":[source],"verification_note":"Compared"}),
    )
    .unwrap_err()
    .to_string();
    // A section the item is not registered to is still refused (moving a
    // registered item is covered by the verify section-binding test).
    assert!(error.starts_with("section_not_found"), "{error}");
    assert_eq!(s.investigations[0].section, "# A");
}

#[test]
fn a_user_selected_workflow_applies_to_each_request_and_is_locked() {
    let (_dir, mut s) = setup();
    s.active_tools.clear();
    s.workflow_mode = "source_document".into();
    s.add_user("Write the manual.".into());
    assert_eq!(s.task.workflow, "source_document");
    assert!(s.task.require_investigation && s.is_document_work());
    assert!(s.active_tools.contains("investigation") && s.active_tools.contains("document_edit"));
    // The model may fill in the task but not reclassify the request.
    let error = tools::execute(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"workflow":"answer"}}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("workflow_selected_by_user:"), "{error}");
    run(
        &mut s,
        "task_state",
        json!({"action":"update","patch":{"completion":["The manual is saved."]}}),
    );
    // A later request resets the task and applies the selection again.
    s.workflow_mode = "answer".into();
    s.add_user("What does main do?".into());
    assert_eq!(s.task.workflow, "answer");
    assert!(!s.task.require_investigation && !s.is_document_work());
}

#[test]
fn an_edit_after_verification_offers_the_reverify_call() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let file = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","status":"written","section":"# Entry"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[file],"verification_note":"Compared main.rs:1."}),
    );
    // A later edit to the section returns the item to "written".
    let doc = std::fs::read(&s.project.output).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":tools::hash(&doc),"text":"# Entry\nThe entry point is main (main.rs:1).\n"}),
    );
    let check = run(&mut s, "investigation", json!({"action":"final_check"}));
    assert_eq!(check["complete"], false);
    assert_eq!(check["incomplete"][0]["previous_source_ids"], json!([file]));
    let example = check["verify_batch_example"].clone();
    assert_eq!(example["items"]["entry"]["source_ids"], json!([file]));
    // The offered call re-verifies the edited section as-is.
    let verified = run(&mut s, "investigation", example);
    assert_eq!(s.investigations[0].status, "verified", "{verified}");
}

#[test]
fn unwritten_items_get_the_call_that_advances_them() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    // The live shape: items registered before writing, with no section.
    for title in ["Entry point", "Errors"] {
        run(
            &mut s,
            "investigation",
            json!({"action":"upsert","status":"in_progress","title":title}),
        );
    }
    let file = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Entry point\nIt starts in main (main.rs:1).\n"}),
    );
    let check = run(&mut s, "investigation", json!({"action":"final_check"}));
    let steps = check["next_steps"].as_array().unwrap();
    // Writing the matching heading bound the first item; its next call is
    // the verification. The other section must be written first.
    let entry = steps
        .iter()
        .find(|step| step["next"]["action"] == "verify")
        .unwrap_or_else(|| panic!("{check}"));
    assert!(
        steps.iter().any(|step| step["next"]
            .as_str()
            .is_some_and(|next| next.contains("\"Errors\""))),
        "{check}"
    );
    let id = entry["id"].as_str().unwrap();
    // The offered call names the section's cited file; it verifies as sent,
    // using the evidence already delivered by the read.
    assert_eq!(entry["next"]["source_ids"], json!(["main.rs"]));
    assert!(file.is_string());
    run(&mut s, "investigation", entry["next"].clone());
    assert!(
        s.investigations
            .iter()
            .any(|i| i.id == id && i.status == "verified")
    );
    // An in_progress item registered after its section was written gets the
    // upsert that marks it written.
    let doc = std::fs::read(&s.project.output).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":tools::hash(&doc),"text":"# Guide\n## Entry point\nIt starts in main (main.rs:1).\n## Errors\nNone.\n## Limits\nNone.\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","status":"in_progress","title":"Limits"}),
    );
    let steps = run(&mut s, "investigation", json!({"action":"final_check"}))["next_steps"].clone();
    let limits = steps
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["next"]["action"] == "upsert")
        .unwrap_or_else(|| panic!("{steps}"));
    assert_eq!(limits["next"]["status"], "written");
    assert_eq!(limits["next"]["section"], "# Guide\n## Limits");
}

#[test]
fn a_passing_final_check_says_what_finishes_the_task() {
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    let file = run(&mut s, "file_read", json!({"path":"main.rs"}))["source"]["id"].clone();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Entry\nmain.rs:1\n"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry","status":"written","section":"# Entry"}),
    );
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[file],"verification_note":"Compared main.rs:1."}),
    );
    let revision = s.task.plan_revision;
    run(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":revision,"operations":[{"op":"insert","texts":["Write the entry section"]}]}),
    );
    // The live shape: passing preflights repeated while a to-do stayed open.
    let check = run(&mut s, "investigation", json!({"action":"final_check"}));
    assert_eq!(check["complete"], true, "{check}");
    let next = check["next"].as_str().unwrap();
    assert!(
        next.contains("task_plan apply") && next.contains("final answer"),
        "{next}"
    );
    let id = s.task.todos[0].id.clone();
    let revision = s.task.plan_revision;
    run(
        &mut s,
        "task_plan",
        json!({"action":"apply","expected_revision":revision,"operations":[{"op":"complete","id":id,"result":"Entry section written and verified"}]}),
    );
    let check = run(&mut s, "investigation", json!({"action":"final_check"}));
    assert!(
        check["next"]
            .as_str()
            .unwrap()
            .starts_with("Give the final answer now")
    );
}

#[test]
fn a_directory_path_is_a_targeted_listing_while_verifying() {
    let (dir, mut s) = setup();
    std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
    std::fs::write(dir.path().join("frontend/src/App.jsx"), "x\n").unwrap();
    run(
        &mut s,
        "investigation",
        json!({"action":"upsert","id":"entry","title":"entry"}),
    );
    s.run_guidance["phase"] = json!("verify");
    // The live shape: a directory path refused as broad discovery.
    let listed = run(
        &mut s,
        "file_list",
        json!({"mode":"paths","path":"frontend/src"}),
    );
    assert_eq!(listed["paths"], json!(["frontend/src/App.jsx"]));
    let error = tools::execute(&mut s, "file_list", json!({"mode":"paths","path":"."}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("verification_reserve:"), "{error}");
}
