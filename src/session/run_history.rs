use super::Session;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::time::Instant;

pub const RUN_HISTORY_LIMIT: usize = 20;

#[derive(Clone, Debug, Serialize)]
pub struct RunRecord {
    pub id: String,
    pub request: String,
    pub workflow: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub status: String,
    pub reason: String,
    pub error: Option<String>,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub usage_estimated: bool,
    pub rounds: usize,
    pub last_stage: String,
    pub checkpoint_pending: bool,
    pub token_limit: usize,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug)]
pub(super) struct ActiveRun {
    id: String,
    request: String,
    workflow: String,
    started_at: DateTime<Utc>,
    started: Instant,
    input_tokens: usize,
    output_tokens: usize,
    rounds: usize,
    usage_estimated: bool,
    reason: Option<String>,
}

fn excerpt(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

impl Session {
    pub(crate) fn begin_run(&mut self) {
        // Web starts this before spawning the worker, so even a worker panic
        // can be recorded from the owner's latest snapshot. Headless starts here too.
        if self.active_run.is_some() {
            return;
        }
        self.active_run = Some(ActiveRun {
            id: crate::memory::id(),
            request: excerpt(
                self.question
                    .as_ref()
                    .map_or(self.latest_request.as_str(), |q| q.text.as_str()),
                240,
            ),
            workflow: if self.question.is_some() {
                "follow_up".into()
            } else {
                self.task.workflow.clone()
            },
            started_at: Utc::now(),
            started: Instant::now(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            rounds: self.task_rounds,
            usage_estimated: false,
            reason: None,
        });
    }

    pub(crate) fn note_run_estimate(&mut self) {
        if let Some(run) = &mut self.active_run {
            run.usage_estimated = true;
        }
    }

    pub(crate) fn set_run_stop_reason(&mut self, reason: &str) {
        if let Some(run) = &mut self.active_run {
            run.reason = Some(reason.into());
        }
    }

    pub(crate) fn finish_run(&mut self) {
        let Some(run) = self.active_run.take() else {
            return;
        };
        let reason = if self.status == "cancelled" {
            "cancelled".into()
        } else if let Some(reason) = run.reason {
            reason
        } else if let Some(error) = &self.last_error {
            excerpt(error.split(':').next().unwrap_or(error), 120)
        } else if self.status == "complete_with_gaps" {
            self.progress_recovery
                .closing
                .as_ref()
                .map_or_else(|| self.status.clone(), |closing| closing.reason.clone())
        } else {
            self.status.clone()
        };
        let last_stage = if self.question.is_some() {
            "question"
        } else if self.checkpoint.is_some() {
            "checkpoint"
        } else {
            self.activity["stage"]
                .as_str()
                .filter(|stage| !matches!(*stage, "idle" | ""))
                .unwrap_or("preparing")
        };
        self.run_history.push_back(RunRecord {
            id: run.id,
            request: run.request,
            workflow: run.workflow,
            started_at: run.started_at,
            ended_at: Utc::now(),
            elapsed_ms: run.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            status: self.status.clone(),
            reason,
            error: self.last_error.as_deref().map(|error| excerpt(error, 4096)),
            input_tokens: self.input_tokens.saturating_sub(run.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(run.output_tokens),
            usage_estimated: run.usage_estimated,
            rounds: self.task_rounds.saturating_sub(run.rounds),
            last_stage: last_stage.into(),
            checkpoint_pending: self.question.is_none() && self.checkpoint.is_some(),
            token_limit: self.config.run_tokens,
            timeout_secs: self.config.run_timeout_secs,
        });
        while self.run_history.len() > RUN_HISTORY_LIMIT {
            self.run_history.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Project};

    #[test]
    fn records_are_bounded_and_survive_a_new_question() {
        let mut s = Session::new(Project::default(), Config::default());
        for index in 0..RUN_HISTORY_LIMIT + 2 {
            s.add_user(format!("Question {index}"));
            s.begin_run();
            s.status = "blocked".into();
            s.last_error = Some("run_budget_exhausted: insufficient budget".into());
            s.finish_run();
            s.finish_run(); // An owner finalizing twice must not duplicate a record.
        }
        assert_eq!(s.run_history.len(), RUN_HISTORY_LIMIT);
        assert_eq!(s.run_history.front().unwrap().request, "Question 2");
        let before = serde_json::to_value(&s.run_history).unwrap();
        s.add_user("Why did it stop?".into());
        s.last_error = None;
        assert_eq!(serde_json::to_value(&s.run_history).unwrap(), before);
    }
}
