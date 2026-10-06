//! Adapt older scheduling fixtures to the grounded review wire protocol.
//! Semantic acceptance/rejection is exercised without this adapter in
//! document_review_findings.rs. These fixtures deliberately confirm their
//! scripted findings so their original scheduling assertions remain useful.
use super::*;
use mnemoarc::{
    config::Config, llm::LlmClient, session::Session, tools::document_review as review,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn structured(payload: &Value, text: &str) -> String {
    let body = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```JSON")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return text.into();
    };
    // A bare array is the issue list itself; keep that shape for the runtime.
    let issues = if value.is_array() {
        value.as_array_mut()
    } else {
        value.get_mut("issues").and_then(Value::as_array_mut)
    };
    let Some(issues) = issues else {
        return text.into();
    };
    let line = payload["document_line_start"].as_u64().unwrap_or(1);
    let quote = payload["document"]
        .as_str()
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .split_once('|')
        .filter(|(prefix, _)| prefix.parse::<usize>().is_ok())
        .map_or_else(
            || {
                payload["document"]
                    .as_str()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
            },
            |(_, line)| line,
        )
        .chars()
        .take(300)
        .collect::<String>();
    for issue in issues {
        let Some(problem) = issue.as_str().map(str::to_owned) else {
            continue;
        };
        let previous = payload["previous_findings"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| {
                (f["problem"] == problem || f["text"] == problem) && f["document"]["quote"] == quote
            })
            .map(|f| f["id"].clone())
            .unwrap_or(Value::Null);
        *issue = json!({"previous_id":previous,"kind":"scope",
            "document":{"start_line":line,"end_line":line,"quote":quote},
            "requirement_id":"R0","sources":[],"problem":problem,"correction":problem,"ui_labels":[]});
    }
    value.to_string()
}

fn confirmation(payload: &Value) -> Completion {
    Completion { text:json!({"decisions":payload["candidates"].as_array().unwrap().iter().map(|c|json!({
        "id":c["id"],"status":"confirmed","reason":"The scheduling fixture explicitly confirms its scripted defect.","duplicate_of":null
    })).collect::<Vec<_>>()}).to_string(), ..Default::default() }
}

pub fn finish(s: &mut Session, text: &str) -> anyhow::Result<()> {
    let doc = std::fs::read_to_string(&s.project.output)?;
    let state = json!(s.document_review);
    let start = state["document_offset"].as_u64().unwrap_or(0) as usize;
    let payload = json!({"document_line_start":start+1,"document":doc.lines().skip(start).collect::<Vec<_>>().join("\n"),"previous_findings":s.document_review.findings});
    review::finish(s, &structured(&payload, text))?;
    while s.document_review.validating {
        let request = review::request(s)?;
        let payload: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap())?;
        review::finish(s, &confirmation(&payload).text)?;
    }
    Ok(())
}

pub struct Client(pub Arc<dyn LlmClient>);
#[async_trait::async_trait]
impl LlmClient for Client {
    async fn complete(
        &self,
        request: Value,
        config: &Config,
        cancel: CancellationToken,
        delta: mpsc::Sender<String>,
    ) -> anyhow::Result<Completion> {
        let payload: Value = request["messages"][1]["content"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        if payload["review_stage"] == "validate_findings" {
            return Ok(confirmation(&payload));
        }
        let mut result = self.0.complete(request, config, cancel, delta).await?;
        if payload["source_document_review"] == true {
            result.text = structured(&payload, &result.text);
        }
        Ok(result)
    }
}

pub async fn run_session(
    s: Session,
    client: Arc<dyn LlmClient>,
    cancel: CancellationToken,
    events: mpsc::Sender<mnemoarc::agent::AgentEvent>,
) -> Session {
    mnemoarc::agent::run_session(s, Arc::new(Client(client)), cancel, events).await
}
