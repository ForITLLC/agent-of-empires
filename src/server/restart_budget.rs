//! A ceiling on how often the daemon may restart one session on its own.
//!
//! An automatic restart is the daemon waking a session: it launches a process,
//! resumes a conversation, and re-reads a goal record, which is billable work
//! nobody asked for at that moment. One session on this fleet reached 371 of
//! them and nothing said so, because the only counter was a lifetime tally in
//! a probe's private state file and nothing ever read it back.
//!
//! The per-restart throttle that existed is a rate, and a rate cannot notice a
//! loop: 30 minutes apart forever is still forever. This is a BUDGET over a
//! window, so a session that keeps coming back exhausts it and the daemon
//! REFUSES with a reason on the record instead of continuing quietly.
//!
//! Refusal is deliberately not silence. A loop that stops without announcing
//! itself is the same failure one layer down: the next person still cannot see
//! why the session is not restarting.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::util::now_ms;

/// How many daemon-initiated restarts one session may take inside
/// [`WINDOW_MS`]. Six per hour is far above any healthy recovery (a real
/// crash-and-resume is one, occasionally two) and far below a loop.
pub const MAX_RESTARTS_PER_WINDOW: usize = 6;

/// The window the budget is counted over.
pub const WINDOW_MS: u64 = 60 * 60 * 1000;

/// How many daemon-initiated restarts one session may accumulate in TOTAL
/// before a human has to look at it.
///
/// A rate ceiling alone does not work, and the 371-restart session is the
/// proof: they arrived roughly 30 minutes apart, which is two per hour and
/// under any sane hourly budget forever. A rate cannot distinguish "recovering
/// occasionally" from "looping slowly", because the only difference is that
/// the slow loop never ends. So the budget has a second dimension that does
/// not slide: a session the daemon has restarted this many times is not
/// recovering, whatever the spacing, and the next restart waits for a human.
///
/// Cleared by [`RestartBudget::clear`], which a human restart calls, so this
/// bounds the daemon and never the operator.
pub const MAX_RESTARTS_TOTAL: usize = 20;

/// Why a restart was refused, in words the next reader can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExhausted {
    pub session_id: String,
    pub restarts_in_window: usize,
    pub restarts_total: usize,
    pub window_minutes: u64,
    pub reason: String,
}

#[derive(Default)]
struct SessionBudget {
    /// Restart timestamps inside the sliding window.
    stamps: Vec<u64>,
    /// Restarts since the last human intervention. Does not slide.
    total: usize,
}

#[derive(Default)]
pub struct RestartBudget {
    inner: Mutex<HashMap<String, SessionBudget>>,
}

impl RestartBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an intent to restart `session_id`, or refuse it.
    ///
    /// `Ok(n)` is the count consumed including this one. `Err` carries the
    /// refusal, which the caller is expected to surface rather than swallow.
    pub fn admit(&self, session_id: &str) -> Result<usize, BudgetExhausted> {
        self.admit_at(session_id, now_ms())
    }

    /// [`admit`] with the clock passed in, so the window logic is testable
    /// without sleeping through an hour.
    pub fn admit_at(&self, session_id: &str, now: u64) -> Result<usize, BudgetExhausted> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let cutoff = now.saturating_sub(WINDOW_MS);
        let b = map.entry(session_id.to_string()).or_default();
        b.stamps.retain(|t| *t >= cutoff);

        let refuse = |in_window: usize, total: usize, why: &str| BudgetExhausted {
            session_id: session_id.to_string(),
            restarts_in_window: in_window,
            restarts_total: total,
            window_minutes: WINDOW_MS / 60_000,
            reason: format!(
                "automatic restart refused: {why}. A session restarting this much is looping, \
                 not recovering; the loop is the thing to fix. A human restart \
                 (`aoe session restart {session_id}`) clears the budget, so this never blocks \
                 an operator."
            ),
        };

        if b.total >= MAX_RESTARTS_TOTAL {
            return Err(refuse(
                b.stamps.len(),
                b.total,
                &format!(
                    "{} automatic restarts since a human last intervened (ceiling {})",
                    b.total, MAX_RESTARTS_TOTAL
                ),
            ));
        }
        if b.stamps.len() >= MAX_RESTARTS_PER_WINDOW {
            return Err(refuse(
                b.stamps.len(),
                b.total,
                &format!(
                    "{} restarts in the last {} minutes (ceiling {})",
                    b.stamps.len(),
                    WINDOW_MS / 60_000,
                    MAX_RESTARTS_PER_WINDOW
                ),
            ));
        }
        b.stamps.push(now);
        b.total += 1;
        Ok(b.stamps.len())
    }

    /// Restarts counted against `session_id` right now. Reported on the
    /// session row so the budget is legible before it runs out.
    pub fn used(&self, session_id: &str) -> usize {
        self.used_at(session_id, now_ms())
    }

    pub fn used_at(&self, session_id: &str, now: u64) -> usize {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let cutoff = now.saturating_sub(WINDOW_MS);
        map.get(session_id)
            .map(|b| b.stamps.iter().filter(|t| **t >= cutoff).count())
            .unwrap_or(0)
    }

    /// Forget a session's history: a human restart is an intervention, and the
    /// budget exists to bound the DAEMON, not the human.
    pub fn clear(&self, session_id: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = WINDOW_MS;

    #[test]
    fn restarts_are_admitted_up_to_the_ceiling_then_refused() {
        let b = RestartBudget::new();
        for i in 1..=MAX_RESTARTS_PER_WINDOW {
            assert_eq!(b.admit_at("s", 1000).unwrap(), i);
        }
        let err = b.admit_at("s", 1000).expect_err("the ceiling did not hold");
        assert_eq!(err.restarts_in_window, MAX_RESTARTS_PER_WINDOW);
    }

    #[test]
    fn the_refusal_says_what_happened_and_what_to_do() {
        // A loop that stops without announcing itself is the same defect one
        // layer down, so the reason is part of the contract, not decoration.
        let b = RestartBudget::new();
        for _ in 0..MAX_RESTARTS_PER_WINDOW {
            b.admit_at("sess-x", 1000).unwrap();
        }
        let err = b.admit_at("sess-x", 1000).unwrap_err();
        assert!(
            err.reason.contains("looping, not recovering"),
            "{}",
            err.reason
        );
        assert!(err.reason.contains("sess-x"), "{}", err.reason);
        assert!(err.reason.contains("aoe session restart"), "{}", err.reason);
    }

    #[test]
    fn the_budget_is_per_session_not_global() {
        let b = RestartBudget::new();
        for _ in 0..MAX_RESTARTS_PER_WINDOW {
            b.admit_at("noisy", 1000).unwrap();
        }
        assert!(b.admit_at("noisy", 1000).is_err());
        assert!(
            b.admit_at("quiet", 1000).is_ok(),
            "one looping session must not deny every other session a restart"
        );
    }

    #[test]
    fn the_window_slides_so_a_healthy_session_recovers_its_budget() {
        let b = RestartBudget::new();
        for _ in 0..MAX_RESTARTS_PER_WINDOW {
            b.admit_at("s", 1000).unwrap();
        }
        assert!(b.admit_at("s", 1000).is_err());
        // An hour later the old restarts are outside the window.
        assert!(b.admit_at("s", 1000 + HOUR + 1).is_ok());
    }

    #[test]
    fn used_reports_the_live_count_before_the_ceiling_is_reached() {
        let b = RestartBudget::new();
        assert_eq!(b.used_at("s", 1000), 0);
        b.admit_at("s", 1000).unwrap();
        b.admit_at("s", 1000).unwrap();
        assert_eq!(b.used_at("s", 1000), 2);
        assert_eq!(
            b.used_at("s", 1000 + HOUR + 1),
            0,
            "the window did not age out"
        );
    }

    #[test]
    fn a_human_intervention_clears_the_budget() {
        let b = RestartBudget::new();
        for _ in 0..MAX_RESTARTS_PER_WINDOW {
            b.admit_at("s", 1000).unwrap();
        }
        assert!(b.admit_at("s", 1000).is_err());
        b.clear("s");
        assert!(
            b.admit_at("s", 1000).is_ok(),
            "the budget bounds the daemon, not the human"
        );
    }

    #[test]
    fn the_ceiling_would_have_caught_the_371_restart_session() {
        // The case this exists for: restarts kept arriving and nothing stopped
        // them. Whatever the spacing, the budget refuses long before 371.
        let b = RestartBudget::new();
        let mut admitted = 0;
        for i in 0..371u64 {
            // 30 minutes apart, which is what the old throttle allowed.
            if b.admit_at("for-support", 1000 + i * 30 * 60 * 1000).is_ok() {
                admitted += 1;
            }
        }
        assert!(
            admitted < 371,
            "every one of the 371 was admitted; the ceiling does nothing"
        );
    }
}
