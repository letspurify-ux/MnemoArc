//! Keep this allocator probe in its own test binary so unrelated tests cannot
//! affect the measured live Rust allocations. Native library caches are excluded.
use mnemoarc::{
    config::{Config, Project},
    context::{self, ContextManager},
    session::{Checkpoint, Investigation, Session, SessionHistory},
    tools::{self, document_review},
};
use serde_json::{Value, json};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::Write,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};

struct TrackedAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, SeqCst) + bytes;
    PEAK.fetch_max(live, SeqCst);
}

unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), SeqCst);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, size) };
        if !pointer.is_null() {
            if size >= layout.size() {
                added(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, SeqCst);
            }
        }
        pointer
    }
}

#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;

fn measured<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    let baseline = LIVE.load(SeqCst);
    PEAK.store(baseline, SeqCst);
    let result = operation();
    (result, PEAK.load(SeqCst).saturating_sub(baseline))
}

const MIB: usize = 1024 * 1024;

fn retired_task_storage() -> usize {
    let mut session = Session::new(Project::default(), Config::default());
    session.add_user("original task".into());
    session.task.constraints.push("keep this constraint".into());
    let baseline = LIVE.load(SeqCst);
    session.investigations = (0..16_384)
        .map(|_| Investigation {
            id: String::new(),
            title: String::new(),
            status: String::new(),
            memory_refs: Default::default(),
            sources: Vec::new(),
            section: String::new(),
            document_hash: None,
            note: String::new(),
        })
        .collect();
    session.completion_gaps = vec![String::new(); 32_768];
    // Resuming still owns this task's state; only a new task retires it.
    session.add_user("continue".into());
    assert_eq!(session.investigations.len(), 16_384);
    session.start_new_task("next task".into());
    assert!(session.investigations.is_empty());
    assert!(session.completion_gaps.is_empty());
    assert_eq!(session.task.constraints, ["keep this constraint"]);
    assert_eq!(session.history.bundles.len(), 3);
    let retained = LIVE.load(SeqCst).saturating_sub(baseline);
    eprintln!("retired task extra live allocation: {retained} bytes");
    retained
}

fn retired_history_storage() -> usize {
    let baseline = LIVE.load(SeqCst);
    let mut history = SessionHistory::default();
    for _ in 0..8192 {
        history.push(vec![json!({"role":"assistant","content":"retired"})], true);
        let bundle = history.bundles.back_mut().unwrap();
        bundle.active = false;
        bundle.reviewed = true;
    }
    history.prune(0).unwrap();
    assert!(history.bundles.is_empty());
    assert_eq!(history.pruned_through, Some(8192));
    assert_eq!(history.next_id, 8192);
    let retained = LIVE.load(SeqCst).saturating_sub(baseline);
    eprintln!("empty pruned history live allocation: {retained} bytes");
    let next = history.push(vec![json!({"role":"user","content":"new request"})], true);
    assert_eq!(next, 8193);
    assert_eq!(
        history.read(next).unwrap().messages[0]["content"],
        "new request"
    );
    retained
}

fn failed_edit_tracker_storage() -> usize {
    let error = tools::envelope(Err(anyhow::anyhow!(
        "section_not_found: {}",
        "unmatched heading ".repeat(1024)
    )));
    let baseline = LIVE.load(SeqCst);
    let mut failures = tools::recovery::FailureTracker::default();
    for index in 0..2048 {
        let arguments = json!({"section":format!("Heading {index}")}).to_string();
        // Document work can correct failures past the ordinary retry limit,
        // including after checkpoints have retired their history bundles.
        let _ = failures.observe("document_edit", &arguments, &error, 8);
    }
    let retained = LIVE.load(SeqCst).saturating_sub(baseline);
    eprintln!("document failure tracker live allocation: {retained} bytes");
    retained
}

fn failure_budget_tracker_storage() -> (usize, usize) {
    let baseline = LIVE.load(SeqCst);
    let mut names = tools::recovery::FailureTracker::default();
    for index in 0..4096 {
        let name = format!("unregistered_{index}_{}", "x".repeat(96));
        let error = tools::envelope(Err(anyhow::anyhow!("unsupported_tool: {name}")));
        let _ = names.observe(&name, "{}", &error, 8);
    }
    let name_bytes = LIVE.load(SeqCst).saturating_sub(baseline);
    drop(names);

    let baseline = LIVE.load(SeqCst);
    let mut codes = tools::recovery::FailureTracker::default();
    for index in 0..4096 {
        let code = format!(
            "invalid_argument_{}{}{}",
            char::from(b'a' + ((index / 676) % 26) as u8),
            char::from(b'a' + ((index / 26) % 26) as u8),
            char::from(b'a' + (index % 26) as u8),
        );
        let error = tools::envelope(Err(anyhow::anyhow!("{code}: rejected")));
        let _ = codes.observe("document_edit", "{}", &error, 8);
    }
    let code_bytes = LIVE.load(SeqCst).saturating_sub(baseline);
    eprintln!("failure budget live allocations: names={name_bytes}, codes={code_bytes} bytes");
    (name_bytes, code_bytes)
}

fn paginated_outline_peak() -> usize {
    let dir = tempfile::tempdir().unwrap();
    let text = format!(
        "pub fn first() {{}}\r\n{}pub fn last(\r\n    value: bool,\r\n) -> bool {{\r\n    value\r\n}}\r\n",
        "\n".repeat(MIB)
    );
    std::fs::write(dir.path().join("sparse.rs"), text).unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        Config::default(),
    );
    session.active_tools.insert("code_outline".into());
    let mut peak = 0;
    for view in ["compact", "detailed"] {
        let (outline, allocated) = measured(|| {
            tools::execute(
                &mut session,
                "code_outline",
                json!({"path":"sparse.rs","query":"last","match":"exact","view":view,"limit":1}),
            )
            .unwrap()
        });
        eprintln!("{view} outline live allocation peak: {allocated} bytes");
        peak = peak.max(allocated);
        assert_eq!(outline["total_symbols"], 1);
        let symbol = &outline["symbols"][0];
        assert_eq!(symbol["name"], "last");
        assert_eq!(symbol["start_line"], MIB + 2);
        if view == "detailed" {
            assert_eq!(symbol["declaration"], "pub fn last(");
            assert_eq!(symbol["signature_start_line"], MIB + 2);
            assert_eq!(symbol["signature_end_line"], MIB + 4);
            let source = &session.sources[symbol["signature_source"]["id"].as_str().unwrap()];
            assert!(source.line_start_complete);
            // The opening body brace is not part of the signature excerpt.
            assert!(!source.line_end_complete);
        }
    }
    peak
}

fn scoped_discovery_extra_peak() -> usize {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("kept")).unwrap();
    std::fs::create_dir(dir.path().join("other")).unwrap();
    std::fs::write(dir.path().join("kept/one.rs"), "fn bounded() {}\n").unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().into(),
            ..Default::default()
        },
        Config::default(),
    );
    session.active_tools.insert("source_search".into());
    let calls = [
        ("file_list", json!({"path":"kept","mode":"paths","limit":1})),
        (
            "source_search",
            json!({"path":"kept","query":"bounded","mode":"files","limit":1}),
        ),
    ];
    let baseline: Vec<_> = calls
        .iter()
        .map(|(tool, args)| {
            measured(|| tools::execute(&mut session, tool, args.clone()).unwrap()).1
        })
        .collect();
    for index in 0..2048 {
        let name = format!("entry_{index:04}_{}.txt", "x".repeat(200));
        std::fs::File::create(dir.path().join("other").join(name)).unwrap();
    }
    let mut extra = 0;
    for ((tool, args), baseline) in calls.into_iter().zip(baseline) {
        let (result, peak) = measured(|| tools::execute(&mut session, tool, args).unwrap());
        if tool == "file_list" {
            assert_eq!(result["paths"], json!(["kept/one.rs"]));
            assert_eq!(result["total_files"], 1);
        } else {
            assert_eq!(result["matching_files"], 1);
        }
        eprintln!("{tool} scoped discovery peak: {peak} bytes (empty sibling: {baseline})");
        extra = extra.max(peak.saturating_sub(baseline));
    }
    extra
}

fn bounded_directory_errors_do_not_retain_every_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().canonicalize().unwrap(),
            ..Default::default()
        },
        Config::default(),
    );
    // Dispatch constructs tool schemas too. Compare with the empty directory
    // so that fixed overhead is not mistaken for retained directory entries.
    let (error, baseline) =
        measured(|| tools::execute(&mut session, "file_read", json!({"path":"."})).unwrap_err());
    assert!(error.to_string().starts_with("path_is_directory:"));
    drop(error);
    for index in 0..2048 {
        let name = format!("entry_{index:04}_{}", "x".repeat(200));
        std::fs::File::create(dir.path().join(name)).unwrap();
    }
    let (error, peak) =
        measured(|| tools::execute(&mut session, "file_read", json!({"path":"."})).unwrap_err());
    assert!(error.to_string().starts_with("path_is_directory:"));
    eprintln!("bounded directory diagnostic allocation peak: {peak} bytes (empty: {baseline})");
    assert!(
        peak < baseline + 64 * 1024,
        "a short directory error retained every file name: {peak} bytes"
    );
}

fn bounded_reads_do_not_duplicate_unreturned_text() {
    let dir = tempfile::tempdir().unwrap();
    let text = "source ".repeat(600_000);
    std::fs::write(dir.path().join("large.txt"), &text).unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().canonicalize().unwrap(),
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            result_tokens: 200,
            ..Default::default()
        },
    );
    context::tokens("Warm the shared tokenizer.", &session.config.model);

    let ((preview, truncated), truncate_peak) =
        measured(|| context::truncate(&text, 64, &session.config.model));
    eprintln!("text truncation extra live allocation peak: {truncate_peak} bytes");
    assert!(truncated);
    assert!(text.starts_with(&preview));
    assert!(context::tokens(&preview, &session.config.model) <= 64);
    drop(preview);

    let (read, file_peak) = measured(|| {
        tools::execute(
            &mut session,
            "file_read",
            json!({"path":"large.txt","max_lines":1}),
        )
        .unwrap()
    });
    eprintln!("bounded file read extra live allocation peak: {file_peak} bytes");
    let shown = read["content"]["text"].as_str().unwrap();
    assert_eq!(read["content"]["truncated"], true);
    assert!(!shown.is_empty());
    assert!(text.starts_with(shown));
    assert!(context::tokens(shown, &session.config.model) <= 100);
    assert_eq!(read["content"]["next_offset"], shown.chars().count());
    assert_eq!(read["content"]["first_line_complete"], true);
    assert_eq!(read["content"]["last_line_complete"], false);
    assert!(
        truncate_peak < 8 * MIB,
        "truncation copied the full input into a character array: {truncate_peak} bytes"
    );
    assert!(
        file_peak < 10 * MIB,
        "a bounded read copied unreturned file text: {file_peak} bytes"
    );
}

#[test]
fn bounded_operations_do_not_duplicate_retained_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("review.md");
    let mut document = String::from("# Observed behavior\n");
    let padding = vec![b'x'; 2 * MIB];
    for index in 0..8 {
        let name = format!("source_{index}.rs");
        let mut file = std::fs::File::create(dir.path().join(&name)).unwrap();
        writeln!(file, "fn observed_{index}() {{}}").unwrap();
        for _ in 0..19 {
            writeln!(file, "// Nearby source context").unwrap();
        }
        file.write_all(&padding).unwrap();
        document.push_str(&format!("Observed behavior {index}. {name}:1\n"));
    }
    drop(padding);
    std::fs::write(&output, document).unwrap();
    let mut session = Session::new(
        Project {
            root: dir.path().canonicalize().unwrap(),
            output,
            ..Default::default()
        },
        Config {
            model: "gpt-4o".into(),
            history_bytes: 4 * MIB,
            ..Default::default()
        },
    );
    session.add_user("Explain the observed behavior with source evidence.".into());
    // Vocabulary initialization is fixed process retention, not review input.
    context::count(&json!("Warm the shared tokenizer."), &session.config.model);

    let (request, review_peak) = measured(|| document_review::request(&mut session).unwrap());
    eprintln!("review preparation extra live allocation peak: {review_peak} bytes");
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    for index in 0..8 {
        assert!(payload["evidence"].as_array().unwrap().iter().any(|chunk| {
            chunk["path"] == format!("source_{index}.rs")
                && chunk["numbered_text"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("1|fn observed_{index}() {{}}"))
        }));
    }
    drop((request, payload));

    let ((), freshness_peak) = measured(|| {
        document_review::finish(&mut session, r#"{"issues":[]}"#).unwrap();
        assert!(document_review::approved(&session));
    });
    eprintln!("review freshness check extra live allocation peak: {freshness_peak} bytes");
    assert!(
        freshness_peak < MIB,
        "freshness checks allocated whole source files: {freshness_peak} bytes"
    );

    // A wide citation includes the huge last line, which cannot fit in a
    // review request. Detect that without first retaining every file's text.
    let broad_document = std::fs::read_to_string(&session.project.output)
        .unwrap()
        .replace(":1\n", ":1-21\n");
    std::fs::write(&session.project.output, broad_document).unwrap();
    session.document_review = Default::default();
    let (error, broad_peak) = measured(|| document_review::request(&mut session).unwrap_err());
    eprintln!("oversized evidence preparation extra live allocation peak: {broad_peak} bytes");
    assert!(error.to_string().starts_with("document_review_budget:"));
    assert!(
        broad_peak < 12 * MIB,
        "review retained evidence that could not fit a page: {broad_peak} bytes"
    );

    // Many bounded lines produce a manifest too large for one request. Keep
    // only chunk ranges during fitting, rather than all later pages' evidence.
    let bounded_lines = format!("{}\n", "source ".repeat(585)).repeat(512);
    for index in 0..8 {
        std::fs::write(
            dir.path().join(format!("source_{index}.rs")),
            &bounded_lines,
        )
        .unwrap();
    }
    drop(bounded_lines);
    let broad_document = std::fs::read_to_string(&session.project.output)
        .unwrap()
        .replace(":1-21\n", ":1-512\n");
    std::fs::write(&session.project.output, broad_document).unwrap();
    let (error, manifest_peak) = measured(|| document_review::request(&mut session).unwrap_err());
    eprintln!("oversized manifest preparation extra live allocation peak: {manifest_peak} bytes");
    assert!(error.to_string().starts_with("document_review_budget:"));
    assert!(
        manifest_peak < 12 * MIB,
        "review materialized evidence for an unusable manifest: {manifest_peak} bytes"
    );

    session.document_review = Default::default();
    for _ in 0..8 {
        session.history.push(
            vec![json!({"role":"assistant","content":"x".repeat(2 * MIB)})],
            true,
        );
    }
    session.checkpoint = Some(Checkpoint {
        id: "allocation-probe".into(),
        bundle_ids: session
            .history
            .bundles
            .iter()
            .map(|bundle| bundle.id)
            .collect(),
        maintenance_bundle_ids: vec![],
        acknowledged: true,
        attempts: 1,
        failed_attempts: 0,
        last_failure: None,
        source_lookup_calls: 0,
        starting_state_revision: session.task.revision,
        starting_memory_generation: session.memory.generation,
        failed: false,
    });
    let ((), checkpoint_peak) = measured(|| ContextManager::commit(&mut session).unwrap());
    eprintln!("checkpoint commit extra live allocation peak: {checkpoint_peak} bytes");
    assert!(
        review_peak < 12 * MIB,
        "review retained whole cited files simultaneously: {review_peak} bytes"
    );
    assert!(
        checkpoint_peak < 4 * MIB,
        "checkpoint duplicated retained history before pruning: {checkpoint_peak} bytes"
    );
    assert!(session.history.bytes() <= session.config.history_bytes);
    assert!(
        session
            .history
            .bundles
            .iter()
            .all(|bundle| !bundle.active && bundle.reviewed)
    );
    assert!(session.checkpoint.is_none());
    bounded_reads_do_not_duplicate_unreturned_text();
    bounded_directory_errors_do_not_retain_every_name();
    let retired_task_bytes = retired_task_storage();
    let retired_bytes = retired_history_storage();
    let failure_tracker_bytes = failed_edit_tracker_storage();
    let (failure_name_bytes, failure_code_bytes) = failure_budget_tracker_storage();
    let outline_peak = paginated_outline_peak();
    let discovery_extra = scoped_discovery_extra_peak();
    assert!(
        retired_task_bytes < 64 * 1024,
        "a new task retained the previous task's empty collection storage: {retired_task_bytes} bytes"
    );
    assert!(
        retired_bytes < 4096,
        "pruning all history retained its old bundle storage: {retired_bytes} bytes"
    );
    assert!(
        failure_tracker_bytes < 256 * 1024,
        "document recovery retained every failed invocation and full error body: {failure_tracker_bytes} bytes"
    );
    assert!(
        failure_name_bytes < 128 * 1024 && failure_code_bytes < 128 * 1024,
        "failure budgets retained every tool name or error code: names={failure_name_bytes}, codes={failure_code_bytes} bytes"
    );
    assert!(
        outline_peak < 8 * MIB,
        "one outline entry retained an index of every unreturned line: {outline_peak} bytes"
    );
    assert!(
        discovery_extra < 128 * 1024,
        "scoped discovery retained unrelated directory entries: {discovery_extra} extra bytes"
    );
}
