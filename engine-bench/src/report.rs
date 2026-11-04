//! Summarises attempts into the table used to pick an engine.

use crate::outcome::Outcome;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct AttemptRecord {
    pub engine: String,
    pub target: String,
    pub round: u32,
    pub outcome: Outcome,
    pub startup_ms: Option<u64>,
    pub solve_ms: Option<u64>,
    pub peak_mb: Option<f64>,
    pub cpu_seconds: Option<f64>,
    /// Whether Prowlarr's replay would have got past protection.
    pub replay_ok: Option<bool>,
    pub note: Option<String>,
}

impl AttemptRecord {
    /// Starts as an engine error, for attempts that fail before any result.
    pub fn failed(engine: &str, target: &str, round: u32) -> Self {
        Self {
            engine: engine.to_string(),
            target: target.to_string(),
            round,
            outcome: Outcome::EngineError,
            startup_ms: None,
            solve_ms: None,
            peak_mb: None,
            cpu_seconds: None,
            replay_ok: None,
            note: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EngineSummary {
    pub engine: String,
    pub attempts: usize,
    pub usable: usize,
    pub outcomes: BTreeMap<String, usize>,
    pub median_startup_ms: Option<f64>,
    pub median_solve_ms: Option<f64>,
    pub median_peak_mb: Option<f64>,
    pub median_cpu_seconds: Option<f64>,
}

pub fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        count if count % 2 == 1 => Some(sorted[middle]),
        _ => Some((sorted[middle - 1] + sorted[middle]) / 2.0),
    }
}

pub fn summarize(records: &[AttemptRecord]) -> Vec<EngineSummary> {
    let mut engines: Vec<&str> = Vec::new();
    for record in records {
        if !engines.contains(&record.engine.as_str()) {
            engines.push(&record.engine);
        }
    }

    engines
        .into_iter()
        .map(|engine| {
            let attempts: Vec<&AttemptRecord> = records
                .iter()
                .filter(|record| record.engine == engine)
                .collect();
            summarize_engine(engine, &attempts)
        })
        .collect()
}

fn summarize_engine(engine: &str, attempts: &[&AttemptRecord]) -> EngineSummary {
    let usable: Vec<&AttemptRecord> = attempts
        .iter()
        .copied()
        .filter(|attempt| attempt.replay_ok == Some(true))
        .collect();

    let mut outcomes = BTreeMap::new();
    for attempt in attempts {
        *outcomes.entry(outcome_name(attempt.outcome)).or_insert(0) += 1;
    }

    EngineSummary {
        engine: engine.to_string(),
        attempts: attempts.len(),
        usable: usable.len(),
        outcomes,
        median_startup_ms: median_of(attempts, |attempt| attempt.startup_ms.map(|ms| ms as f64)),
        median_solve_ms: median_of(&usable, |attempt| attempt.solve_ms.map(|ms| ms as f64)),
        median_peak_mb: median_of(attempts, |attempt| attempt.peak_mb),
        median_cpu_seconds: median_of(attempts, |attempt| attempt.cpu_seconds),
    }
}

fn median_of(
    attempts: &[&AttemptRecord],
    value: impl Fn(&AttemptRecord) -> Option<f64>,
) -> Option<f64> {
    let values: Vec<f64> = attempts
        .iter()
        .filter_map(|attempt| value(attempt))
        .collect();
    median(&values)
}

fn outcome_name(outcome: Outcome) -> String {
    serde_json::to_value(outcome)
        .ok()
        .and_then(|name| name.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn render_table(summaries: &[EngineSummary]) -> String {
    let mut table = String::from(
        "| Engine | Usable solves | Median solve | Median peak memory | Median CPU | Median startup | Outcomes |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for summary in summaries {
        let outcomes: Vec<String> = summary
            .outcomes
            .iter()
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        table.push_str(&format!(
            "| {} | {}/{} | {} | {} | {} | {} | {} |\n",
            summary.engine,
            summary.usable,
            summary.attempts,
            shown(summary.median_solve_ms.map(|ms| ms / 1000.0), |s| format!(
                "{s:.1} s"
            )),
            shown(summary.median_peak_mb, |mb| format!("{mb:.0} MB")),
            shown(summary.median_cpu_seconds, |s| format!("{s:.1} s")),
            shown(summary.median_startup_ms, |ms| format!("{ms:.0} ms")),
            outcomes.join(", "),
        ));
    }
    table
}

fn shown(value: Option<f64>, format: impl Fn(f64) -> String) -> String {
    value.map(format).unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(engine: &str, outcome: Outcome, solve_ms: u64, replay_ok: bool) -> AttemptRecord {
        AttemptRecord {
            engine: engine.into(),
            target: "https://indexer.test/".into(),
            round: 1,
            outcome,
            startup_ms: Some(100),
            solve_ms: Some(solve_ms),
            peak_mb: Some(200.0),
            cpu_seconds: Some(1.5),
            replay_ok: Some(replay_ok),
            note: None,
        }
    }

    #[test]
    fn median_of_an_odd_count_is_the_middle_value() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
    }

    #[test]
    fn median_of_an_even_count_averages_the_middle_two() {
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn median_of_nothing_is_none() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn engines_stay_in_first_seen_order() {
        let summaries = summarize(&[
            record("chrome", Outcome::Cleared, 10, true),
            record("byparr", Outcome::Cleared, 10, true),
            record("chrome", Outcome::Cleared, 10, true),
        ]);
        let names: Vec<_> = summaries.iter().map(|s| s.engine.as_str()).collect();
        assert_eq!(names, ["chrome", "byparr"]);
        assert_eq!(summaries[0].attempts, 2);
    }

    #[test]
    fn usable_requires_the_replay_to_pass() {
        let summaries = summarize(&[
            record("chrome", Outcome::Cleared, 10, true),
            record("chrome", Outcome::Cleared, 10, false),
        ]);
        assert_eq!(summaries[0].usable, 1);
    }

    #[test]
    fn solve_time_only_counts_usable_attempts() {
        let summaries = summarize(&[
            record("chrome", Outcome::Cleared, 1_000, true),
            record("chrome", Outcome::ChallengeTimeout, 60_000, false),
        ]);
        assert_eq!(summaries[0].median_solve_ms, Some(1_000.0));
    }

    #[test]
    fn an_engine_that_never_succeeds_has_no_solve_time() {
        let summaries = summarize(&[record("panda", Outcome::EngineError, 5, false)]);
        assert_eq!(summaries[0].median_solve_ms, None);
    }

    #[test]
    fn outcomes_are_counted_by_name() {
        let summaries = summarize(&[
            record("chrome", Outcome::Cleared, 10, true),
            record("chrome", Outcome::Blocked, 10, false),
            record("chrome", Outcome::Blocked, 10, false),
        ]);
        assert_eq!(summaries[0].outcomes["cleared"], 1);
        assert_eq!(summaries[0].outcomes["blocked"], 2);
    }

    #[test]
    fn memory_ignores_attempts_that_did_not_measure_it() {
        let mut unmeasured = record("byparr", Outcome::Cleared, 10, true);
        unmeasured.peak_mb = None;
        let summaries = summarize(&[record("byparr", Outcome::Cleared, 10, true), unmeasured]);
        assert_eq!(summaries[0].median_peak_mb, Some(200.0));
    }

    #[test]
    fn table_shows_one_row_per_engine() {
        let table = render_table(&summarize(&[record(
            "chrome",
            Outcome::Cleared,
            1_500,
            true,
        )]));
        let row = table.lines().find(|line| line.contains("chrome")).unwrap();
        assert!(row.contains("1/1"), "{row}");
        assert!(row.contains("1.5 s"), "{row}");
        assert!(row.contains("200 MB"), "{row}");
    }

    #[test]
    fn table_marks_missing_values() {
        let table = render_table(&summarize(&[record(
            "panda",
            Outcome::EngineError,
            5,
            false,
        )]));
        let row = table.lines().find(|line| line.contains("panda")).unwrap();
        assert!(row.contains("0/1"), "{row}");
        assert!(row.contains('-'), "{row}");
    }
}
