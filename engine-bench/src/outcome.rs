//! Turns what a page looks like into the result of an attempt.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long a finished page must stay put before it counts as having no
/// challenge. Some sites swap in the challenge after the first load.
pub const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The challenge cleared on its own and `cf_clearance` was issued.
    Cleared,
    /// The real page loaded without a challenge appearing.
    NoChallenge,
    /// A block page, which no amount of waiting will clear.
    Blocked,
    /// A challenge stayed on screen until the timeout.
    ChallengeTimeout,
    /// A challenge showed an interactive widget. This harness never clicks it.
    InteractiveTimeout,
    /// Nothing usable loaded before the timeout.
    LoadTimeout,
    /// The engine crashed or lacks a CDP command the harness needs.
    EngineError,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageSnapshot {
    pub title: String,
    pub href: String,
    #[serde(rename = "readyState")]
    pub ready_state: String,
    pub body: String,
    /// The page defines `window._cf_chl_opt`, which only challenge pages do.
    #[serde(rename = "chlOpt")]
    pub chl_opt: bool,
    pub turnstile: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Waiting,
    Done(Outcome),
}

const CHALLENGE_TITLES: [&str; 2] = ["just a moment...", "ddos-guard"];
const BLOCK_TITLES: [&str; 2] = ["Access denied", "Attention Required! | Cloudflare"];

impl PageSnapshot {
    fn is_challenge(&self) -> bool {
        self.chl_opt || CHALLENGE_TITLES.contains(&self.title.trim().to_lowercase().as_str())
    }

    fn is_block(&self) -> bool {
        BLOCK_TITLES.contains(&self.title.trim()) || self.body.trim() == "error code: 1020"
    }

    /// True for a fully loaded page that isn't a challenge.
    pub fn looks_finished(&self) -> bool {
        self.ready_state == "complete" && self.href != "about:blank" && !self.is_challenge()
    }
}

/// `settled` is how long the page has looked finished, measured by the caller.
pub fn classify(page: &PageSnapshot, has_clearance: bool, settled: Duration) -> Verdict {
    if page.is_challenge() {
        Verdict::Waiting
    } else if page.is_block() {
        Verdict::Done(Outcome::Blocked)
    } else if has_clearance {
        Verdict::Done(Outcome::Cleared)
    } else if page.looks_finished() && settled >= SETTLE {
        Verdict::Done(Outcome::NoChallenge)
    } else {
        Verdict::Waiting
    }
}

pub fn at_deadline(last: Option<&PageSnapshot>) -> Outcome {
    match last {
        Some(page) if page.is_challenge() && page.turnstile => Outcome::InteractiveTimeout,
        Some(page) if page.is_challenge() => Outcome::ChallengeTimeout,
        _ => Outcome::LoadTimeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str) -> PageSnapshot {
        PageSnapshot {
            title: title.into(),
            href: "https://indexer.test/".into(),
            ready_state: "complete".into(),
            ..Default::default()
        }
    }

    fn challenge() -> PageSnapshot {
        page("Just a moment...")
    }

    #[test]
    fn challenge_page_keeps_waiting() {
        assert_eq!(classify(&challenge(), false, SETTLE), Verdict::Waiting);
    }

    #[test]
    fn challenge_page_keeps_waiting_even_with_the_cookie_set() {
        assert_eq!(classify(&challenge(), true, SETTLE), Verdict::Waiting);
    }

    #[test]
    fn translated_challenge_is_recognised_by_its_script_marker() {
        let translated = PageSnapshot {
            chl_opt: true,
            ..page("Nur einen Moment...")
        };
        assert_eq!(classify(&translated, false, SETTLE), Verdict::Waiting);
    }

    #[test]
    fn real_page_with_clearance_is_cleared() {
        assert_eq!(
            classify(&page("Indexer"), true, Duration::ZERO),
            Verdict::Done(Outcome::Cleared)
        );
    }

    #[test]
    fn block_titles_are_blocked() {
        for title in ["Access denied", "Attention Required! | Cloudflare"] {
            assert_eq!(
                classify(&page(title), false, Duration::ZERO),
                Verdict::Done(Outcome::Blocked),
                "{title}"
            );
        }
    }

    #[test]
    fn error_1020_body_is_blocked() {
        let blocked = PageSnapshot {
            body: " error code: 1020\n".into(),
            ..page("")
        };
        assert_eq!(
            classify(&blocked, false, Duration::ZERO),
            Verdict::Done(Outcome::Blocked)
        );
    }

    #[test]
    fn finished_page_waits_out_the_settle_time() {
        assert_eq!(
            classify(&page("Indexer"), false, Duration::from_millis(100)),
            Verdict::Waiting
        );
    }

    #[test]
    fn finished_page_after_settling_has_no_challenge() {
        assert_eq!(
            classify(&page("Indexer"), false, SETTLE),
            Verdict::Done(Outcome::NoChallenge)
        );
    }

    #[test]
    fn page_still_loading_keeps_waiting() {
        let loading = PageSnapshot {
            ready_state: "interactive".into(),
            ..page("Indexer")
        };
        assert_eq!(classify(&loading, false, SETTLE), Verdict::Waiting);
    }

    #[test]
    fn blank_page_keeps_waiting() {
        let blank = PageSnapshot {
            href: "about:blank".into(),
            ..page("")
        };
        assert_eq!(classify(&blank, false, SETTLE), Verdict::Waiting);
    }

    #[test]
    fn deadline_on_a_challenge_is_a_challenge_timeout() {
        assert_eq!(at_deadline(Some(&challenge())), Outcome::ChallengeTimeout);
    }

    #[test]
    fn deadline_on_a_turnstile_widget_is_an_interactive_timeout() {
        let interactive = PageSnapshot {
            turnstile: true,
            ..challenge()
        };
        assert_eq!(at_deadline(Some(&interactive)), Outcome::InteractiveTimeout);
    }

    #[test]
    fn deadline_with_nothing_loaded_is_a_load_timeout() {
        assert_eq!(at_deadline(None), Outcome::LoadTimeout);
    }

    #[test]
    fn turnstile_on_a_normal_page_is_not_an_interactive_timeout() {
        let login_form = PageSnapshot {
            turnstile: true,
            ..page("Log in")
        };
        assert_eq!(at_deadline(Some(&login_form)), Outcome::LoadTimeout);
    }
}
