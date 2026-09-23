use mnemoarc::llm::Completion;
use serde_json::{Value, json};

/// Existing workflow fixtures test structural gates. Supply an explicit model
/// acceptance verdict for the additional independent review; semantic rejection
/// and recovery are covered by completion_review.rs.
pub fn acceptance(request: &Value) -> Option<Completion> {
    let payload: Value = serde_json::from_str(request["messages"][1]["content"].as_str()?).ok()?;
    if payload["completion_review"] != true {
        return None;
    }
    let evidence = payload["evidence"]
        .as_array()?
        .iter()
        .find(|e| e["id"] != "answer")
        .map(|e| e["id"].clone())
        .unwrap_or(json!("answer"));
    Some(Completion { text: json!({"checks":payload["criteria"].as_array()?.iter().map(|c| json!({"id":c["id"],"status":"met","reason":"Fixture result satisfies this criterion","evidence":[evidence],"next_action":""})).collect::<Vec<_>>()}).to_string(), ..Default::default() })
}
