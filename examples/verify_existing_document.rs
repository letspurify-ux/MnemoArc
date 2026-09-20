//! Explicit live verification of an existing document; never writes the document.
use mnemoarc::{
    config::{Config, Secret},
    llm::{LlmClient, OpenAiClient},
    session::Session,
    tools::document_review,
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};
use tokio_util::sync::CancellationToken;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var("MNEMOARC_LIVE_TEST").as_deref() != Ok("1") {
        anyhow::bail!("Set MNEMOARC_LIVE_TEST=1 to authorize provider calls");
    }
    dotenvy::dotenv().ok();
    let path = Path::new("config.toml");
    let mut config = Config::load(path, &BTreeMap::new())?;
    if path.with_extension("credentials.json").exists() {
        let keys: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(path.with_extension("credentials.json"))?)?;
        config.api_key = keys.get(&config.api_key_env).cloned().map(Secret);
    }
    let project = config
        .projects
        .iter()
        .find(|p| p.name == "MnemoArc")
        .unwrap()
        .clone();
    let mut s = Session::new(project, config);
    s.answer_review_question="기존 문서에서 MnemoArc backend의 실제 동작 흐름과 Mermaid, 인용 및 수치 한도가 최신 소스와 일치하는지 검토한다. 범위를 늘리거나 구현 세부사항을 모두 추가할 필요는 없다. 최종 결과 보고는 별도로 제공된다.".into();
    let mut pages = Vec::new();
    let mut response_failures = 0;
    let mut input = 0usize;
    let mut output = 0usize;
    loop {
        let request = document_review::request(&mut s)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut request_config = s.config.clone();
        request_config.output_tokens = request_config.output_tokens.min(4096);
        let result = OpenAiClient
            .complete(request, &request_config, CancellationToken::new(), tx)
            .await?;
        drain.await?;
        let usage = result.usage.clone().unwrap_or_default();
        input += usage.input;
        output += usage.output;
        if result.length_limited || !result.calls.is_empty() || result.text.trim().is_empty() {
            response_failures += 1;
            s.last_error = Some(
                "document_review_incomplete: review must return complete JSON without tools".into(),
            );
            eprintln!(
                "Incomplete review response {response_failures}/3; retrying same evidence page"
            );
            anyhow::ensure!(response_failures < 3, "Incomplete review response limit");
            continue;
        }
        println!(
            "page {}: input={} output={} verdict={}",
            pages.len() + 1,
            usage.input,
            usage.output,
            result.text
        );
        pages.push(json!({"input":usage.input,"output":usage.output,"verdict":result.text}));
        if let Err(error) = document_review::finish(&mut s, &result.text) {
            response_failures += 1;
            s.last_error = Some(error.to_string());
            anyhow::ensure!(
                response_failures < 3 && error.to_string().starts_with("document_review_invalid:"),
                "{error}"
            );
            continue;
        }
        response_failures = 0;
        if !s.document_review.pending {
            break;
        }
    }
    let approved = document_review::approved(&s);
    let report = json!({"approved":approved,"output":s.project.output,"review":s.document_review,"pages":pages,"input":input,"output_tokens":output});
    std::fs::write(
        ".mnemoarc/live-final-review.json",
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "FINAL approved={approved}, pages={}, input={input}, output={output}",
        pages.len()
    );
    Ok(())
}
