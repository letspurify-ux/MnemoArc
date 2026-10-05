#[allow(dead_code)]
pub mod document_review;
use mnemoarc::{config::Config, llm::Completion};
use serde_json::{Value, json};

/// The compact 64K-context profile these tests were written against, kept
/// apart from Config::default so a default change does not rewrite their
/// scenarios (Config::compact_test in src/config.rs holds the same profile).
#[allow(dead_code)]
pub fn compact_config() -> Config {
    Config {
        base_url: "https://api.openai.com/v1".into(),
        model: String::new(),
        model_context: None,
        context_tokens: 64000,
        output_tokens: 8000,
        reasoning_effort: None,
        memory_body_bytes: 8192,
        memory_bytes: 16 * 1024 * 1024,
        high_water: 0.8,
        low_water: 0.6,
        request_timeout_secs: 180,
        run_timeout_secs: 1800,
        run_tokens: 500000,
        review_limit: 3,
        ..Config::default()
    }
}

/// Existing workflow fixtures test structural gates. Supply an explicit model
/// acceptance verdict for the additional independent review; semantic rejection
/// and recovery are covered by completion_review.rs.
#[allow(dead_code)]
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

#[allow(dead_code)]
pub async fn seed_session(client: &reqwest::Client, url: &str) -> Option<String> {
    let state: Value = client
        .get(format!("{url}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for project in state["config"]["projects"].as_array().unwrap() {
        if !std::path::Path::new(project["root"].as_str().unwrap()).is_dir() {
            continue;
        }
        let created: Value = client
            .post(format!("{url}/api/sessions"))
            .header("x-mnemoarc-client", "web")
            .json(&serde_json::json!({"project":project,"workflow":"answer"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        return Some(created["id"].as_str().unwrap().to_owned());
    }
    None
}
