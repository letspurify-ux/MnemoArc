mod support;
use mnemoarc::{config::Project, session::Session, tools};
use serde_json::json;
use std::sync::{Arc, Barrier};

#[test]
fn document_and_project_writers_with_the_same_revision_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.md");
    let original = "# Shared\n\nOriginal content.\n";
    std::fs::write(&path, original).unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8).map(|index| {
            let barrier = barrier.clone();
            let mut session = Session::new(Project {
                root: dir.path().into(),
                output: if index % 2 == 0 { "shared.md".into() } else { format!("other-{index}.md").into() },
                ..Default::default()
            }, support::compact_config());
            session.active_tools.insert("document_edit".into());
            scope.spawn(move || {
                let replacement = format!("# Shared\n\nWriter {index}.\n");
                let (tool, args) = if index % 2 == 0 {
                    ("document_edit", json!({"action":"write","text":replacement,"expected_hash":tools::hash(original.as_bytes())}))
                } else {
                    ("file_patch", json!({"operations":[
                        {"action":"replace","path":"shared.md","content":replacement,"expected_hash":tools::hash(original.as_bytes())},
                        {"action":"add","path":format!("extra-{index}.txt"),"content":"written atomically with shared.md"}
                    ]}))
                };
                barrier.wait();
                (index, replacement, tools::execute(&mut session, tool, args))
            })
        }).collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    let winners: Vec<_> = results
        .iter()
        .filter(|(_, _, result)| result.is_ok())
        .collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), winners[0].1);
    for (index, _, result) in &results {
        if let Err(error) = result {
            assert!(error.to_string().contains("revision_conflict"), "{error}");
        }
        if index % 2 == 1 {
            assert_eq!(
                dir.path().join(format!("extra-{index}.txt")).exists(),
                result.is_ok()
            );
        }
    }
}
