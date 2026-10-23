//! Regressions from the 2026-10-14 review of the model-facing tools: each
//! test reproduces a defect a reviewer confirmed (a panic, a continuation
//! that repeated or lost its page, a section edit that dropped the next
//! section, a citation the checks never saw, a call that failed a
//! checkpoint without running).

mod reads {
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

    /// whole_word wraps the query in \b...\b. A query that ends (or starts)
    /// with punctuation then needs a WORD character after (before) it, so the
    /// exact whole token never matches.
    #[test]
    fn whole_word_query_ending_in_punctuation_matches_itself() {
        let (dir, mut s) = setup();
        write(&dir, "a.rs", "fn main() {\n    load();\n}\n");
        let r = tools::execute(
            &mut s,
            "source_search",
            json!({"query":"load()","whole_word":true}),
        )
        .unwrap();
        assert_eq!(r["total_matching_lines"], 1, "{r}");
    }

    /// One unreadable file makes the whole listing/search fail instead of
    /// being skipped.
    #[cfg(unix)]
    #[test]
    fn unreadable_file_does_not_fail_listing_and_search() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (dir, mut s) = setup();
        write(&dir, "a.rs", "fn hit() {}\n");
        write(&dir, "secret.rs", "fn hit() {}\n");
        let secret = dir.path().join("secret.rs");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listed = tools::execute(&mut s, "file_list", json!({}));
        let searched = tools::execute(&mut s, "source_search", json!({"query":"hit"}));
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listed = listed.map_err(|e| e.to_string());
        let searched = searched.map_err(|e| e.to_string());
        assert!(listed.is_ok(), "file_list: {listed:?}");
        assert!(searched.is_ok(), "source_search: {searched:?}");
    }

    /// An unreadable sibling directory makes an unscoped listing fail.
    #[cfg(unix)]
    #[test]
    fn unreadable_directory_does_not_fail_unscoped_listing() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (dir, mut s) = setup();
        write(&dir, "a.rs", "fn hit() {}\n");
        write(&dir, "private/b.rs", "fn hit() {}\n");
        let private = dir.path().join("private");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listed = tools::execute(&mut s, "file_list", json!({"mode":"paths"}));
        let searched = tools::execute(&mut s, "source_search", json!({"query":"hit"}));
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listed = listed.map_err(|e| e.to_string());
        let searched = searched.map_err(|e| e.to_string());
        assert!(listed.is_ok(), "file_list: {listed:?}");
        assert!(searched.is_ok(), "source_search: {searched:?}");
    }

    /// history search returns next_cursor, but the tool has no cursor argument:
    /// sending it back is rejected.
    #[test]
    fn history_search_next_cursor_can_be_sent_back() {
        let (_dir, mut s) = setup();
        for i in 0..3 {
            s.history.push(
                vec![json!({"role":"user","content":format!("needle {i}")})],
                true,
            );
        }
        let first = tools::execute(
            &mut s,
            "history",
            json!({"action":"search","query":"needle","limit":1}),
        )
        .unwrap();
        let cursor = first["next_cursor"].clone();
        assert!(cursor.is_u64(), "{first}");
        let next = tools::execute(
            &mut s,
            "history",
            json!({"action":"search","query":"needle","limit":1,"cursor":cursor}),
        )
        .map_err(|e| e.to_string());
        assert!(next.is_ok(), "{next:?}");
    }

    /// A provider that fills every field sends id:"" beside path; that blank id
    /// filters out every source, so a path lookup returns nothing.
    #[test]
    fn source_lookup_blank_id_is_not_a_filter() {
        let (dir, mut s) = setup();
        write(&dir, "src/a.rs", "fn a() {}\nfn b() {}\n");
        tools::execute(&mut s, "file_read", json!({"path":"src/a.rs"})).unwrap();
        let by_path = tools::execute(&mut s, "source_lookup", json!({"path":"src/a.rs"})).unwrap();
        assert_eq!(by_path["total"], 1, "{by_path}");
        let filled =
            tools::execute(&mut s, "source_lookup", json!({"path":"src/a.rs","id":""})).unwrap();
        assert_eq!(filled["total"], 1, "{filled}");
    }

    /// A path_glob with a leading ./ (relative to project.root) matches nothing.
    #[test]
    fn dot_slash_path_glob_matches_project_files() {
        let (dir, mut s) = setup();
        write(&dir, "src/a.rs", "fn hit() {}\n");
        let listed = tools::execute(
            &mut s,
            "file_list",
            json!({"path_glob":"./src/**/*.rs","mode":"paths"}),
        )
        .unwrap();
        let searched = tools::execute(
            &mut s,
            "source_search",
            json!({"query":"hit","path_glob":"./src/**"}),
        )
        .unwrap();
        assert_eq!(listed["paths"], json!(["src/a.rs"]), "{listed}");
        assert_eq!(searched["total_matching_lines"], 1, "{searched}");
    }

    /// path_glob is matched against root-relative paths only; a glob written
    /// with the project root's absolute path (as models copy from results,
    /// which carry absolute paths) matches nothing.
    #[test]
    fn absolute_root_path_glob_matches_project_files() {
        let (dir, mut s) = setup();
        write(&dir, "src/a.rs", "fn hit() {}\n");
        let root = dir.path().canonicalize().unwrap();
        let glob = format!("{}/src/**/*.rs", root.display());
        let listed = tools::execute(
            &mut s,
            "file_list",
            json!({"path_glob":glob,"mode":"paths"}),
        )
        .unwrap();
        assert_eq!(listed["paths"], json!(["src/a.rs"]), "{listed}");
    }

    /// A history continuation sent as the whole next_cursor object (accepted by
    /// unwrap_continuation_cursor) reads the right text, but limit_result
    /// re-parses the raw arguments: the shortened page's continuation then
    /// points into a NEW archive at an offset counted from 0, not from the
    /// requested offset in the original bundle.
    #[test]
    fn history_read_via_cursor_object_keeps_its_continuation() {
        let (_dir, mut s) = setup();
        let body: String = (0..3000)
            .map(|i| format!("\"quoted {i}\" \\ path\n"))
            .collect();
        let id = s
            .history
            .push(vec![json!({"role":"user","content":body})], true);
        let plain = call(
            &mut s,
            "h1",
            "history",
            json!({"action":"read","id":id,"offset":100}),
        );
        assert_eq!(plain["status"], "ok", "{plain}");
        assert_eq!(
            plain["next_cursor"]["id"], id,
            "plain read: {}",
            plain["next_cursor"]
        );
        let wrapped = call(
            &mut s,
            "h2",
            "history",
            json!({"cursor":json!({"tool":"history","action":"read","id":id,"offset":100}).to_string()}),
        );
        assert_eq!(wrapped["status"], "ok", "{wrapped}");
        assert_eq!(
            wrapped["data"]["text"], plain["data"]["text"],
            "same page text"
        );
        assert_eq!(
            wrapped["next_cursor"], plain["next_cursor"],
            "continuation of the same page"
        );
    }
}

mod document_edit {
    use crate::support;
    use mnemoarc::{config::Project, session::Session, tools};
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
        session.active_tools = tools::ToolRegistry::optional_names();
        (dir, session)
    }

    fn sha(text: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    fn with_doc(text: &str) -> (tempfile::TempDir, Session) {
        let (dir, s) = setup();
        std::fs::write(dir.path().join("summary.md"), text).unwrap();
        (dir, s)
    }

    fn doc(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("summary.md")).unwrap()
    }

    /// Runs the call, turning a panic into an Err string so the probe reports it.
    fn call(s: &mut Session, name: &str, args: Value) -> Result<Value, String> {
        let name = name.to_owned();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tools::execute(s, &name, args)
        }));
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(format!("{error:#}")),
            Err(panic) => Err(format!(
                "PANIC: {}",
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default()
            )),
        }
    }

    fn section_replace(
        dir: &tempfile::TempDir,
        s: &mut Session,
        section: &str,
        text: &str,
    ) -> Result<Value, String> {
        let current = doc(dir);
        call(
            s,
            "document_edit",
            json!({"action":"section","section":section,"text":text,"expected_hash":sha(&current)}),
        )
    }

    #[test]
    fn scoped_anchor_elsewhere_in_korean_doc_without_trailing_newline_does_not_panic() {
        let text = "# 가이드\n\n설명 문장입니다\n\n## 설치\n\n내용입니다";
        let (_dir, mut s) = with_doc(text);
        let result = call(
            &mut s,
            "document_edit",
            json!({"action":"replace_text","section":"## 설치","old_text":"설명 문장입니다","text":"바뀐 문장"}),
        );
        // A later, valid write in a fresh session of the same process.
        let (_dir2, mut s2) = with_doc("# A\n\nold\n");
        let later = call(
            &mut s2,
            "document_edit",
            json!({"action":"replace_text","old_text":"old","text":"new"}),
        );
        let error = result.unwrap_err();
        assert!(!error.starts_with("PANIC"), "{error}");
        assert!(error.contains("patch_target_must_match_once"), "{error}");
        assert!(later.is_ok(), "{later:?}");
    }

    /// insert_before_text anchored at a heading line with one-line text glues
    /// the text to the heading ("...A.## B") and the heading is lost.
    #[test]
    fn inline_text_inserted_before_a_heading_line_keeps_the_heading() {
        let (dir, mut s) = with_doc("# G\n\n## A\n\nbody\n\n## B\n\nb\n");
        let r = call(
            &mut s,
            "document_edit",
            json!({"action":"insert_before_text","old_text":"## B","text":"Closing sentence of A."}),
        );
        let after = doc(&dir);
        // Either refused, or saved with ## B still a heading on its own line.
        if r.is_ok() {
            assert!(after.contains("\n## B\n"), "heading B destroyed: {after:?}");
        }
    }

    #[test]
    fn inline_text_inserted_after_a_passage_that_ends_before_a_heading_keeps_the_heading() {
        let (dir, mut s) = with_doc("# G\n\n## A\n\nbody\n\n## B\n\nb\n");
        let r = call(
            &mut s,
            "document_edit",
            json!({"action":"insert_after_text","old_text":"body\n\n","text":"More."}),
        );
        let after = doc(&dir);
        if r.is_ok() {
            assert!(after.contains("\n## B\n"), "heading B destroyed: {after:?}");
        }
    }

    /// The citation scanner knows a fence may open right after a list marker
    /// ("1. ```bash", list_fence_content), but headings() does not, so a shell
    /// comment inside that fence ("   # install") becomes a level-1 heading. A
    /// section replace then stops at the comment and leaves the rest of the
    /// fence behind.
    #[test]
    fn shell_comment_in_a_list_item_fence_is_not_a_heading() {
        let text =
            "# G\n\n## A\n\n1. ```bash\n   # install deps\n   npm install\n   ```\n\n## B\n\nb\n";
        let (dir, mut s) = with_doc(text);
        section_replace(&dir, &mut s, "## A", "## A\n\nnew A\n").unwrap();
        let after = doc(&dir);
        assert_eq!(after, "# G\n\n## A\n\nnew A\n\n## B\n\nb\n");
    }

    /// Same root cause without any '#' line: the list-item fence's closing
    /// "   ```" is taken for an opening fence, which hides every later heading,
    /// so the section replace deletes section B.
    #[test]
    fn list_item_fence_closer_does_not_hide_the_next_section() {
        let text = "# G\n\n## A\n\n1. ```bash\n   npm install\n   ```\n\n## B\n\nb\n";
        let (dir, mut s) = with_doc(text);
        section_replace(&dir, &mut s, "## A", "## A\n\nnew A\n").unwrap();
        let after = doc(&dir);
        assert_eq!(after, "# G\n\n## A\n\nnew A\n\n## B\n\nb\n");
    }

    /// A line starting with an inline ``` code span is not a fence opener in
    /// CommonMark (a backtick fence's info string cannot contain backticks).
    #[test]
    fn inline_triple_backtick_span_does_not_open_a_fence() {
        let text = "# G\n\n## A\n\n```cargo test``` 명령으로 실행합니다.\n\n## B\n\nbody B\n";
        let (dir, mut s) = with_doc(text);
        section_replace(&dir, &mut s, "## A", "## A\n\nnew A\n").unwrap();
        let after = doc(&dir);
        assert!(
            after.contains("## B\n\nbody B"),
            "section B was eaten: {after:?}"
        );
    }
}

mod citations {
    use crate::support;
    use mnemoarc::{
        config::{Config, Project},
        session::Session,
        tools::{self, ToolRegistry},
    };

    use serde_json::{Value, json};

    /// A source_document session with every optional tool active, as the
    /// existing documentation tests use.
    fn doc_setup() -> (tempfile::TempDir, Session) {
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
        s.select_workflow("source_document").unwrap();
        (dir, s)
    }

    fn run(s: &mut Session, name: &str, args: Value) -> Value {
        tools::execute(s, name, args).unwrap()
    }

    fn issues_of<'a>(audit: &'a Value, kind: &str) -> Vec<&'a Value> {
        audit["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|issue| issue["kind"] == kind)
            .collect()
    }

    fn outline_titles(page: &Value) -> Vec<String> {
        page["outline"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["heading"].as_str().unwrap().to_owned())
            .collect()
    }

    // ---------------------------------------------------------------------------
    // Citations in 4-space nested list items are skipped as indented code.
    // ---------------------------------------------------------------------------
    #[test]
    fn nested_list_item_citation_is_checked() {
        let (dir, mut s) = doc_setup();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run(&mut s, "file_read", json!({"path":"a.rs"}));
        // A 4-space nested bullet is list content in CommonMark, not code.
        let text = "# Guide\n\n- Parent item cites a.rs:1\n    - Child item cites missing.rs:5\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let audit = run(&mut s, "document_audit", json!({}));
        assert_eq!(audit["citations_checked"], 2, "{audit}");
        assert_eq!(issues_of(&audit, "citation_path").len(), 1, "{audit}");
    }

    #[test]
    fn nested_list_item_unread_citation_is_reported() {
        let (dir, mut s) = doc_setup();
        std::fs::write(
            dir.path().join("a.rs"),
            (1..=20).map(|n| format!("line {n}\n")).collect::<String>(),
        )
        .unwrap();
        run(
            &mut s,
            "file_read",
            json!({"path":"a.rs","start_line":1,"max_lines":2}),
        );
        let text = "# Guide\n\n1. Setup reads a.rs:1-2\n    1. The loop is at a.rs:10-15\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let audit = run(&mut s, "document_audit", json!({}));
        let unread = issues_of(&audit, "unread_citation");
        assert_eq!(unread.len(), 1, "a.rs:10-15 was never read: {audit}");
    }

    // ---------------------------------------------------------------------------
    // More shapes of the two confirmed defects.
    // ---------------------------------------------------------------------------
    #[test]
    fn ordered_item_ten_continuation_citation_is_checked() {
        let (dir, mut s) = doc_setup();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        run(&mut s, "file_read", json!({"path":"a.rs"}));
        // Item 10's content starts at column 5, so its continuation paragraph
        // is indented four spaces; it is prose, not an indented code block.
        let mut text = String::from("# Steps\n\n");
        for n in 1..=9 {
            text.push_str(&format!("{n}. Step a.rs:1\n"));
        }
        text.push_str("10. Last step\n\n    It is implemented in missing.rs:5.\n");
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let audit = run(&mut s, "document_audit", json!({}));
        assert_eq!(issues_of(&audit, "citation_path").len(), 1, "{audit}");
    }

    // ---------------------------------------------------------------------------
    // headings(): fences that the citation scanner and CommonMark treat differently.
    // ---------------------------------------------------------------------------
    #[test]
    fn list_marker_fence_does_not_swallow_later_headings() {
        let (_dir, mut s) = doc_setup();
        // `1. ```md` opens a fenced block inside a list item (the citation
        // scanner knows this shape); its `# Example` line is code, and the
        // closing fence must not open a new one.
        let text = "# Guide\n\n1. ```md\n   # Example\n   ```\n\n## Real\n\nBody.\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let page = run(&mut s, "document_inspect", json!({}));
        assert_eq!(outline_titles(&page), ["# Guide", "## Real"], "{page}");
    }

    #[test]
    fn list_marker_fence_heading_is_not_editable_as_a_section() {
        let (_dir, mut s) = doc_setup();
        let text = "# Guide\n\n1. ```md\n   # Example\n   ```\n\n## Real\n\nBody.\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        // The real heading after the example cannot be addressed.
        let result = tools::execute(&mut s, "document_inspect", json!({"section":"## Real"}));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn inline_triple_backtick_span_is_not_a_fence_for_headings() {
        let (_dir, mut s) = doc_setup();
        // A backtick info string cannot open a backtick fence (CommonMark 4.5):
        // this line is a paragraph with a code span.
        let text = "# Guide\n\n```js``` marks a JavaScript block.\n\n## Real\n\nBody.\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let page = run(&mut s, "document_inspect", json!({}));
        assert_eq!(outline_titles(&page), ["# Guide", "## Real"], "{page}");
    }

    // ---------------------------------------------------------------------------
    // Citation path character class.
    // ---------------------------------------------------------------------------
    #[test]
    fn sveltekit_plus_page_citation_is_recognised() {
        let (dir, mut s) = doc_setup();
        std::fs::create_dir_all(dir.path().join("src/routes")).unwrap();
        std::fs::write(
            dir.path().join("src/routes/+page.svelte"),
            "<h1>Hi</h1>\n<p>x</p>\n",
        )
        .unwrap();
        run(
            &mut s,
            "file_read",
            json!({"path":"src/routes/+page.svelte"}),
        );
        let text = "# Guide\n\nThe page is src/routes/+page.svelte:1-2.\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let audit = run(&mut s, "document_audit", json!({}));
        assert!(issues_of(&audit, "citation_path").is_empty(), "{audit}");
        assert_eq!(audit["structural_ok"], true, "{audit}");
    }

    #[test]
    fn nextjs_dynamic_route_citation_is_recognised() {
        let (dir, mut s) = doc_setup();
        std::fs::create_dir_all(dir.path().join("app/users/[id]")).unwrap();
        std::fs::write(
            dir.path().join("app/users/[id]/page.tsx"),
            "export default function Page() {}\n",
        )
        .unwrap();
        run(
            &mut s,
            "file_read",
            json!({"path":"app/users/[id]/page.tsx"}),
        );
        let text = "# Guide\n\nThe user page is app/users/[id]/page.tsx:1.\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let audit = run(&mut s, "document_audit", json!({}));
        assert!(issues_of(&audit, "citation_path").is_empty(), "{audit}");
    }

    #[test]
    fn backslash_path_is_not_garbled_on_save() {
        let (dir, mut s) = doc_setup();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/agent.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run(&mut s, "file_read", json!({"path":"src/agent.rs"}));
        let text = "# Guide\n\nOn Windows the loop is in src\\agent.rs:1-2.\n";
        run(
            &mut s,
            "document_edit",
            json!({"action":"create","text":text}),
        );
        let written = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
        assert!(
            !written.contains("src\\src/agent.rs"),
            "save rewrote the path into a doubled one: {written:?}"
        );
    }
}

mod markup {
    use crate::support;
    use mnemoarc::{config::Project, session::Session, tools};
    use serde_json::{Value, json};

    fn setup() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("summary.md"),
                ..Default::default()
            },
            support::compact_config(),
        );
        (dir, session)
    }

    /// file_write content recovered from leaked <arg_key> markup must be written
    /// byte for byte: no trimmed trailing newline / indentation, no JSON decoding.
    #[test]
    fn leaked_markup_content_is_written_verbatim() {
        let (dir, mut session) = setup();
        let mut args = serde_json::Map::new();
        args.insert(
            "content</arg_key><arg_value>    indented();\n</arg_value><arg_key>path".into(),
            json!("leak.rs"),
        );
        let result = tools::execute(&mut session, "file_write", Value::Object(args));
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("leak.rs")).unwrap(),
            "    indented();\n"
        );
    }

    /// The same for file_edit new_text: a JS string literal keeps its quotes.
    #[test]
    fn leaked_markup_new_text_is_not_json_decoded() {
        let (dir, mut session) = setup();
        std::fs::write(dir.path().join("a.js"), "let s = 1;\n").unwrap();
        let mut args = serde_json::Map::new();
        args.insert("path".into(), json!("a.js"));
        args.insert("expected_hash".into(), json!(tools::hash(b"let s = 1;\n")));
        args.insert(
            "new_text</arg_key><arg_value>\"hi\"</arg_value><arg_key>old_text".into(),
            json!("1"),
        );
        let result = tools::execute(&mut session, "file_edit", Value::Object(args));
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.js")).unwrap(),
            "let s = \"hi\";\n"
        );
    }
}

mod checkpoint {
    use crate::support;
    use mnemoarc::{config::Project, session::Session, tools};
    use serde_json::{Value, json};

    fn setup() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("summary.md"),
                ..Default::default()
            },
            support::compact_config(),
        );
        (dir, session)
    }

    fn call(id: &str, name: &str, args: Value) -> mnemoarc::llm::ToolCall {
        mnemoarc::llm::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    fn pending_checkpoint(s: &mut Session) {
        s.checkpoint = Some(mnemoarc::session::Checkpoint {
            id: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
            bundle_ids: vec![],
            maintenance_bundle_ids: vec![],
            acknowledged: false,
            attempts: 0,
            failed_attempts: 0,
            last_failure: None,
            source_lookup_calls: 0,
            starting_state_revision: s.task.revision,
            starting_memory_generation: s.memory.generation,
            failed: false,
        });
    }

    /// A call the checkpoint withholds ran nothing (tests/closing.rs
    /// a_withheld_call_does_not_hold_the_checkpoint). The same call with one
    /// argument mistake also runs nothing, but argument validation runs before
    /// the checkpoint gate, so it fails the checkpoint batch.
    #[test]
    fn withheld_call_with_bad_argument_does_not_hold_checkpoint() {
        let (dir, mut s) = setup();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        pending_checkpoint(&mut s);
        let result = tools::run_call(
            &mut s,
            &call("c1", "file_read", json!({"path":"main.rs","lines":"1-5"})),
        );
        let cp = s.checkpoint.as_ref().unwrap();
        assert!(
            !cp.failed,
            "withheld file_read with an unknown argument held the checkpoint: {}",
            result["error"]
        );
        // The acknowledgement in the same batch then succeeds.
        let ack = tools::run_call(
            &mut s,
            &call(
                "c2",
                "checkpoint_complete",
                json!({"id":"0f8fad5b-d9cb-469f-a165-70867728950e","progress":"Read main"}),
            ),
        );
        assert_eq!(ack["status"], "ok", "{ack}");
    }

    #[test]
    fn unknown_tool_during_checkpoint_does_not_hold_checkpoint() {
        let (_dir, mut s) = setup();
        pending_checkpoint(&mut s);
        let result = tools::run_call(&mut s, &call("c1", "read_file", json!({"path":"main.rs"})));
        assert!(
            !s.checkpoint.as_ref().unwrap().failed,
            "{}",
            result["error"]
        );
    }

    /// An inactive optional tool is withheld by the checkpoint too, but the
    /// tool_not_active refusal comes first and holds the checkpoint.
    #[test]
    fn inactive_optional_tool_during_checkpoint_does_not_hold_it() {
        let (_dir, mut s) = setup();
        s.active_tools.remove("file_list");
        pending_checkpoint(&mut s);
        let result = tools::run_call(&mut s, &call("c1", "file_list", json!({})));
        assert!(
            !s.checkpoint.as_ref().unwrap().failed,
            "{}",
            result["error"]
        );
    }

    /// At the lookup limit source_lookup runs nothing; its refusal says to call
    /// checkpoint_complete, which then fails in the same batch.
    #[test]
    fn lookup_limit_refusal_does_not_hold_checkpoint() {
        let (_dir, mut s) = setup();
        pending_checkpoint(&mut s);
        s.checkpoint.as_mut().unwrap().source_lookup_calls = 3;
        let result = tools::run_call(&mut s, &call("c1", "source_lookup", json!({"path":"main"})));
        assert!(
            !s.checkpoint.as_ref().unwrap().failed,
            "{}",
            result["error"]
        );
    }
}

mod pipeline {
    use crate::support;
    use mnemoarc::{config::Project, session::Session, tools};
    use serde_json::{Value, json};

    fn setup() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(
            Project {
                root: dir.path().into(),
                output: dir.path().join("summary.md"),
                ..Default::default()
            },
            support::compact_config(),
        );
        (dir, session)
    }

    fn call(id: &str, name: &str, args: Value) -> mnemoarc::llm::ToolCall {
        mnemoarc::llm::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    fn cursor_offset(cursor: &str) -> usize {
        cursor.rsplit_once(':').unwrap().1.parse().unwrap()
    }

    #[test]
    fn search_wrapped_cursor_page_advances() {
        let (dir, mut s) = setup();
        let line = format!("needle {}\n", "x".repeat(400));
        std::fs::write(dir.path().join("a.txt"), line.repeat(300)).unwrap();
        let first = tools::run_call(
            &mut s,
            &call("s1", "source_search", json!({"query":"needle","limit":100})),
        );
        let first_cursor = first["data"]["next_cursor"].as_str().unwrap().to_owned();
        let first_end = cursor_offset(&first_cursor);
        let shown_first = first["data"]["matches"].as_array().unwrap().len();
        println!(
            "first: shown {shown_first} next {first_cursor} truncated {}",
            first["truncated"]
        );
        assert_eq!(first_end, shown_first);
        let wrapped = tools::run_call(
            &mut s,
            &call(
                "s2",
                "source_search",
                json!({"query":"needle","limit":100,"cursor":{"tool":"source_search","cursor":first_cursor}}),
            ),
        );
        assert_eq!(wrapped["status"], "ok", "{wrapped}");
        let first_line = wrapped["data"]["matches"][0]["line"].clone();
        let shown = wrapped["data"]["matches"].as_array().unwrap().len();
        let end = cursor_offset(wrapped["data"]["next_cursor"].as_str().unwrap());
        println!("wrapped: first line {first_line} shown {shown} next {end}");
        assert_eq!(first_line, json!(first_end + 1));
        assert_eq!(end, first_end + shown);
    }

    // document_inspect's own next_cursor object, sent back whole as the cursor,
    // is unwrapped by the runtime; the next continuation must still name the
    // section (limit_result rebuilds it from the raw call arguments).
    #[test]
    fn inspect_wrapped_cursor_keeps_section() {
        let (dir, mut s) = setup();
        let body = "본문 문장입니다. ".repeat(4000);
        std::fs::write(
            dir.path().join("summary.md"),
            format!("# T\n\n## A\n\n{body}\n\n## B\n\nend\n"),
        )
        .unwrap();
        let first = tools::run_call(
            &mut s,
            &call("i1", "document_inspect", json!({"section":"## A"})),
        );
        let next = first["next_cursor"].clone();
        println!("first next_cursor {next}");
        assert_eq!(next["section"], "## A");
        let second = tools::run_call(
            &mut s,
            &call("i2", "document_inspect", json!({"cursor":next})),
        );
        assert_eq!(second["status"], "ok", "{second}");
        println!(
            "second read_offset {} next_cursor {}",
            second["data"]["read_offset"], second["next_cursor"]
        );
        assert_eq!(second["data"]["section"], "## A");
        // Following the returned continuation as given.
        let mut third_args = second["next_cursor"].clone();
        third_args.as_object_mut().unwrap().remove("tool");
        let third = tools::run_call(&mut s, &call("i3", "document_inspect", third_args));
        println!("third: {} {}", third["status"], third["error"]);
        assert_eq!(
            second["next_cursor"]["section"], "## A",
            "continuation lost its section"
        );
    }

    // A history read continuation object sent back whole as the cursor: the
    // runtime reads from its offset, but the bounded result's next offset is
    // computed from the raw call arguments.
    #[test]
    fn history_wrapped_cursor_continues_forward() {
        let (_dir, mut s) = setup();
        let text = "\"quoted\" 문장 ".repeat(6000);
        let id = s
            .history
            .push(vec![json!({"role":"user","content":text})], true);
        let first = tools::run_call(
            &mut s,
            &call("h1", "history", json!({"action":"read","id":id})),
        );
        println!(
            "first next_cursor {} truncated {}",
            first["next_cursor"], first["truncated"]
        );
        let next = first["next_cursor"].clone();
        let start = next["offset"].as_u64().unwrap();
        assert!(start > 0);
        let second = tools::run_call(
            &mut s,
            &call(
                "h2",
                "history",
                json!({"action":"read","id":id,"cursor":next}),
            ),
        );
        assert_eq!(second["status"], "ok", "{second}");
        println!("second next_cursor {}", second["next_cursor"]);
        let resumed = second["next_cursor"]["offset"].as_u64().unwrap();
        let delivered = second["data"]["text"].as_str().unwrap().chars().count() as u64;
        println!("start {start} delivered {delivered} resumed {resumed}");
        assert_eq!(
            resumed,
            start + delivered,
            "continuation does not resume after the delivered text"
        );
    }

    // suggest::special_field computes end - start + 1 unchecked.
    #[test]
    fn end_line_hint_does_not_overflow() {
        let (dir, mut s) = setup();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let r = tools::run_call(
            &mut s,
            &mnemoarc::llm::ToolCall {
                id: "e1".into(),
                name: "file_read".into(),
                arguments: r#"{"path":"a.txt","start_line":0,"end_line":18446744073709551615}"#
                    .into(),
            },
        );
        println!("{}", r["error"]);
        assert!(
            !r["error"]
                .as_str()
                .unwrap_or("")
                .contains("tool_worker_panic"),
            "{r}"
        );
    }

    // Leaked markup values are parsed as JSON, so a string argument that looks
    // like a number arrives as a number and the call is rejected.
    #[test]
    fn markup_numeric_text_stays_string() {
        let (dir, mut s) = setup();
        std::fs::write(dir.path().join("a.txt"), "error 404 here\n").unwrap();
        let r = tools::run_call(
            &mut s,
            &call(
                "q1",
                "source_search",
                json!({"query</arg_key><arg_value>404</arg_value><arg_key>path":"a.txt"}),
            ),
        );
        println!("{} {}", r["status"], r["error"]);
        assert_eq!(r["status"], "ok", "{r}");
    }
}
