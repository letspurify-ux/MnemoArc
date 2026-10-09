use crate::support;
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
            ..support::compact_config()
        },
    );
    s.active_tools = ToolRegistry::optional_names();
    (dir, s)
}
/// Citation checks and section binding are source_document features; the
/// default answer workflow reports a plain write.
fn source_setup() -> (tempfile::TempDir, Session) {
    let (dir, mut s) = setup();
    s.select_workflow("source_document").unwrap();
    (dir, s)
}
fn run(s: &mut Session, name: &str, args: Value) -> Value {
    tools::execute(s, name, args).unwrap()
}

#[test]
fn coverage_pagination_rejects_obsolete_document_versions() {
    let (dir, mut s) = setup();
    let observe = |s: &mut Session, name: &str, revision: usize| {
        std::fs::write(
            dir.path().join(name),
            format!("# Guide\nunread first\nobserved revision {revision}\nunread last\n"),
        )
        .unwrap();
        let call = mnemoarc::llm::ToolCall {
            id: format!("read-{name}-{revision}"),
            name: "file_read".into(),
            arguments: json!({"path":name,"start_line":3,"max_lines":1}).to_string(),
        };
        let result = tools::envelope(tools::execute(
            s,
            "file_read",
            serde_json::from_str(&call.arguments).unwrap(),
        ));
        assert_eq!(result["status"], "ok");
        tools::record_delivered_read(s, &call, &result);
        let page = run(s, "document_inspect", json!({"path":name,"limit":1}));
        assert_eq!(page["coverage"]["missing_range_count"], 2);
        assert_eq!(page["coverage"]["next_offset"], 1);
        page
    };
    // A colon in a Unix filename must not make another path look like the
    // current path's obsolete cursor prefix. Windows drive colons are also
    // handled by splitting the version and offset from the end of each key.
    let other_path = if cfg!(unix) {
        "input.md:other.md"
    } else {
        "other.md"
    };
    let other = observe(&mut s, other_path, 0);
    let mut previous: Option<Value> = None;
    for revision in 0..4 {
        let page = observe(&mut s, "input.md", revision);
        if let Some(previous) = &previous {
            let error = tools::execute(
                &mut s,
                "document_inspect",
                json!({"path":"input.md","limit":1,"coverage_offset":1,"expected_hash":previous["hash"]}),
            )
            .unwrap_err();
            assert!(error.to_string().starts_with("document_revision_conflict:"));
        }
        previous = Some(page);
    }
    // Restoring identical old bytes must not revive a discarded legacy page:
    // its delivered coverage may no longer match even though its hash does.
    let input = dir.path().join("input.md");
    let current = std::fs::read(&input).unwrap();
    let retired = "# Guide\nunread first\nobserved revision 0\nunread last\n";
    std::fs::write(&input, retired).unwrap();
    let error = tools::execute(
        &mut s,
        "document_inspect",
        json!({"path":"input.md","limit":1,"coverage_offset":1,"expected_hash":tools::hash(retired.as_bytes())}),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("document_coverage_revision_conflict:")
    );
    std::fs::write(&input, current).unwrap();
    for (path, page) in [("input.md", previous.unwrap()), (other_path, other)] {
        let last = run(
            &mut s,
            "document_inspect",
            json!({"path":path,"limit":1,"coverage_offset":1,"expected_hash":page["hash"]}),
        );
        assert!(last["coverage"]["next_offset"].is_null());
        // A caller that echoes the revision can replay a consumed page
        // without depending on the server's legacy continuation entry.
        let replay = run(
            &mut s,
            "document_inspect",
            json!({"path":path,"limit":1,"coverage_offset":1,"expected_hash":page["hash"],"expected_coverage_revision":page["coverage"]["revision"]}),
        );
        assert_eq!(replay["coverage"], last["coverage"]);
    }
}

#[test]
fn a_citation_into_an_unreadable_file_says_to_remove_it() {
    // A cited file over 16 MiB or not UTF-8 fails every audit; the bare read
    // error gave a model nothing to act on.
    let (dir, mut s) = source_setup();
    std::fs::File::create(dir.path().join("huge.rs"))
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    std::fs::write(dir.path().join("blob.rs"), b"\x00\x01\x02").unwrap();
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Doc\nSee huge.rs:1-2 and blob.rs:1-1.\n"}),
    );
    let issues = saved["citation_check"]["issues"].as_array().unwrap();
    let large = issues
        .iter()
        .find(|i| i["citation"] == "huge.rs:1-2")
        .unwrap();
    assert!(
        large["error"]
            .as_str()
            .unwrap()
            .starts_with("unsupported_large_file"),
        "{large}"
    );
    assert!(
        large["guidance"]
            .as_str()
            .unwrap()
            .contains("remove this citation"),
        "{large}"
    );
    let binary = issues
        .iter()
        .find(|i| i["citation"] == "blob.rs:1-1")
        .unwrap();
    assert!(
        binary["guidance"].as_str().unwrap().contains("not UTF-8"),
        "{binary}"
    );
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], false);
    assert!(audit["issues"].as_array().unwrap().iter().any(|i| {
        i["guidance"]
            .as_str()
            .is_some_and(|g| g.contains("remove this citation"))
    }));
}

#[test]
fn a_long_outline_in_a_save_result_keeps_the_edited_part_and_the_top_levels() {
    // A 400-section document's save result carried 400 outline entries, cut
    // to the result budget on every save. The outline is bounded instead:
    // the edited headings with their ancestors and neighbours, then the top
    // levels while they fit; heading_count and outline_omitted say what is
    // left out, and document_inspect pages the complete outline.
    let (_dir, mut s) = source_setup();
    let mut doc = String::from("# Manual\n");
    for h in 0..400 {
        doc.push_str(&format!("## Section {h}\nBody.\n"));
    }
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":doc}),
    );
    // A whole-document write has no local focus: the levels alone, in order.
    let outline = created["outline"].as_array().unwrap();
    assert_eq!(outline.len(), 60, "{created}");
    assert_eq!(created["heading_count"], 401);
    assert_eq!(created["outline_omitted"], 341);
    assert_eq!(outline[0]["heading"], "# Manual");
    assert_eq!(outline[59]["heading"], "## Section 58");
    assert!(
        created["outline_note"]
            .as_str()
            .unwrap()
            .contains("document_inspect"),
        "{created}"
    );
    let appended = run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":created["hash"],"text":"### Detail\nMore."}),
    );
    let outline = appended["outline"].as_array().unwrap();
    assert_eq!(outline.len(), 60, "{appended}");
    assert_eq!(appended["outline_omitted"], 342);
    let headings: Vec<&str> = outline
        .iter()
        .map(|entry| entry["heading"].as_str().unwrap())
        .collect();
    // The new section with its ancestors and the four headings before it
    // (its parent among them), then level-2 sections from the top; the
    // middle of the document is left out.
    assert_eq!(headings[0], "# Manual");
    assert_eq!(headings[54], "## Section 53");
    assert_eq!(
        headings[55..],
        [
            "## Section 396",
            "## Section 397",
            "## Section 398",
            "## Section 399",
            "### Detail"
        ],
        "{headings:?}"
    );
    assert!(!headings.contains(&"## Section 100"));
    assert_eq!(
        outline[59]["section_path"],
        "# Manual\n## Section 399\n### Detail"
    );
    // A result budget below even the bounded outline cuts it further; the
    // continuation then pages the complete outline from its start, because
    // the bounded entries are a selection rather than a prefix.
    let call = mnemoarc::llm::ToolCall {
        id: "save".into(),
        name: "document_edit".into(),
        arguments: json!({"action":"append"}).to_string(),
    };
    let mut base = json!({"status":"ok","data":appended.clone()});
    base["data"]["outline"] = json!([]);
    let limit = tools::result_tokens(&call, &base, &s.config.model) + 300;
    let limited = tools::limit_result(
        &mut s,
        &call,
        json!({"status":"ok","data":appended.clone()}),
        limit,
    );
    let retained = limited["data"]["outline"].as_array().unwrap().len();
    assert!(retained > 0 && retained < 60, "{retained}");
    assert_eq!(limited["truncated"], true);
    assert_eq!(limited["next_cursor"]["tool"], "document_inspect");
    assert_eq!(limited["next_cursor"]["offset"], 0);
    assert_eq!(limited["next_cursor"]["expected_hash"], appended["hash"]);
}

#[test]
fn a_save_returns_the_outline_for_the_next_placement() {
    // Live runs paid a document_inspect round before most insertions because
    // the save result carried no outline: 104 outline-only inspections in 41
    // runs, 87 of them followed directly by an edit.
    let (_dir, mut s) = source_setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Setup\n### Steps\nOne.\n## Usage\n### Steps\nTwo.\n"}),
    );
    let outline = created["outline"].as_array().unwrap();
    assert_eq!(outline.len(), 5, "{created}");
    assert_eq!(created["heading_count"], 5);
    assert!(created.get("outline_omitted").is_none());
    assert_eq!(
        outline[2],
        json!({"heading":"### Steps","section_path":"# Guide\n## Setup\n### Steps","level":3,"start_line":3})
    );
    assert_eq!(outline[4]["section_path"], "# Guide\n## Usage\n### Steps");
    // The repeated title is placed from the save result alone.
    let inserted = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":outline[4]["section_path"],"expected_hash":created["hash"],"text":"### Notes\nThree."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## Setup\n### Steps\nOne.\n## Usage\n### Steps\nTwo.\n### Notes\nThree.\n"
    );
    assert_eq!(
        inserted["outline"].as_array().unwrap().len(),
        6,
        "{inserted}"
    );
    // A batch and the answer workflow report the outline too.
    let batched = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":inserted["hash"],"edits":[{"action":"append","text":"## Limits\nBounds."}]}),
    );
    assert_eq!(batched["outline"][6]["heading"], "## Limits", "{batched}");
    s.select_workflow("answer").unwrap();
    let plain = run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":batched["hash"],"text":"## More\nText."}),
    );
    assert_eq!(plain["outline"][7]["heading"], "## More", "{plain}");
    assert!(plain.get("citation_check").is_none());
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
        "## One\n# Two\n",
        "## One\n## Flow\n",
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
        "### One\n## Two\n",
        "### One\n### First\n",
        "### One\n### One\n",
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
fn the_current_document_hash_also_names_the_section_version() {
    // A live model copied the document hash from an audit into
    // expected_section_hash, was refused as stale and re-read a section
    // that had not changed.
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\n## Input\n\nOne line box.\n\n## Output\n\nAnswers.\n"}),
    );
    let replaced = run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","section":"## Input","expected_section_hash":created["hash"],"old_text":"One line box.","text":"A box that grows."}),
    );
    let rewritten = run(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## Output","expected_hash":replaced["hash"],"expected_section_hash":replaced["hash"],"text":"## Output\n\nShort answers.\n"}),
    );
    // An older document hash is still refused.
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","section":"## Input","expected_section_hash":created["hash"],"old_text":"A box","text":"The box"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with(
            "section_revision_conflict: expected_section_hash is not the current hash"
        ),
        "{error}"
    );
    // In a batch the hash of the document it started from still names a
    // section that its earlier edits left as it was ...
    let base = rewritten["hash"].clone();
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":base,"edits":[
            {"action":"replace_text","section":"## Output","old_text":"Short answers.","text":"Brief answers."},
            {"action":"replace_text","section":"## Input","expected_section_hash":base,"old_text":"A box","text":"The box"}
        ]}),
    );
    let after = std::fs::read_to_string(&s.project.output).unwrap();
    assert!(
        after.contains("The box that grows.") && after.contains("Brief answers."),
        "{after}"
    );
    // ... but not one they changed, and the hint names that edit.
    let base = json!(tools::hash(after.as_bytes()));
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":base,"edits":[
            {"action":"replace_text","section":"## Output","old_text":"Brief","text":"Terse"},
            {"action":"replace_text","section":"## Output","expected_section_hash":base,"old_text":"answers","text":"replies"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("index=1") && error.contains("section_revision_conflict"),
        "{error}"
    );
    assert!(
        error.contains("edits[0] in this same batch already changed it"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), after);
}

#[test]
fn a_section_hash_beside_a_scoped_text_edit_guards_that_section() {
    // A live model sent replace_text with section and the section_hash it
    // had read, and was refused: only action=section took a section hash.
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\n## Input\n\nOne line box.\n\n## Output\n\nAnswers.\n"}),
    );
    let section_hash = |s: &mut Session| {
        run(s, "document_inspect", json!({"section":"## Input"}))["section_hash"].clone()
    };
    let read = section_hash(&mut s);
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","section":"## Input","expected_section_hash":read,"old_text":"One line box.","text":"A box that grows."}),
    );
    // The hash read before that edit is stale now; nothing is written.
    let before = std::fs::read_to_string(&s.project.output).unwrap();
    assert!(before.contains("A box that grows."), "{before}");
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","section":"## Input","expected_section_hash":read,"old_text":"A box","text":"The box"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with(
            "section_revision_conflict: expected_section_hash is not the current hash"
        ),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), before);
    // Batch edits take the same guard.
    let current = section_hash(&mut s);
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":tools::hash(before.as_bytes()),"edits":[{"action":"delete_text","section":"## Input","expected_section_hash":current,"old_text":" that grows"}]}),
    );
    assert!(
        std::fs::read_to_string(&s.project.output)
            .unwrap()
            .contains("A box.")
    );
    // A blank one is an unfilled placeholder; without section there is no
    // section for a filled one to check.
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","section":"## Input","expected_section_hash":"","old_text":"A box.","text":"The box."}),
    );
    let current = section_hash(&mut s);
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_section_hash":current,"old_text":"The box.","text":"A box."}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(
            "expected_section_hash of action=replace_text checks the section that section names"
        ),
        "{error}"
    );
}

#[test]
fn lone_carriage_returns_in_document_text_are_saved_as_line_breaks() {
    // A live model sent lone carriage returns as the line breaks of its
    // edits (19 to 40 each); saved, they showed as broken text that the
    // document review reported three times.
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\r\rFirst line.\rSecond line.\r\n"}),
    );
    let saved = std::fs::read_to_string(&s.project.output).unwrap();
    assert_eq!(saved, "# Guide\n\nFirst line.\nSecond line.\r\n");
    // An old_text copied from such text still finds the saved passage.
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":tools::hash(saved.as_bytes()),"edits":[{"action":"replace_text","old_text":"First line.\rSecond","text":"One line.\rTwo"}]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n\nOne line.\nTwo line.\r\n"
    );
}

#[test]
fn citations_into_test_code_are_named_on_save_and_audit() {
    // A live document cited a test's loop as how reviews repeat, and the
    // document review approved it.
    let (dir, mut s) = source_setup();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();
    std::fs::write(
        dir.path().join("src/agent.rs"),
        "fn run() {}\n\n#[cfg(test)]\nmod review_tests {\n    #[test]\n    fn loops() {}\n}\n\nfn after() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("tests/flow.rs"), "fn flow() {}\n").unwrap();
    std::fs::write(dir.path().join("src/view.test.js"), "it('opens');\n").unwrap();
    for path in ["src/agent.rs", "tests/flow.rs", "src/view.test.js"] {
        run(&mut s, "file_read", json!({"path":path}));
    }
    let text = "# Flow\n\nRuns start here (src/agent.rs:1).\nReviews repeat (src/agent.rs:5-6).\nFlows are checked (tests/flow.rs:1).\nThe view opens (src/view.test.js:1).\nAfter runs (src/agent.rs:9).\n";
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":text}),
    );
    let check = &saved["test_code_check"];
    let cited: Vec<_> = check["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["line"].as_u64().unwrap(),
                item["citation"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        cited,
        [
            (4, "src/agent.rs:5-6"),
            (5, "tests/flow.rs:1"),
            (6, "src/view.test.js:1"),
        ],
        "{saved}"
    );
    assert_eq!(check["flagged"], 3);
    // Advice only: the audit still passes and repeats it.
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
    assert_eq!(audit["test_code_check"]["flagged"], 3, "{audit}");
}

#[test]
fn an_unaccepted_edit_field_is_reported_with_the_fields_still_missing() {
    // The live call also lacked text; its error named only the extra field,
    // so one retry could fix one problem.
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\n## Input\n\nBox.\n"}),
    );
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"## Input","old_text":"Box."}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with(
            "invalid_action_arguments: document_edit action=insert_after does not accept old_text"
        ),
        "{error}"
    );
    assert!(
        error.contains("text is also missing; action=insert_after needs text, section"),
        "{error}"
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
    s.document_written = true;
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
fn inspecting_another_document_says_it_is_not_the_configured_output() {
    // A live run inspected the project's own user manual by path, took its
    // outdated links for its output's text, and spent its closing requests
    // editing passages its output no longer had.
    let (dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nText.\n"}),
    );
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(
        dir.path().join("docs/user-manual.md"),
        "# Manual\nOld link.\n",
    )
    .unwrap();
    let other = run(
        &mut s,
        "document_inspect",
        json!({"path":"docs/user-manual.md"}),
    );
    assert_eq!(other["configured_output"], false, "{other}");
    assert!(
        other["note"]
            .as_str()
            .is_some_and(|note| note.contains("not the configured output")),
        "{other}"
    );
    let own = run(&mut s, "document_inspect", json!({}));
    assert!(own.get("configured_output").is_none(), "{own}");
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

// Live run 2026-10-09: a model named the output by its file name joined to
// the project root 12 times and was told no project file had that name.
#[test]
fn the_output_file_name_at_the_project_root_names_the_output() {
    let (dir, mut s) = setup();
    let outside = tempfile::tempdir().unwrap();
    s.project.output = outside.path().join("generated.md");
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nText.\n"}),
    );
    let joined = dir.path().join("generated.md").display().to_string();
    let read = run(&mut s, "file_read", json!({"path":joined}));
    assert!(read.to_string().contains("Text."), "{read}");
    let inspected = run(&mut s, "document_inspect", json!({"path":joined}));
    assert!(inspected.to_string().contains("Guide"), "{inspected}");
    // The name in another directory is not the output.
    let error = tools::execute(&mut s, "file_read", json!({"path":"docs/generated.md"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("file_not_found"), "{error}");
}

// Live run 2026-10-09: a model paged document_inspect with offset alone five
// times (14 times in 12 runs) and document_audit without its revision twice.
#[test]
fn a_next_page_without_its_hash_continues_while_the_text_is_unchanged() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## One\nline one\n## Two\nline two\n"}),
    );
    let first = run(&mut s, "document_inspect", json!({}));
    let next = run(&mut s, "document_inspect", json!({"offset":1}));
    assert_eq!(next["hash"], first["hash"], "{next}");
    // After an edit the remembered hash names older text.
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"line two","text":"line 2"}),
    );
    let error = tools::execute(&mut s, "document_inspect", json!({"offset":1}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("document_hash_required"), "{error}");
    let error = tools::execute(&mut s, "document_audit", json!({"offset":1}))
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("document_audit_revision_required"),
        "{error}"
    );
    let audit = run(&mut s, "document_audit", json!({"limit":1}));
    let page = run(&mut s, "document_audit", json!({"offset":1,"limit":1}));
    assert_eq!(page["revision"], audit["revision"], "{page}");
}

#[test]
fn an_audit_given_the_output_path_audits_the_output() {
    // A live model named the configured output in document_audit's path and
    // was refused as an unknown argument.
    let (dir, mut s) = source_setup();
    let outside = tempfile::tempdir().unwrap();
    s.project.output = outside.path().join("generated.md");
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nText.\n"}),
    );
    let output = s.project.output.display().to_string();
    // The same file through a symlinked directory (/var is /private/var on
    // macOS) counts too.
    let real = s
        .project
        .output
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    for path in [output.as_str(), real.as_str(), "generated.md", ""] {
        let audit = run(&mut s, "document_audit", json!({"path":path}));
        assert_eq!(audit["structural_ok"], true, "{path}: {audit}");
    }
    // Another path is still not something the audit reads.
    std::fs::write(dir.path().join("other.md"), "# Other\n").unwrap();
    let error = tools::execute(&mut s, "document_audit", json!({"path":"other.md"}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("unknown_argument"), "{error}");
}

#[test]
fn inspecting_the_output_by_path_before_the_first_write_reports_it_missing() {
    // A live model named the configured output's path before writing it and
    // got file_not_found, while a call without a path reports exists:false.
    let (dir, mut s) = setup();
    let outside = tempfile::tempdir().unwrap();
    s.project.output = outside.path().join("generated.md");
    let output = s.project.output.display().to_string();
    for path in [output.as_str(), "generated.md"] {
        let inspected = run(&mut s, "document_inspect", json!({"path":path}));
        assert_eq!(inspected["exists"], false, "{path}: {inspected}");
    }
    // An output inside the project, named relative to its root.
    s.project.output = dir.path().join("docs/manual.md");
    let inspected = run(&mut s, "document_inspect", json!({"path":"docs/manual.md"}));
    assert_eq!(inspected["exists"], false, "{inspected}");
    // Any other missing path is still an error.
    assert!(
        tools::execute(&mut s, "document_inspect", json!({"path":"missing.md"}))
            .unwrap_err()
            .to_string()
            .contains("file_not_found")
    );
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
    // The blank first line was delivered, but the cited second line was not.
    assert!(
        !blank["source"]["excerpt"]
            .as_str()
            .unwrap()
            .trim()
            .is_empty()
            || id.is_string()
    );
    let audit = run(&mut s, "document_audit", json!({}));
    assert!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unread_citation"
                && issue["path"] == "main.rs"
                && issue["start_line"] == 2
                && issue["end_line"] == 2),
        "{audit}"
    );
}

#[test]
fn settings_validate_reserves() {
    let (_dir, mut s) = setup();
    s.config.writing_reserve_ratio = 0.2;
    assert!(s.config.validate().is_err());
    s.config.writing_reserve_ratio = 0.5;
    assert!(s.config.validate().is_ok());
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
    let (_dir, mut s) = source_setup();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n<!-- Example citation: missing.rs:9999 -->\n<!--\n```md\nmissing.rs:9999\n-->\n## Real\nText.\n"}),
    );
    assert_eq!(result["citation_check"]["citations_checked"], 0, "{result}");
    assert_eq!(result["citation_check"]["issue_count"], 0);
}

#[test]
fn document_audit_detects_list_item_fences_that_swallow_prose_and_citations() {
    let (dir, mut s) = source_setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n1. ```chart code block instructions\n   [not a link](missing.rs#L999)\n2. ```mermaid code block instructions\n   [not a link](missing.rs#L999)\n3. See [source](source.rs#L1).\n"}),
    );
    let check = &result["citation_check"];
    assert_eq!(check["citations_checked"], 1, "{check}");
    let fence_lines = check["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["kind"] == "unclosed_code_fence")
        .map(|issue| issue["line"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(fence_lines, [2, 4], "{check}");
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], false, "{audit}");
    assert_eq!(audit["citations_checked"], 1, "{audit}");
    assert_eq!(audit["issues"][0]["kind"], "unclosed_code_fence");
}

#[test]
fn closed_list_item_fences_keep_example_citations_out_of_the_audit() {
    let (dir, mut s) = source_setup();
    std::fs::write(dir.path().join("source.rs"), "fn source() {}\n").unwrap();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n1. ```js\n   missing.rs:999\n   ```\n2. ```mermaid\n   A[source.rs:1]\n   ```\n3. See source.rs:1.\n"}),
    );
    let check = &result["citation_check"];
    assert_eq!(check["citations_checked"], 2, "{check}");
    assert_eq!(check["issue_count"], 0, "{check}");
}

#[test]
fn a_link_target_error_names_its_folder_and_the_citation_to_write() {
    // A live model copied a docs/ page's `../frontend/...#L` links into an
    // output outside the project, was told that relative paths use
    // project.root, and rewrote one link target six ways.
    let (dir, mut s) = source_setup();
    std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
    std::fs::write(dir.path().join("frontend/src/App.jsx"), "a\nb\nc\n").unwrap();
    let outside = tempfile::tempdir().unwrap();
    s.project.output = outside.path().join("generated.md");
    let root = dir.path().canonicalize().unwrap();
    let spelled = format!("../..{}/frontend/src/App.jsx#L2-L3", root.display());
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":format!(
            "# Guide\nOpen. [App](../frontend/src/App.jsx#L1-L2)\nSend. [App](frontend/src/App.jsx#L3)\nRoot. [App]({spelled})\nGone. [Gone](../gone.jsx#L1)\nText. frontend/src/App.jsx:1-3\n"
        )}),
    );
    let issues = saved["citation_check"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 4, "{issues:?}");
    let error = |citation: &str| {
        issues
            .iter()
            .find(|issue| issue["citation"] == citation)
            .and_then(|issue| issue["error"].as_str())
            .unwrap_or_else(|| panic!("{citation}: {issues:?}"))
            .to_owned()
    };
    for (citation, instead) in [
        (
            "../frontend/src/App.jsx#L1-L2",
            "cite it as frontend/src/App.jsx:1-2",
        ),
        (
            "frontend/src/App.jsx#L3",
            "cite it as frontend/src/App.jsx:3",
        ),
        (spelled.as_str(), "cite it as frontend/src/App.jsx:2-3"),
        ("../gone.jsx#L1", "path:start-end relative to project.root"),
    ] {
        let error = error(citation);
        assert!(
            error.contains("resolves from the output document's folder")
                && error.contains(instead)
                && !error.contains("Relative paths use project.root"),
            "{error}"
        );
    }
    // The same link from an output inside the project resolves.
    s.project.output = dir.path().join("docs/manual.md");
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    let inside = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nOpen. [App](../frontend/src/App.jsx#L1-L2)\n"}),
    );
    assert_eq!(inside["citation_check"]["issue_count"], 0, "{inside}");
}

#[test]
fn a_section_edit_without_a_heading_points_to_a_whole_document_write() {
    // A live model sent its complete text as action=section with section ""
    // and was told only that section must not be empty.
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nText.\n"}),
    );
    for args in [
        json!({"action":"section","text":"# Guide\nNew.\n","expected_hash":created["hash"],"section":"","expected_section_hash":""}),
        json!({"action":"section","text":"# Guide\nNew.\n","expected_hash":created["hash"]}),
    ] {
        let error = tools::execute(&mut s, "document_edit", args)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("To replace the whole document, use action=write"),
            "{error}"
        );
    }
}

#[test]
fn inline_code_comment_marker_does_not_hide_following_citation() {
    let (dir, mut s) = source_setup();
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
    let (dir, mut s) = source_setup();
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
fn section_replacement_appends_new_sibling_sections_after_it() {
    // Live run 2026-10-08: the model rewrote section 9 and appended section
    // 10 in the same text three times; each rejection re-sent thousands of
    // tokens. A following same-level heading the document lacks is a new
    // section after this one; one the document has would be duplicated.
    let (_dir, mut s) = setup();
    let original = "# Guide\n## 1.3 Part\nOriginal.\n## Next\nLater.\n";
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":original}),
    );
    let inspected = run(&mut s, "document_inspect", json!({"section":"## 1.3 Part"}));
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## 1.3 Part","expected_hash":created["hash"],"expected_section_hash":inspected["section_hash"],"text":"## 1.3 Part\nUpdated.\n## Next\nRewritten.\n"}),
    )
    .unwrap_err()
    .to_string();
    for detail in [
        "invalid_argument_value: section replacement cannot add a heading the document already has",
        "line 3 of text contains \"## Next\", which is at document line 4",
        "rewrite \"## Next\" with its own action=section edit",
    ] {
        assert!(error.contains(detail), "missing {detail:?}: {error}");
    }
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        original
    );
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## 1.3 Part","expected_hash":created["hash"],"expected_section_hash":inspected["section_hash"],"text":"## 1.3 Part\nUpdated.\n### Detail\nMore.\n## 1.4 Added\nNew.\n## 1.5 Also added\nNewer.\n"}),
    );
    assert_eq!(
        saved["inserted_sections"],
        json!(["## 1.4 Added", "## 1.5 Also added"])
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n## 1.3 Part\nUpdated.\n### Detail\nMore.\n## 1.4 Added\nNew.\n## 1.5 Also added\nNewer.\n## Next\nLater.\n"
    );
    // A plain rewrite reports no inserted sections.
    let inspected = run(
        &mut s,
        "document_inspect",
        json!({"section":"## 1.4 Added"}),
    );
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"## 1.4 Added","expected_hash":saved["hash"],"expected_section_hash":inspected["section_hash"],"text":"## 1.4 Added\nChanged.\n"}),
    );
    assert!(saved.get("inserted_sections").is_none(), "{saved}");
}

#[test]
fn section_replacement_cannot_insert_peer_or_ancestor_headings() {
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        let original = "# Guide\n## 1.3 Part\nOriginal.\n## Next\nLater.\n";
        let created = run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":original}),
        );
        let inspected = run(&mut s, "document_inspect", json!({"section":"## 1.3 Part"}));
        for (heading, level) in [("## 1.3.1 Detail", 2), ("# Ancestor", 1)] {
            let edit = json!({"action":"section","section":"## 1.3 Part","expected_section_hash":inspected["section_hash"],"text":format!("## 1.3 Part\nUpdated.\n### Valid child\nDetails.\n{heading}\nUnexpected.\n")});
            let (name, args) = if batch {
                (
                    "document_edit_batch",
                    json!({"expected_hash":created["hash"],"edits":[
                        {"action":"replace_text","old_text":"Later.","text":"Changed."},
                        edit
                    ]}),
                )
            } else {
                let mut edit = edit;
                edit["expected_hash"] = created["hash"].clone();
                ("document_edit", edit)
            };
            let error = tools::execute(&mut s, name, args).unwrap_err().to_string();
            for detail in [
                "invalid_argument_value: section replacement cannot add a sibling or ancestor heading".to_string(),
                "target \"## 1.3 Part\" is level 2".to_string(),
                format!("line 5 of text contains level-{level} heading {heading:?}"),
                "Child headings are allowed".to_string(),
                "change its prefix to ### (level 3)".to_string(),
                "not section numbering".to_string(),
                "A later level-2 heading that is not numbered as a child of this section and does not exist yet is inserted as a new section after this one".to_string(),
            ] {
                assert!(error.contains(&detail), "missing {detail:?}: {error}");
            }
            if batch {
                assert!(error.starts_with("document_batch_operation_failed: index=1;"));
                assert!(error.contains("no changes persisted"));
            }
            assert_eq!(
                std::fs::read_to_string(&s.project.output).unwrap(),
                original
            );
        }

        // Follow the error's advice with the same hashes after the rollback.
        let edit = json!({"action":"section","section":"## 1.3 Part","expected_section_hash":inspected["section_hash"],"text":"## 1.3 Part\nUpdated.\n### 1.3.1 Detail\nChild content.\n"});
        let result = if batch {
            run(
                &mut s,
                "document_edit_batch",
                json!({"expected_hash":created["hash"],"edits":[edit]}),
            )
        } else {
            let mut edit = edit;
            edit["expected_hash"] = created["hash"].clone();
            run(&mut s, "document_edit", edit)
        };
        let expected =
            "# Guide\n## 1.3 Part\nUpdated.\n### 1.3.1 Detail\nChild content.\n## Next\nLater.\n";
        assert_eq!(
            std::fs::read_to_string(&s.project.output).unwrap(),
            expected
        );
        assert_eq!(result["hash"], tools::hash(expected.as_bytes()));
    }
}

#[test]
fn section_replacement_at_level_six_does_not_suggest_a_level_seven_heading() {
    let (_dir, mut s) = setup();
    let original = "# Guide\n###### 1.3 Detail\nOriginal.\n";
    std::fs::write(&s.project.output, original).unwrap();
    let inspected = run(
        &mut s,
        "document_inspect",
        json!({"section":"###### 1.3 Detail"}),
    );
    // A heading numbered as this section's child cannot be a child here.
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"###### 1.3 Detail","expected_hash":inspected["hash"],"expected_section_hash":inspected["section_hash"],"text":"###### 1.3 Detail\nUpdated.\n###### 1.3.1 More\nExtra.\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("line 3 of text contains level-6 heading \"###### 1.3.1 More\""));
    assert!(error.contains("cannot have Markdown child headings"));
    assert!(error.contains("use paragraphs or lists"));
    assert!(!error.contains("#######"));
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        original
    );
    // A new level-6 sibling is a new section after this one.
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"section","section":"###### 1.3 Detail","expected_hash":inspected["hash"],"expected_section_hash":inspected["section_hash"],"text":"###### 1.3 Detail\nUpdated.\n###### 1.4 More\nExtra.\n"}),
    );
    assert_eq!(saved["inserted_sections"], json!(["###### 1.4 More"]));
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n###### 1.3 Detail\nUpdated.\n###### 1.4 More\nExtra.\n"
    );
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
fn delivered_nested_sections_with_repeated_titles_keep_their_coverage() {
    let (_dir, mut s) = setup();
    let body = "# Guide\r\n## Alpha\r\n### Shared\r\nFirst body.\r\n## Beta\r\n### Shared\r\nSecond body.\r\n";
    std::fs::write(&s.project.output, body).unwrap();
    let section = "# Guide\n## Beta\n### Shared";
    let delivered = deliver(&mut s, "document_inspect", json!({"section":section}), 4000);
    assert_eq!(delivered["status"], "ok", "{delivered}");
    assert_eq!(delivered["data"]["section_path"], section);
    assert_eq!(delivered["data"]["content"]["truncated"], false);
    let outline = run(&mut s, "document_inspect", json!({}));
    assert_eq!(outline["coverage"]["fully_read_lines"], 2, "{outline}");
    assert_eq!(
        outline["coverage"]["missing_ranges"],
        json!([{"start_line":1,"end_line":5}])
    );
    assert_eq!(outline["outline"][2]["fully_read"], false);
    assert_eq!(outline["outline"][4]["fully_read"], true);
}

#[test]
fn repeated_section_title_coverage_survives_limited_continuation_pages() {
    for newline in ["\n", "\r\n"] {
        let (dir, mut s) = setup();
        let section = "# Guide\n## Beta\n### Shared";
        let expected = format!("### Shared\n{}", "읽은 내용🙂\n".repeat(60)).replace('\n', newline);
        let body = format!(
            "{}{}",
            "# Guide\n## Alpha\n### Shared\nUnread.\n## Beta\n".replace('\n', newline),
            expected
        );
        std::fs::write(dir.path().join("input.md"), body).unwrap();
        let mut args = json!({"path":"input.md","section":section});
        let mut reconstructed = String::new();
        let mut pages = 0;
        for _ in 0..100 {
            let page = deliver(&mut s, "document_inspect", args, 500);
            assert_eq!(page["status"], "ok", "{page}");
            reconstructed.push_str(page["data"]["content"]["text"].as_str().unwrap());
            pages += 1;
            if page["data"]["content"]["truncated"] != true {
                break;
            }
            args = page["next_cursor"].clone();
            assert_eq!(args["section"], section);
            args.as_object_mut().unwrap().remove("tool");
        }
        assert!(pages > 1);
        assert_eq!(reconstructed, expected);
        let outline = run(&mut s, "document_inspect", json!({"path":"input.md"}));
        assert_eq!(outline["coverage"]["fully_read_lines"], 61, "{outline}");
        assert_eq!(
            outline["coverage"]["missing_ranges"],
            json!([{"start_line":1,"end_line":5}])
        );
        assert_eq!(outline["outline"][2]["fully_read"], false);
        assert_eq!(outline["outline"][4]["fully_read"], true);
    }
}

#[cfg(unix)]
#[test]
fn document_edits_preserve_existing_file_permissions() {
    use std::os::unix::fs::PermissionsExt;
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        let original = "# Guide\nOriginal body.\n";
        std::fs::write(&s.project.output, original).unwrap();
        std::fs::set_permissions(&s.project.output, std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let edit =
            json!({"action":"replace_text","old_text":"Original body.","text":"Updated body."});
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
            "# Guide\nUpdated body.\n"
        );
        assert_eq!(
            std::fs::metadata(&s.project.output)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
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
            // Live shape: a replacement without its trailing blank line must
            // not glue the next heading under a list.
            (
                "# Doc\n\n## Target\n\n- old\n\n## Next\n\nRest\n",
                "## Target\n\n- new",
                "# Doc\n\n## Target\n\n- new\n\n## Next\n\nRest\n",
            ),
            (
                "# Doc\r\n\r\n## Target\r\n- old\r\n\r\n## Next\r\n",
                "## Target\r\n- new\r\n",
                "# Doc\r\n\r\n## Target\r\n- new\r\n\r\n## Next\r\n",
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
fn batch_failure_names_every_failing_operation() {
    let (_dir, mut s) = setup();
    let original = "# Doc\n## Target\nalpha beta gamma\n";
    std::fs::write(&s.project.output, original).unwrap();
    let inspected = run(&mut s, "document_inspect", json!({}));
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":inspected["hash"],"edits":[
            {"action":"replace_text","old_text":"alpha","text":"ALPHA"},
            {"action":"replace_text","old_text":"delta","text":"DELTA"},
            {"action":"replace_text","old_text":"gamma","text":"GAMMA"},
            {"action":"replace_text","old_text":"beta gamma","text":"BG"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("document_batch_operation_failed: index=1; action=replace_text;"),
        "{error}"
    );
    assert!(error.contains("2 operations failed"), "{error}");
    assert!(error.contains("[index=3; action=replace_text;"), "{error}");
    // Hints still name the edit index that changed the text, past the
    // skipped failure.
    assert!(
        error.contains("edits[2] in this same batch already changed it"),
        "{error}"
    );
    assert!(error.ends_with("no changes persisted"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        original
    );
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
        assert!(
            error.starts_with("invalid_argument_value:"),
            "{name}: {error}"
        );
        assert!(error.contains("NUL"), "{name}: {error}");
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
    assert!(error.starts_with("invalid_argument_value:"));
    assert!(error.contains("NUL"));
    assert!(!s.project.output.exists());
}

#[test]
fn one_insertion_may_add_several_sections_of_its_level() {
    // The live-run shape: two level-3 sections in one insert_last_child,
    // refused three times in one run. They follow the anchor's existing
    // children in order, with their own nested headings.
    let body = "# Manual\n\n## 1. Start\n\nBody\n\n### 1-1. Old\n\nOld\n\n## 2. Next\n\nLater\n";
    for batch in [false, true] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let edit = json!({"action":"insert_last_child","section":"## 1. Start",
            "text":"### 1-2. Routes\n\nRoutes\n\n#### Detail\n\nMore\n\n### 1-3. Worker\n\nWorker\n"});
        if batch {
            run(
                &mut s,
                "document_edit_batch",
                json!({"expected_hash":tools::hash(body.as_bytes()),"edits":[edit]}),
            );
        } else {
            let mut edit = edit;
            edit["expected_hash"] = json!(tools::hash(body.as_bytes()));
            run(&mut s, "document_edit", edit);
        }
        assert_eq!(
            std::fs::read_to_string(&s.project.output).unwrap(),
            "# Manual\n\n## 1. Start\n\nBody\n\n### 1-1. Old\n\nOld\n\n### 1-2. Routes\n\nRoutes\n\n#### Detail\n\nMore\n\n### 1-3. Worker\n\nWorker\n\n## 2. Next\n\nLater\n"
        );
    }
    // A later inserted heading that already exists under the parent is
    // named as the duplicate, and nothing is written.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_before","section":"## 2. Next","expected_hash":tools::hash(body.as_bytes()),
            "text":"## 1-9. New\nNew\n## 1. Start\nAgain\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with(
            "invalid_argument_value: insert_before text adds \"## 1. Start\", which already exists"
        ),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
}

#[test]
fn section_insert_errors_name_the_heading_that_breaks_the_rule() {
    let body = "# Manual\n## 1. Start\nBody\n";
    let cases = [
        // A heading above the inserted level would leave the parent.
        (
            "### 1-1. Overview\ntext\n## 2. Next\nmore\n",
            "line 3 of text starts level-2 heading \"## 2. Next\", above that level",
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
    // Section insertions follow the same rule: right after the model's own
    // write the hash is known, after an outside change it is required.
    let hash = tools::hash(b"# Guide\nChanged elsewhere.\n");
    run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"## Three\n"}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"## Three","text":"## Four\n"}),
    );
    std::fs::write(&s.project.output, "# Guide\n## Three\n").unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"## Three","text":"## Four\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("document_hash_required"), "{error}");
}

#[test]
fn a_missing_hash_is_reported_with_the_edits_other_problems() {
    let (_dir, mut s) = setup();
    let body = "# Guide\n\n## Install\n\nSteps.\n";
    // Seeded outside the tools, so the model's own last write does not cover it.
    std::fs::write(&s.project.output, body).unwrap();
    // The live shape: a sibling H2 sent as a child, without a hash. One
    // response names both problems and how to place the heading.
    let call = |s: &mut Session, args: Value| {
        tools::run_call(
            s,
            &mnemoarc::llm::ToolCall {
                id: "edit".into(),
                name: "document_edit".into(),
                arguments: args.to_string(),
            },
        )
    };
    let result = call(
        &mut s,
        json!({"action":"insert_last_child","section":"Install","text":"## Query\n"}),
    );
    let error = result["error"].as_str().unwrap();
    assert_eq!(
        result["recovery"]["code"], "document_hash_required",
        "{error}"
    );
    assert!(
        error.starts_with("document_hash_required: document_edit action=insert_last_child"),
        "{error}"
    );
    assert_eq!(
        result["recovery"]["tools"],
        json!(["document_inspect", "document_edit"])
    );
    assert!(
        error.contains("must start with a level-3 heading"),
        "{error}"
    );
    assert!(
        error.contains("use insert_after or insert_before"),
        "{error}"
    );
    assert!(error.contains("nothing was written"), "{error}");
    // A valid edit without a hash still names only the hash.
    let result = call(
        &mut s,
        json!({"action":"insert_after","section":"Install","text":"## Query\n"}),
    );
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("has no other problem"), "{error}");
    // Argument errors found before the run mention the missing hash too.
    let result = call(
        &mut s,
        json!({"action":"section","section":"Install","text":"## Install\n"}),
    );
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("missing_argument: expected_section_hash"),
        "{error}"
    );
    assert!(error.contains("expected_hash is also missing"), "{error}");
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    // A sent but stale hash is a revision conflict, not checked further.
    let stale = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"Install","expected_hash":tools::hash(b"old"),"text":"## Query\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(stale.starts_with("document_revision_conflict"), "{stale}");

    // A batch without a hash lists every failing operation as well.
    let batch = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"edits":[
            {"action":"replace_text","old_text":"Absent.","text":"x"},
            {"action":"insert_first_child","section":"Install","text":"### Run\n"},
            {"action":"insert_after","section":"Install","text":"### Query\n"}
        ]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        batch.starts_with("document_hash_required: document_edit_batch"),
        "{batch}"
    );
    assert!(batch.contains("its edits were also checked"), "{batch}");
    assert!(batch.contains("2 failed: [index=0;"), "{batch}");
    assert!(batch.contains("[index=2; action=insert_after"), "{batch}");
    assert!(
        batch.contains("use insert_last_child or insert_first_child"),
        "{batch}"
    );
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    // After the model's own write the batch hash is known and may be omitted.
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"Install","expected_hash":tools::hash(body.as_bytes()),"text":"## Query\n"}),
    );
    run(
        &mut s,
        "document_edit_batch",
        json!({"edits":[{"action":"insert_last_child","section":"Query","text":"### Rows\n"}]}),
    );
    assert!(
        std::fs::read_to_string(&s.project.output)
            .unwrap()
            .contains("### Rows")
    );
}

#[test]
fn a_full_rewrite_sent_as_an_insertion_replaces_the_anchor_once() {
    let body = "# 사용법\n**전송**: Enter 키 또는 ➤ 전송 단추로 보냅니다 (`App.jsx:1325-1327`).\n";
    let anchor = "**전송**: Enter 키 또는 ➤ 전송 단추로 보냅니다 (`App.jsx:1325-1327`).";
    for (batch, action) in [(false, "insert_after_text"), (true, "insert_before_text")] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        // The live shape: the "inserted" text restates the anchor to reword it.
        let edit = json!({"action":action,"old_text":anchor,"text":format!("{anchor} 빈 입력은 보낼 수 없습니다.")});
        if batch {
            run(
                &mut s,
                "document_edit_batch",
                json!({"expected_hash":tools::hash(body.as_bytes()),"edits":[edit]}),
            );
        } else {
            let mut edit = edit;
            edit["expected_hash"] = json!(tools::hash(body.as_bytes()));
            run(&mut s, "document_edit", edit);
        }
        let revised = std::fs::read_to_string(&s.project.output).unwrap();
        assert_eq!(revised.matches(anchor).count(), 1, "{revised}");
        assert!(revised.contains("빈 입력은 보낼 수 없습니다."));
    }
    // Multiple copies remain ambiguous and must not be persisted.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","expected_hash":tools::hash(body.as_bytes()),"old_text":anchor,"text":format!("{anchor}\n{anchor}")}),
    ).unwrap_err().to_string();
    assert!(error.contains("repeats old_text ambiguously"), "{error}");
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    // New text next to the anchor, and a short anchor that recurs inside a
    // longer line, still work.
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
        json!({"action":"insert_before_text","expected_hash":result["hash"],"old_text":"# 사용법","text":"# 사용법 요약\n"}),
    );
    assert!(
        std::fs::read_to_string(&s.project.output)
            .unwrap()
            .starts_with("# 사용법 요약\n# 사용법\n")
    );
}

#[test]
fn a_section_inserted_after_a_short_heading_that_it_repeats_goes_before_it() {
    // Live shape: to put a section before "## 2. ...", a model inserted the
    // section followed by that heading after the heading. The 15-character
    // heading was under the 20-character floor for a repeated anchor, so it
    // was kept and the document held "## 2. ..." twice.
    let body = "# Guide\n\n## 1. Intro\n\nText one.\n\n## 2. Start\n\nBody two.\n";
    let expected = "# Guide\n\n## 1. Intro\n\nText one.\n\n## 1-1. Panel\n\nPanel text.\n\n## 2. Start\n\nBody two.\n";
    for (batch, text) in [
        (false, "## 1-1. Panel\n\nPanel text.\n\n## 2. Start"),
        (true, "## 1-1. Panel\n\nPanel text.\n\n## 2. Start\n"),
    ] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let edit = json!({"action":"insert_after_text","old_text":"## 2. Start","text":text});
        if batch {
            run(
                &mut s,
                "document_edit_batch",
                json!({"expected_hash":tools::hash(body.as_bytes()),"edits":[edit]}),
            );
        } else {
            run(&mut s, "document_edit", edit);
        }
        assert_eq!(
            std::fs::read_to_string(&s.project.output).unwrap(),
            expected
        );
    }
    // A new heading right after a heading line, without repeating it, would
    // take over that section's text: refuse it and name the section actions.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","old_text":"## 2. Start","text":"\n## 1-1. Panel\n\nPanel text.\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("take over that text"), "{error}");
    assert!(error.contains("insert_before with section"), "{error}");
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    // Text that is not a heading may still follow a heading line.
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","old_text":"## 2. Start","text":"\n\nA lead sentence."}),
    );
    assert_eq!(result["status"].as_str().unwrap_or("ok"), "ok", "{result}");
    assert!(
        std::fs::read_to_string(&s.project.output)
            .unwrap()
            .contains("## 2. Start\n\nA lead sentence.")
    );
}

#[test]
fn block_insertion_beside_a_mid_line_anchor_is_rejected() {
    let body = "# 실행\n- Oracle에 관해서는 두 가지 경우가 다릅니다. docker가 없으면 건너뜁니다.\n";
    // Live shape: a list item inserted after a sentence that ends mid-line.
    for (action, old_text, text) in [
        (
            "insert_after_text",
            "- Oracle에 관해서는 두 가지 경우가 다릅니다.",
            "- MariaDB는 필수입니다.\n",
        ),
        (
            "insert_after_text",
            "두 가지 경우가 다릅니다.",
            "- MariaDB는 필수입니다.",
        ),
        (
            "insert_before_text",
            "docker가 없으면",
            "\n- 컨테이너가 없으면 건너뜁니다.",
        ),
    ] {
        let (_dir, mut s) = setup();
        std::fs::write(&s.project.output, body).unwrap();
        let error = tools::execute(
            &mut s,
            "document_edit",
            json!({"action":action,"expected_hash":tools::hash(body.as_bytes()),"old_text":old_text,"text":text}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("in the middle of a line"), "{error}");
        assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    }
    // Inline text mid-line and a line after a whole-line anchor still work.
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let result = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","expected_hash":tools::hash(body.as_bytes()),"old_text":"다릅니다.","text":" 아래를 보십시오."}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","expected_hash":result["hash"],"old_text":"docker가 없으면 건너뜁니다.","text":"\n- MariaDB는 필수입니다."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# 실행\n- Oracle에 관해서는 두 가지 경우가 다릅니다. 아래를 보십시오. docker가 없으면 건너뜁니다.\n- MariaDB는 필수입니다.\n"
    );
}

#[test]
fn block_insertion_keeps_its_far_edge_off_the_neighboring_line() {
    let body = "# 매뉴얼\n\n소개 문장입니다.\n\n## 1. 실행\n\n본문입니다.\n";
    // Live shape: a section inserted before a heading anchor without a
    // trailing newline became "...(README.md:27-28).## 1. 실행과 화면 접속".
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let result = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":tools::hash(body.as_bytes()),"edits":[
            {"action":"insert_before_text","old_text":"## 1. 실행","text":"## 구조\n\n구조 설명입니다."},
            {"action":"insert_after_text","old_text":"소개 문장입니다.","text":"\n- 추가 항목입니다."}
        ]}),
    );
    let expected = "# 매뉴얼\n\n소개 문장입니다.\n- 추가 항목입니다.\n\n## 구조\n\n구조 설명입니다.\n\n## 1. 실행\n\n본문입니다.\n";
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected
    );
    // A heading inserted after a line-ending anchor starts on its own line
    // and keeps the blank line before it.
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","expected_hash":result["hash"],"old_text":"본문입니다.","text":"## 2. 종료\n종료 설명입니다."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        expected.replace(
            "본문입니다.\n",
            "본문입니다.\n\n## 2. 종료\n종료 설명입니다.\n"
        )
    );
}

#[test]
fn broken_characters_are_rejected_before_any_document_write() {
    // Live run 2026-10-04: an append wrote "객��" (객체); the reviewer could not
    // quote the line, and the broken word survived into the approved document.
    let body = "# 매뉴얼\n\n객체 브라우저 설명입니다.\n";
    let (_dir, mut s) = setup();
    std::fs::write(&s.project.output, body).unwrap();
    let hash = tools::hash(body.as_bytes());
    for (name, args, field) in [
        (
            "document_edit",
            json!({"action":"append","expected_hash":hash,"text":"## 5. 객체\n\n지원하는 객\u{FFFD}\u{FFFD}에만 나타납니다."}),
            "text",
        ),
        (
            "document_edit_batch",
            json!({"expected_hash":hash,"edits":[
                {"action":"replace_text","old_text":"설명입니다.","text":"안내입니다."},
                {"action":"replace_text","old_text":"객체","text":"객\u{FFFD}"}
            ]}),
            "edits[1].text",
        ),
    ] {
        let error = match tools::execute(&mut s, name, args) {
            Ok(value) => value.to_string(),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(&format!("{field} contains U+FFFD")),
            "{error}"
        );
        assert!(
            error.contains("지원하는 객") || error.contains("객\u{FFFD}"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
    }
    // An exact old_text may still quote a broken word to repair it.
    let broken = "# 매뉴얼\n\n객\u{FFFD} 브라우저 설명입니다.\n";
    std::fs::write(&s.project.output, broken).unwrap();
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"객\u{FFFD}","text":"객체"}),
    );
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), body);
}

#[test]
fn empty_document_placeholders_do_not_block_create_or_batch_edits() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","expected_hash":"","expected_section_hash":"","old_text":"","section":"","text":"# Guide\nA.\n"}),
    );
    let edited = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":created["hash"],"edits":[
            {"action":"append","text":"## Details\nB.\n","old_text":"","section":"","expected_section_hash":""}
        ]}),
    );
    assert_eq!(edited["operation_count"], 1);
    assert!(
        std::fs::read_to_string(&s.project.output)
            .unwrap()
            .contains("## Details\nB.")
    );
}

#[test]
fn reading_the_output_before_it_exists_says_to_create_it() {
    let (_dir, mut s) = setup();
    // The live shape: reads and an audit of the output before any write.
    let output = s.project.output.display().to_string();
    let error = tools::execute(&mut s, "file_read", json!({"path":output,"max_lines":10}))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("file_not_found"), "{error}");
    assert!(
        error.contains("The configured output does not exist yet"),
        "{error}"
    );
    // document_inspect reports the missing output as a call without a path
    // does, with the same advice.
    let inspected = run(&mut s, "document_inspect", json!({"path":output}));
    assert_eq!(inspected["exists"], false, "{inspected}");
    assert!(
        inspected["guidance"]
            .as_str()
            .is_some_and(|text| text.contains("document_edit action=create")),
        "{inspected}"
    );
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
}

#[test]
fn a_user_selected_workflow_applies_to_each_request_and_is_locked() {
    let (_dir, mut s) = setup();
    s.active_tools.clear();
    s.workflow_mode = "source_document".into();
    s.add_user("Write the manual.".into());
    assert_eq!(s.task.workflow, "source_document");
    assert!(s.is_document_work());
    assert!(s.active_tools.contains("document_audit") && s.active_tools.contains("document_edit"));
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
    assert!(!s.is_document_work());
}

#[test]
fn a_directory_path_is_a_targeted_listing_while_verifying() {
    let (dir, mut s) = setup();
    std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
    std::fs::write(dir.path().join("frontend/src/App.jsx"), "x\n").unwrap();
    s.document_written = true;
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

#[test]
fn hash_copy_with_a_dropped_span_names_the_loss_and_fresh_inspect_ignores_it() {
    let (_dir, mut s) = setup();
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\nFirst line.\n"}),
    );
    let current = written["hash"].as_str().unwrap().to_owned();
    let corrupted = format!("{}{}", &current[..41], &current[46..]);
    let err = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":corrupted,"edits":[{"action":"append","text":"More.\n"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains(&format!("{:?} missing", &current[41..46])),
        "{err}"
    );
    assert!(err.contains(&current), "{err}");
    // A fresh read returns the current hash even with a placeholder hash.
    let page = run(
        &mut s,
        "document_inspect",
        json!({"expected_hash":corrupted,"offset":0,"coverage_offset":0}),
    );
    assert_eq!(page["hash"], current);
    let err = tools::execute(
        &mut s,
        "document_inspect",
        json!({"expected_hash":corrupted,"offset":1}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("document_revision_conflict"), "{err}");
}

#[test]
fn delete_text_ignores_a_filled_text_but_not_a_real_replacement() {
    let (_dir, mut s) = setup();
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\nKeep.\n\nDrop me.\n"}),
    );
    let hash = written["hash"].as_str().unwrap().to_owned();
    let err = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[{"action":"delete_text","old_text":"\nDrop me.\n","text":"Other"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("not valid for action=delete_text"), "{err}");
    let deleted = run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[{"action":"delete_text","old_text":"\nDrop me.\n","text":"\nDrop me.\n"}]}),
    );
    let doc = std::fs::read_to_string(&s.project.output).unwrap();
    assert!(!doc.contains("Drop me."), "{doc}");
    run(
        &mut s,
        "document_edit",
        json!({"action":"delete_text","expected_hash":deleted["hash"],"old_text":"Keep.","text":""}),
    );
    assert!(
        !std::fs::read_to_string(&s.project.output)
            .unwrap()
            .contains("Keep.")
    );
}

#[test]
fn source_documentation_never_writes_project_files_and_append_creates_output() {
    let (dir, mut s) = setup();
    s.select_workflow("source_document").unwrap();
    let names: Vec<_> = tools::ToolRegistry::definitions(&s)
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    for withheld in ["file_edit", "file_write", "file_patch"] {
        assert!(!names.iter().any(|name| name == withheld), "{names:?}");
    }
    let err = tools::execute(
        &mut s,
        "file_write",
        json!({"path":"manual-output.md","content":"# Stray\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.starts_with("workflow_write_scope:"), "{err}");
    assert!(!dir.path().join("manual-output.md").exists());
    // Luna's first write: append with every placeholder filled, before any
    // output exists.
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":"","expected_section_hash":"","old_text":"","section":"","text":"# Manual\n\nFirst.\n"}),
    );
    assert!(created["hash"].is_string(), "{created}");
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Manual\n\nFirst.\n"
    );
}

#[test]
fn retyped_old_text_with_a_repeated_opening_shows_the_passage_to_copy() {
    let (_dir, mut s) = setup();
    let body = "# Guide\n\n첫 화면에는 환영 문구가 표시됩니다. 추천 질문 칩을 누르면 입력란에 반영됩니다.\n\n첫 화면에는 환영 문구가 표시됩니다. 홈 버튼으로 돌아옵니다.\n";
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":body}),
    );
    // The opening occurs twice and a middle phrase was dropped, so there is
    // no single divergence point.
    let err = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":written["hash"],"edits":[{"action":"replace_text","old_text":"첫 화면에는 환영 문구가 표시됩니다. 칩을 누르면 입력란에 반영됩니다.","text":"바뀜"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("its start and end match this passage"),
        "{err}"
    );
    assert!(
        err.contains("추천 질문 칩을 누르면 입력란에 반영됩니다."),
        "{err}"
    );
}

#[test]
fn inserted_sections_follow_blank_line_heading_spacing() {
    let (_dir, mut s) = setup();
    std::fs::write(s.project.root.join("main.js"), "function run() {}\n").unwrap();
    run(&mut s, "file_read", json!({"path":"main.js"}));
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n\n## Overview\n\nStart. main.js:1\n\n## Errors\n\nFailures.\n"}),
    );
    let inserted = run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"## Errors","expected_hash":created["hash"],"text":"## Flow\n\nSteps."}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after","section":"## Overview","expected_hash":inserted["hash"],"text":"## Setup\n\nInstall."}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n\n## Overview\n\nStart. main.js:1\n\n## Setup\n\nInstall.\n\n## Errors\n\nFailures.\n\n## Flow\n\nSteps.\n"
    );
    // The blank line added after the cited section is layout only: the
    // citation stays read.
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
}

#[test]
fn replace_text_ignores_whitespace_around_old_text() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n- First part. Loading fails\n  quietly here.\n"}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":created["hash"],"old_text":"  Loading fails\n  quietly here.\n","text":"  Loading errors\n  appear at the top.\n"}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n- First part. Loading errors\n  appear at the top.\n"
    );
}

#[test]
fn text_edits_recover_table_rows_copied_with_their_line_number_pipe() {
    // Live run 2026-10-08: file_read shows a table row as `57|| a | b |`;
    // the model sent `|| a | b |` as old_text, with the JSON's `\"` copied
    // as a literal backslash, and failed 21 edits (12 on one row it never
    // fixed).
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n| Page | Shown |\n|---|---|\n| **settings** | `page === \"settings\"` |\n| **projects** | `page === \"projects\"` |\n"}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":created["hash"],"old_text":"|| **settings** | `page === \\\"settings\\\"` |","text":"|| **settings** | 설정 화면 |"}),
    );
    let saved = run(
        &mut s,
        "document_edit_batch",
        json!({"edits":[{"action":"replace_text","old_text":"|| **projects** | `page === \"projects\"` |\n","text":"|| **projects** | 프로젝트 화면 |\n|| **chat** | 채팅 화면 |\n"}]}),
    );
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\n| Page | Shown |\n|---|---|\n| **settings** | 설정 화면 |\n| **projects** | 프로젝트 화면 |\n| **chat** | 채팅 화면 |\n"
    );
    // A document that has `\"` keeps it: only an absent passage is
    // recovered, and only as a single exact match.
    let quoted = "# Guide\n```js\nconst a = \"x \\\"y\\\"\";\n```\n| **a** | \"b\" |\n";
    let saved = run(
        &mut s,
        "document_edit",
        json!({"action":"write","expected_hash":saved["hash"],"text":quoted}),
    );
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":saved["hash"],"old_text":"| **a** | \\\"b\\\" |","text":"| **a** | c |"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("patch_target_must_match_once"), "{error}");
    assert_eq!(std::fs::read_to_string(&s.project.output).unwrap(), quoted);
}

#[test]
fn appended_heading_follows_the_document_heading_spacing() {
    let cases = [
        // Spaced headings: one blank line whatever the document ends with.
        (
            "# Guide\n\n## One\nText.\n",
            "# Guide\n\n## One\nText.\n\n## Two\nMore.\n",
        ),
        (
            "# Guide\n\n## One\nText.",
            "# Guide\n\n## One\nText.\n\n## Two\nMore.\n",
        ),
        (
            "# Guide\n\n## One\nText.\n\n",
            "# Guide\n\n## One\nText.\n\n## Two\nMore.\n",
        ),
        (
            "# Guide\r\n\r\n## One\r\nText.\r\n",
            "# Guide\r\n\r\n## One\r\nText.\r\n\r\n## Two\nMore.\n",
        ),
        // With only a title so far, the line after it shows the spacing: a
        // live manual began "# Title\n\nIntro" and its first "## ..." was
        // glued to the intro.
        ("# Guide\nIntro.\n", "# Guide\nIntro.\n## Two\nMore.\n"),
        (
            "# Guide\n\nIntro.\n\n1. Step\nSources: a.js\n",
            "# Guide\n\nIntro.\n\n1. Step\nSources: a.js\n\n## Two\nMore.\n",
        ),
        ("# Guide\n", "# Guide\n## Two\nMore.\n"),
        // Headings written without blank lines stay that way.
        (
            "# Guide\n## One\nText.\n",
            "# Guide\n## One\nText.\n## Two\nMore.\n",
        ),
    ];
    for (original, expected) in cases {
        let (dir, mut s) = setup();
        std::fs::write(dir.path().join("summary.md"), original).unwrap();
        let hash = tools::hash(original.as_bytes());
        run(
            &mut s,
            "document_edit",
            json!({"action":"append","expected_hash":hash,"text":"## Two\nMore.\n"}),
        );
        let written = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
        assert_eq!(written, expected, "{original:?}");
    }
    // Appended prose still joins the document as given.
    let (dir, mut s) = setup();
    std::fs::write(dir.path().join("summary.md"), "# Guide\n\n## One\nText.\n").unwrap();
    let hash = tools::hash(b"# Guide\n\n## One\nText.\n");
    run(
        &mut s,
        "document_edit",
        json!({"action":"append","expected_hash":hash,"text":"More.\n"}),
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("summary.md")).unwrap(),
        "# Guide\n\n## One\nText.\nMore.\n"
    );
}

#[test]
fn anchored_text_edits_accept_an_omitted_hash_but_check_a_supplied_one() {
    let (_dir, mut s) = setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nfirst line\nlast line\n"}),
    );
    // The exact old_text match is the precondition, so no hash is needed.
    for edit in [
        json!({"action":"replace_text","old_text":"first line","text":"opening line"}),
        json!({"action":"patch","old_text":"opening line","text":"start line"}),
        json!({"action":"insert_after_text","old_text":"start line\n","text":"middle line\n"}),
        json!({"action":"insert_before_text","old_text":"last line","text":"extra line\n"}),
        json!({"action":"delete_text","old_text":"extra line\n"}),
    ] {
        run(&mut s, "document_edit", edit);
    }
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\nstart line\nmiddle line\nlast line\n"
    );
    // Text that is no longer in the document still fails, so a stale view
    // cannot silently apply.
    let stale = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","old_text":"first line","text":"x"}),
    )
    .unwrap_err()
    .to_string();
    assert!(stale.contains("patch_target_must_match_once"), "{stale}");
    // A supplied hash is still the optimistic lock.
    let conflict = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":"0".repeat(64),"old_text":"start line","text":"x"}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        conflict.contains("document_revision_conflict"),
        "{conflict}"
    );
    // A blank hash is a filled placeholder and reads as omitted: a live
    // provider sent expected_hash "" and was told it must not be empty. The
    // anchored edit needs no hash; a structural edit and a batch, as with an
    // omitted hash, take the hash of the model's own last write.
    run(
        &mut s,
        "document_edit",
        json!({"action":"replace_text","expected_hash":"","section":"","expected_section_hash":"","old_text":"start line","text":"opening line"}),
    );
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","expected_hash":"","section":"# Guide","text":"## More\n"}),
    );
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":"","edits":[{"action":"replace_text","expected_hash":"","old_text":"opening line","text":"start line"}]}),
    );
    // Nor is a blank copy in an edit a conflict with a real top-level hash.
    let current = tools::hash(&std::fs::read(&s.project.output).unwrap());
    run(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":current,"edits":[{"action":"replace_text","expected_hash":"","old_text":"middle line","text":"center line"}]}),
    );
    let saved = std::fs::read_to_string(&s.project.output).unwrap();
    assert!(
        saved.starts_with("# Guide\nstart line\ncenter line\nlast line\n")
            && saved.contains("## More"),
        "{saved}"
    );
    // After a change by someone else there is no own hash to take, and the
    // blank hash is reported as missing, not as an empty value.
    std::fs::write(&s.project.output, "# Guide\nedited elsewhere\n").unwrap();
    let blank = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","expected_hash":"","section":"# Guide","text":"## Later\n"}),
    )
    .unwrap_err()
    .to_string();
    assert!(blank.starts_with("document_hash_required"), "{blank}");
    assert_eq!(
        std::fs::read_to_string(&s.project.output).unwrap(),
        "# Guide\nedited elsewhere\n"
    );
}

#[test]
fn structural_inserts_given_old_text_name_the_passage_anchored_action() {
    let (_dir, mut s) = setup();
    let created = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Setup\nInstall it.\n"}),
    );
    let hash = created["hash"].as_str().unwrap().to_owned();
    // A live run sent old_text to insert_before and later insert_after; the
    // error listed allowed arguments but never named insert_*_text.
    for action in ["insert_before", "insert_after"] {
        let error = tools::execute(
            &mut s,
            "document_edit",
            json!({"action":action,"section":"# Guide\n## Setup","old_text":"Install it.","text":"Note.\n","expected_hash":hash}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with(&format!(
                "invalid_action_arguments: document_edit action={action} does not accept old_text"
            )),
            "{error}"
        );
        assert!(
            error.contains("drop old_text and name that heading in section"),
            "{error}"
        );
        assert!(
            error.contains(&format!("use action={action}_text with old_text")),
            "{error}"
        );
    }
    let error = tools::execute(
        &mut s,
        "document_edit",
        json!({"action":"insert_last_child","section":"# Guide","old_text":"Install it.","text":"## Use\n","expected_hash":hash}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("insert_before_text or insert_after_text"),
        "{error}"
    );
    let error = tools::execute(
        &mut s,
        "document_edit_batch",
        json!({"expected_hash":hash,"edits":[{"action":"insert_after","section":"# Guide\n## Setup","old_text":"Install it.","text":"## Use\n"}]}),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("edits[0].old_text is not valid for action=insert_after; insert_after inserts a new section"),
        "{error}"
    );
    // The named action accepts the same call.
    run(
        &mut s,
        "document_edit",
        json!({"action":"insert_after_text","old_text":"Install it.","text":" Then run it.","expected_hash":hash}),
    );
}

#[test]
fn unread_citations_follow_delivered_lines_and_file_versions_and_merge_ranges() {
    let (dir, mut s) = source_setup();
    std::fs::write(
        dir.path().join("a.rs"),
        (1..=12).map(|n| format!("line {n}\n")).collect::<String>(),
    )
    .unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
    // Only a.rs:1-4 is delivered before the save.
    run(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":1,"max_lines":4}),
    );
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nA. a.rs:2-3\nB. a.rs:5-6\nC. a.rs:7-8\nD. b.rs:1\n"}),
    );
    // The save reports what was cited without being read; adjacent gaps of
    // one file merge into one range.
    let check = &written["citation_check"];
    assert_eq!(check["unread_citation_count"], 2, "{written}");
    let unread = check["unread_citations"].as_array().unwrap();
    assert!(unread.iter().any(|range| range["path"] == "a.rs"
        && range["start_line"] == 5
        && range["end_line"] == 8));
    assert!(unread.iter().any(|range| range["path"] == "b.rs"
        && range["start_line"] == 1
        && range["end_line"] == 1));
    // With the document review on, the audit lists the same ranges without
    // blocking completion: the review compares cited ranges with the source.
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
    // Without a review the audit blocks completion on them.
    s.config.source_document_review = false;
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], false, "{audit}");
    assert_eq!(
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|issue| issue["kind"] == "unread_citation")
            .count(),
        2
    );
    // Reading the listed ranges resolves them without a bookkeeping call.
    run(
        &mut s,
        "file_read",
        json!({"path":"a.rs","start_line":5,"max_lines":4}),
    );
    run(&mut s, "file_read", json!({"path":"b.rs"}));
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
    assert!(tools::unread_citations(&s).unwrap().is_empty());
    // A changed file invalidates the earlier reads of that file only.
    std::fs::write(dir.path().join("b.rs"), "fn b() { changed() }\n").unwrap();
    let unread = tools::unread_citations(&s).unwrap();
    assert_eq!(unread.len(), 1, "{unread:?}");
    assert_eq!(unread[0]["path"], "b.rs");
}

#[test]
fn a_long_unread_citation_suggests_citing_entry_lines() {
    let (dir, mut s) = source_setup();
    std::fs::write(
        dir.path().join("App.jsx"),
        (1..=130).map(|n| format!("row {n}\n")).collect::<String>(),
    )
    .unwrap();
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Screen\nThe screen lives in App.jsx:1-130\n"}),
    );
    let unread = &written["citation_check"]["unread_citations"][0];
    assert_eq!(unread["path"], "App.jsx");
    assert!(
        unread["note"].as_str().unwrap().contains("entry lines"),
        "{written}"
    );
}

#[test]
fn a_save_names_broad_citations_while_the_review_is_on() {
    // A 62-line document citing ten 2,000-line ranges took 35 review
    // requests; reading those ranges first left the writer no notice.
    let (dir, mut s) = source_setup();
    std::fs::write(
        dir.path().join("a.rs"),
        (1..=300)
            .map(|n| format!("let v{n} = {n};\n"))
            .collect::<String>(),
    )
    .unwrap();
    let text = "# Guide\nAll values. a.rs:1-200\nOne value. a.rs:10-12\n";
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":text}),
    );
    let check = &written["citation_check"];
    assert_eq!(check["broad_citation_count"], 1, "{check}");
    assert_eq!(check["broad_cited_lines"], 200);
    assert_eq!(
        check["broad_citations"],
        json!([{"citation":"a.rs:1-200","document_line":2,"lines":200}])
    );
    assert!(
        check["broad_citation_guidance"]
            .as_str()
            .unwrap()
            .contains("document review sends every cited line")
    );
    // Without the review a long range costs no review requests; an unread
    // one keeps its own note.
    s.config.source_document_review = false;
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"write","text":format!("{text}\n"),"expected_hash":written["hash"]}),
    );
    assert!(
        written["citation_check"]
            .get("broad_citation_count")
            .is_none(),
        "{written}"
    );
}

#[test]
fn a_self_citation_of_the_output_is_never_owed_a_read() {
    let (dir, mut s) = source_setup();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    run(&mut s, "file_read", json!({"path":"a.rs"}));
    // The output cites itself beside a real source; only the source counts.
    let written = run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\nSee summary.md:1 and a.rs:1\n"}),
    );
    assert_eq!(
        written["citation_check"]["unread_citation_count"], 0,
        "{written}"
    );
    let audit = run(&mut s, "document_audit", json!({}));
    assert_eq!(audit["structural_ok"], true, "{audit}");
}

#[test]
fn a_blank_section_reads_the_outline() {
    let (_dir, mut s) = source_setup();
    run(
        &mut s,
        "document_edit",
        json!({"action":"create","text":"# Guide\n## Start\nbody\n"}),
    );
    // The live shape: every optional field filled with an empty value.
    let page = run(
        &mut s,
        "document_inspect",
        json!({"path":"","section":"","offset":0,"limit":30,"coverage_offset":0,"expected_hash":"","expected_coverage_revision":""}),
    );
    assert_eq!(page["outline"].as_array().map(Vec::len), Some(2), "{page}");
}
