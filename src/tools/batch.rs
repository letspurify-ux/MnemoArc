//! Independently committed items retain their outcomes when later work stops.
use super::*;
use tokio_util::sync::CancellationToken;

pub(super) fn run_items(
    items: &serde_json::Map<String, Value>,
    cancel: &CancellationToken,
    mut execute: impl FnMut(&str, &Value) -> Result<Value>,
) -> Vec<Value> {
    let ordered: BTreeMap<_, _> = items.iter().collect();
    ordered.into_iter().map(|(id, entry)| {
        let result = if cancel.is_cancelled() {
            Err(recovery::DiagnosticError {
                message: "cancelled: this batch item was not started; completed item outcomes are retained".into(),
                data: json!({"execution":"not_started"}),
            }.into())
        } else {
            execute(id, entry)
        };
        json!({"id":id,"result":envelope(result)})
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_keeps_completed_items_and_never_starts_remaining_items() {
        let cancel = CancellationToken::new();
        let items = json!({"a":{},"b":{},"c":{}});
        let mut completed = vec![];
        let results = run_items(items.as_object().unwrap(), &cancel, |id, _| {
            completed.push(id.to_owned());
            cancel.cancel();
            Ok(json!({"saved":id}))
        });
        assert_eq!(completed, ["a"]);
        assert_eq!(results[0]["result"]["status"], "ok");
        assert_eq!(results[0]["result"]["data"]["saved"], "a");
        for result in &results[1..] {
            assert_eq!(result["result"]["status"], "cancelled");
            assert_eq!(result["result"]["data"]["execution"], "not_started");
        }
        let result = envelope(Ok(json!({"results":results})));
        assert_eq!(result["status"], "error");
        assert_eq!(result["partial_success"], true);
    }
}
