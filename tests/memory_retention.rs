//! Keep this allocator probe in its own test binary so unrelated tests cannot
//! affect the measured live Rust allocations. Native library caches are excluded.
use mnemoarc::{
    config::{Config, Project},
    context::{self, ContextManager},
    session::{Checkpoint, Session},
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
}
