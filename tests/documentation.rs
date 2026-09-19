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
        json!({"action":"create","text":"# Entry\nmain.rs:1\n## Errors\nmain.rs:999\n"}),
    );
    for (id, section) in [("entry", "# Entry"), ("errors", "## Errors")] {
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
    run(
        &mut s,
        "investigation",
        json!({"action":"verify","id":"entry","source_ids":[id],"verification_note":"Agent claims comparison."}),
    );
    assert_eq!(
        run(&mut s, "investigation", json!({"action":"final_check"}))["complete"],
        false
    );
    assert_eq!(s.reviews, 0);
}

#[test]
fn settings_validate_reserves_and_patch_keeps_existing_references() {
    let (dir, mut s) = setup();
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
