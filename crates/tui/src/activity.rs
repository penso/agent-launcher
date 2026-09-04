use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    time::{Duration, Instant},
};

use agent_launcher_core::{RunState, RuntimeSnapshot};

const HISTORY_DURATION: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default)]
pub(crate) struct AgentActivity {
    pub score: u64,
    pub working: usize,
    pub attention: usize,
    pub idle: usize,
    history: VecDeque<(Instant, u64)>,
    last_event_sequences: HashMap<String, u64>,
    last_states: HashMap<String, RunState>,
    initialized: bool,
}

impl AgentActivity {
    pub fn initialize(&mut self, snapshot: &RuntimeSnapshot) {
        self.history.clear();
        self.last_event_sequences.clear();
        self.last_states.clear();
        let now = Instant::now();
        let wall_clock = chrono::Utc::now();
        let mut historical = BTreeMap::<u64, (u64, u64)>::new();
        for events in snapshot.run_events.values() {
            for event in events {
                let Ok(age) = (wall_clock - event.timestamp).to_std() else {
                    continue;
                };
                if age <= HISTORY_DURATION {
                    historical.entry(age.as_secs()).or_default().0 += 1;
                }
            }
        }
        for run in &snapshot.runs {
            if matches!(run.state, RunState::Completed | RunState::Failed) {
                let Ok(age) = (wall_clock - run.updated_at).to_std() else {
                    continue;
                };
                if age <= HISTORY_DURATION {
                    historical.entry(age.as_secs()).or_default().1 += 1;
                }
            }
        }
        for (age, (events, completions)) in historical.into_iter().rev() {
            let score = (events.min(4) * 10 + completions.min(2) * 20).min(100);
            if let Some(sampled_at) = now.checked_sub(Duration::from_secs(age)) {
                self.history.push_back((sampled_at, score));
            }
        }
        for run in &snapshot.runs {
            let next_sequence = snapshot
                .run_events
                .get(&run.id)
                .and_then(|events| events.last())
                .map_or(0, |event| event.sequence.saturating_add(1));
            self.last_event_sequences
                .insert(run.id.clone(), next_sequence);
            self.last_states.insert(run.id.clone(), run.state);
        }
        self.record_at(snapshot, now);
        self.initialized = true;
    }

    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn record(&mut self, snapshot: &RuntimeSnapshot) {
        self.record_at(snapshot, Instant::now());
    }

    fn record_at(&mut self, snapshot: &RuntimeSnapshot, now: Instant) {
        self.working = snapshot
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.state,
                    RunState::Provisioning | RunState::Starting | RunState::Running
                )
            })
            .count();
        self.attention = snapshot
            .runs
            .iter()
            .filter(|run| run.state.needs_attention())
            .count();
        self.idle = snapshot
            .runs
            .iter()
            .filter(|run| run.state == RunState::Idle)
            .count();

        let mut new_events = 0_u64;
        let mut completed = 0_u64;
        for run in &snapshot.runs {
            let next_sequence = snapshot
                .run_events
                .get(&run.id)
                .and_then(|events| events.last())
                .map_or(0, |event| event.sequence.saturating_add(1));
            if let Some(previous) = self
                .last_event_sequences
                .insert(run.id.clone(), next_sequence)
            {
                new_events = new_events.saturating_add(next_sequence.saturating_sub(previous));
            }
            if self
                .last_states
                .insert(run.id.clone(), run.state)
                .is_some_and(|previous| {
                    previous.is_active()
                        && matches!(run.state, RunState::Completed | RunState::Failed)
                })
            {
                completed = completed.saturating_add(1);
            }
        }
        self.last_event_sequences
            .retain(|run_id, _| snapshot.runs.iter().any(|run| run.id == *run_id));
        self.last_states
            .retain(|run_id, _| snapshot.runs.iter().any(|run| run.id == *run_id));

        let needs_input = snapshot
            .runs
            .iter()
            .filter(|run| run.state == RunState::NeedsInput)
            .count() as u64;
        self.score = ((self.working as u64 * 45)
            + (self.attention as u64 * 12)
            + (needs_input * 8)
            + (self.idle as u64 * 4)
            + (new_events.min(4) * 10)
            + (completed.min(2) * 20))
            .min(100);
        self.history.push_back((now, self.score));
        while self
            .history
            .front()
            .is_some_and(|(sampled_at, _)| now.duration_since(*sampled_at) > HISTORY_DURATION)
        {
            self.history.pop_front();
        }
    }

    pub fn sparkline(&self, width: usize) -> Vec<u64> {
        self.sparkline_at(width, Instant::now())
    }

    fn sparkline_at(&self, width: usize, now: Instant) -> Vec<u64> {
        if width == 0 {
            return Vec::new();
        }
        let mut values = vec![0; width];
        for (sampled_at, value) in &self.history {
            let age = now.checked_duration_since(*sampled_at).unwrap_or_default();
            if age > HISTORY_DURATION {
                continue;
            }
            let elapsed = HISTORY_DURATION.saturating_sub(age);
            let column = ((elapsed.as_secs_f64() / HISTORY_DURATION.as_secs_f64()) * width as f64)
                .floor() as usize;
            let column = column.min(width - 1);
            values[column] = values[column].max(*value);
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use agent_launcher_core::{EventEnvelope, OutputStream, RunEvent, RunSummary, RuntimeSnapshot};
    use chrono::Utc;

    use super::*;

    fn run(id: &str, state: RunState) -> RunSummary {
        let now = Utc::now();
        RunSummary {
            id: id.to_owned(),
            issue_key: format!("issue-{id}"),
            workspace: None,
            agent: "opencode".to_owned(),
            state,
            message: None,
            session_id: None,
            started_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn scores_agent_state_new_feedback_and_completion() {
        let start = Instant::now();
        let mut activity = AgentActivity::default();
        let mut snapshot = RuntimeSnapshot {
            runs: vec![run("one", RunState::Running)],
            ..RuntimeSnapshot::default()
        };
        activity.record_at(&snapshot, start);
        assert_eq!(activity.score, 45);
        assert_eq!(activity.working, 1);

        snapshot.run_events = HashMap::from([("one".to_owned(), vec![EventEnvelope {
            run_id: "one".to_owned(),
            sequence: 0,
            timestamp: Utc::now(),
            payload: RunEvent::Output {
                stream: OutputStream::Pty,
                text: "working".to_owned(),
            },
        }])]);
        activity.record_at(&snapshot, start + Duration::from_secs(1));
        assert_eq!(activity.score, 55);

        snapshot.runs[0].state = RunState::Completed;
        activity.record_at(&snapshot, start + Duration::from_secs(2));
        assert_eq!(activity.score, 20);
        assert_eq!(activity.working, 0);
    }

    #[test]
    fn retains_fifteen_minutes_and_downsamples_peaks() {
        let start = Instant::now();
        let mut activity = AgentActivity::default();
        let mut snapshot = RuntimeSnapshot::default();
        activity.record_at(&snapshot, start);
        snapshot.runs.push(run("one", RunState::Running));
        activity.record_at(&snapshot, start + Duration::from_secs(5));
        snapshot.runs.push(run("two", RunState::Running));
        activity.record_at(&snapshot, start + Duration::from_secs(10));
        snapshot.runs.clear();
        activity.record_at(&snapshot, start + Duration::from_secs(15));
        assert_eq!(activity.sparkline_at(2, start + Duration::from_secs(15)), [
            0, 90
        ]);

        activity.record_at(
            &snapshot,
            start + HISTORY_DURATION + Duration::from_secs(11),
        );
        assert_eq!(
            activity.sparkline_at(2, start + HISTORY_DURATION + Duration::from_secs(11)),
            [0, 0]
        );
    }

    #[test]
    fn initializes_history_from_persisted_events_and_completions() {
        let now = Utc::now();
        let mut completed = run("done", RunState::Completed);
        completed.updated_at = now - chrono::Duration::seconds(30);
        let snapshot = RuntimeSnapshot {
            runs: vec![completed],
            run_events: HashMap::from([("done".to_owned(), vec![EventEnvelope {
                run_id: "done".to_owned(),
                sequence: 4,
                timestamp: now - chrono::Duration::minutes(1),
                payload: RunEvent::Output {
                    stream: OutputStream::Pty,
                    text: "finished".to_owned(),
                },
            }])]),
            ..RuntimeSnapshot::default()
        };
        let mut activity = AgentActivity::default();

        activity.initialize(&snapshot);

        assert_eq!(activity.sparkline(3), [0, 0, 20]);
        assert_eq!(activity.last_event_sequences.get("done"), Some(&5));
    }
}
