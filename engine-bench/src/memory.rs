//! Tracks peak memory and CPU time for an engine's process group.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::sleep;

const SAMPLE_EVERY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub pgid: u32,
    pub rss_kb: u64,
    pub cpu_seconds: f64,
}

#[derive(Debug, Default)]
pub struct Tracker {
    peak_kb: u64,
    cpu_by_pid: HashMap<u32, f64>,
}

impl Tracker {
    pub fn record(&mut self, pgid: u32, samples: &[ProcSample]) {
        let mut total_kb = 0;
        for sample in samples.iter().filter(|sample| sample.pgid == pgid) {
            total_kb += sample.rss_kb;
            self.cpu_by_pid.insert(sample.pid, sample.cpu_seconds);
        }
        self.peak_kb = self.peak_kb.max(total_kb);
    }

    pub fn peak_mb(&self) -> f64 {
        self.peak_kb as f64 / 1024.0
    }

    pub fn cpu_seconds(&self) -> f64 {
        self.cpu_by_pid.values().sum()
    }
}

/// Parses `ps -axo pid=,pgid=,rss=,time=`.
pub fn parse_ps(output: &str) -> Vec<ProcSample> {
    output.lines().filter_map(parse_ps_row).collect()
}

fn parse_ps_row(row: &str) -> Option<ProcSample> {
    let mut columns = row.split_whitespace();
    Some(ProcSample {
        pid: columns.next()?.parse().ok()?,
        pgid: columns.next()?.parse().ok()?,
        rss_kb: columns.next()?.parse().ok()?,
        cpu_seconds: parse_cpu_time(columns.next()?)?,
    })
}

/// Parses `ps` CPU time: `0:00.14` on macOS, `[dd-]hh:mm:ss` on Linux.
pub fn parse_cpu_time(text: &str) -> Option<f64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<f64>().ok()?, clock),
        None => (0.0, text),
    };
    if !clock.contains(':') {
        return None;
    }
    let mut seconds = 0.0;
    for part in clock.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

/// Reads proportional memory from `/proc/<pid>/smaps_rollup`. Unlike RSS it
/// splits shared pages between processes, so multi-process browsers add up.
pub fn parse_pss(smaps_rollup: &str) -> Option<u64> {
    smaps_rollup.lines().find_map(|line| {
        line.strip_prefix("Pss:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// Reads CPU seconds from `/proc/<pid>/stat`. Linux `ps` only reports whole
/// seconds, which rounds short-lived browser processes down to zero.
pub fn parse_stat_cpu(stat: &str, ticks_per_second: f64) -> Option<f64> {
    // The process name can hold spaces, so count fields after its closing bracket.
    let (_, fields) = stat.rsplit_once(')')?;
    let mut times = fields.split_whitespace().skip(11);
    let user: f64 = times.next()?.parse().ok()?;
    let system: f64 = times.next()?.parse().ok()?;
    Some((user + system) / ticks_per_second)
}

async fn read_group(pgid: u32) -> Vec<ProcSample> {
    let Ok(output) = Command::new("ps")
        .args([
            "-ax", "-o", "pid=", "-o", "pgid=", "-o", "rss=", "-o", "time=",
        ])
        .output()
        .await
    else {
        return Vec::new();
    };

    let mut samples = parse_ps(&String::from_utf8_lossy(&output.stdout));
    samples.retain(|sample| sample.pgid == pgid);
    if cfg!(target_os = "linux") {
        // SAFETY: `sysconf` takes a constant and touches no memory of ours.
        let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        for sample in &mut samples {
            let rollup = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", sample.pid));
            if let Some(pss_kb) = rollup.ok().as_deref().and_then(parse_pss) {
                sample.rss_kb = pss_kb;
            }
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", sample.pid));
            let cpu = stat
                .ok()
                .and_then(|stat| parse_stat_cpu(&stat, ticks_per_second));
            if let Some(cpu_seconds) = cpu {
                sample.cpu_seconds = cpu_seconds;
            }
        }
    }
    samples
}

/// Watches a process group in the background until `finish` is called.
pub struct Sampler {
    pgid: u32,
    tracker: Arc<Mutex<Tracker>>,
    task: JoinHandle<()>,
}

impl Sampler {
    pub fn start(pgid: u32) -> Self {
        let tracker = Arc::new(Mutex::new(Tracker::default()));
        let shared = tracker.clone();
        let task = tokio::spawn(async move {
            loop {
                let samples = read_group(pgid).await;
                shared.lock().unwrap().record(pgid, &samples);
                sleep(SAMPLE_EVERY).await;
            }
        });
        Self {
            pgid,
            tracker,
            task,
        }
    }

    /// Returns peak megabytes and CPU seconds, with one last reading so very
    /// short attempts still register.
    pub async fn finish(self) -> (f64, f64) {
        self.task.abort();
        let samples = read_group(self.pgid).await;
        let mut tracker = self.tracker.lock().unwrap();
        tracker.record(self.pgid, &samples);
        (tracker.peak_mb(), tracker.cpu_seconds())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, pgid: u32, rss_kb: u64, cpu_seconds: f64) -> ProcSample {
        ProcSample {
            pid,
            pgid,
            rss_kb,
            cpu_seconds,
        }
    }

    #[test]
    fn parses_macos_cpu_time() {
        assert_eq!(parse_cpu_time("0:00.14"), Some(0.14));
        assert_eq!(parse_cpu_time("2:03.50"), Some(123.5));
    }

    #[test]
    fn parses_linux_cpu_time() {
        assert_eq!(parse_cpu_time("00:00:12"), Some(12.0));
        assert_eq!(parse_cpu_time("01:02:03"), Some(3723.0));
    }

    #[test]
    fn parses_linux_cpu_time_with_days() {
        assert_eq!(parse_cpu_time("1-00:00:01"), Some(86401.0));
    }

    #[test]
    fn rejects_malformed_cpu_time() {
        assert_eq!(parse_cpu_time("soon"), None);
        assert_eq!(parse_cpu_time(""), None);
    }

    #[test]
    fn parses_ps_rows_and_skips_bad_ones() {
        let output = "  101   100  2048   0:01.50\nnot a row\n  102   100  1024   0:00.25\n";
        assert_eq!(
            parse_ps(output),
            vec![sample(101, 100, 2048, 1.5), sample(102, 100, 1024, 0.25)]
        );
    }

    const STAT: &str = "42 (chromium) S 1 42 42 0 -1 4194560 100 0 0 0 150 50 0 0 20 0 9 0 12345";

    #[test]
    fn adds_user_and_system_time_from_proc_stat() {
        assert_eq!(parse_stat_cpu(STAT, 100.0), Some(2.0));
    }

    #[test]
    fn process_names_with_spaces_and_brackets_do_not_shift_the_fields() {
        let stat = STAT.replace("(chromium)", "(Web (Content) x)");
        assert_eq!(parse_stat_cpu(&stat, 100.0), Some(2.0));
    }

    #[test]
    fn truncated_proc_stat_is_none() {
        assert_eq!(parse_stat_cpu("42 (chromium) S 1", 100.0), None);
        assert_eq!(parse_stat_cpu("", 100.0), None);
    }

    #[test]
    fn reads_pss_from_smaps_rollup() {
        let rollup =
            "00400000-7fff [rollup]\nRss:    2048 kB\nPss:    1024 kB\nPss_Anon:   512 kB\n";
        assert_eq!(parse_pss(rollup), Some(1024));
    }

    #[test]
    fn missing_pss_is_none() {
        assert_eq!(parse_pss("Rss:  2048 kB\n"), None);
    }

    #[test]
    fn peak_sums_only_the_engines_process_group() {
        let mut tracker = Tracker::default();
        tracker.record(
            100,
            &[
                sample(101, 100, 2048, 0.0),
                sample(102, 100, 2048, 0.0),
                sample(900, 900, 999_999, 0.0),
            ],
        );
        assert_eq!(tracker.peak_mb(), 4.0);
    }

    #[test]
    fn peak_keeps_the_highest_total_seen() {
        let mut tracker = Tracker::default();
        tracker.record(100, &[sample(101, 100, 4096, 0.0)]);
        tracker.record(100, &[sample(101, 100, 1024, 0.0)]);
        assert_eq!(tracker.peak_mb(), 4.0);
    }

    #[test]
    fn cpu_time_survives_a_process_exiting_between_samples() {
        let mut tracker = Tracker::default();
        tracker.record(100, &[sample(101, 100, 1, 1.0), sample(102, 100, 1, 2.0)]);
        tracker.record(100, &[sample(101, 100, 1, 1.5)]);
        assert_eq!(tracker.cpu_seconds(), 3.5);
    }
}
