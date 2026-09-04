//! Adaptive polling interval and command channel for session monitoring

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Global count of active session-id poller threads for budget enforcement
static ACTIVE_POLLER_COUNT: AtomicU32 = AtomicU32::new(0);

/// Default ceiling on concurrent session-id poller threads.
///
/// Each session that loses its poller stops refreshing the agent session id
/// shown in its TUI row, so this cap doubles as a "how many concurrent
/// sessions can keep their identity live" budget. A fleet that registers
/// more live sessions than this raises it through
/// `[session] session_id_poller_max_threads`.
pub const DEFAULT_SESSION_ID_POLLER_MAX_THREADS: u32 = 50;

/// The live ceiling, process-wide. Seeded with the default; the configured
/// value is applied once at startup by
/// [`configure_session_id_poller_max_threads`].
static SESSION_ID_POLLER_MAX_THREADS: AtomicU32 =
    AtomicU32::new(DEFAULT_SESSION_ID_POLLER_MAX_THREADS);

/// Apply the configured poller-thread ceiling for this process.
///
/// A value of 0 is treated as "unset" and keeps the default, so an empty or
/// zeroed config key can never freeze every session id.
pub fn configure_session_id_poller_max_threads(max: u32) {
    let max = if max == 0 {
        DEFAULT_SESSION_ID_POLLER_MAX_THREADS
    } else {
        max
    };
    SESSION_ID_POLLER_MAX_THREADS.store(max, Ordering::SeqCst);
}

/// The current poller-thread ceiling.
pub fn session_id_poller_max_threads() -> u32 {
    SESSION_ID_POLLER_MAX_THREADS.load(Ordering::SeqCst)
}

/// `(active, max)` for the session-id poller thread budget.
pub fn session_id_poller_budget() -> (u32, u32) {
    (
        ACTIVE_POLLER_COUNT.load(Ordering::SeqCst),
        session_id_poller_max_threads(),
    )
}

/// True when no further poller thread can be started right now.
///
/// Callers that re-check a dead poller on a timer consult this before
/// attempting a start, so an over-budget fleet does not pay a failed spawn
/// plus a warning line per session per tick.
pub fn session_id_poller_budget_exhausted() -> bool {
    let (active, max) = session_id_poller_budget();
    active >= max
}

/// First retry delay after a poller could not be (re)started.
const POLLER_REPAIR_INITIAL_DELAY: Duration = Duration::from_secs(5);
/// Longest retry delay; the schedule doubles up to this and then holds.
const POLLER_REPAIR_MAX_DELAY: Duration = Duration::from_secs(60);
/// At the capped delay, log a reminder every this many deferrals
/// (60 s × 10 = one line per ten minutes per session).
const POLLER_REPAIR_REMIND_EVERY: u32 = 10;

/// Retry schedule for one session whose session-id poller could not be
/// (re)started — typically because the process-wide thread budget is spent.
///
/// The daemon and the TUI both walk every registered session on a ~2 s
/// tick and ask [`crate::session::Instance::repair_session_id_poller_if_needed`]
/// to replace a missing poller. Without a schedule, a fleet over budget
/// retries and warns for every over-budget session on every tick — the
/// 2026-09-04 fleet logged ~2.5 warning lines per second from two sessions.
/// This state lives on the instance (`#[serde(skip)]`) and is carried across
/// reloads like the poller handle itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollerRepairBackoff {
    next_attempt: Option<Instant>,
    delay: Option<Duration>,
    deferrals: u32,
}

impl PollerRepairBackoff {
    /// True when a repair attempt may run at `now`.
    pub fn due(&self, now: Instant) -> bool {
        match self.next_attempt {
            None => true,
            Some(at) => now >= at,
        }
    }

    /// Record a failed (or skipped-for-budget) attempt at `now` and schedule
    /// the next one: 5 s, then doubling to a 60 s ceiling.
    ///
    /// Returns the new delay when the caller should log — on the first
    /// deferral, on every escalation, and as a periodic reminder once the
    /// delay is capped — and `None` for the quiet in-between deferrals.
    pub fn defer(&mut self, now: Instant) -> Option<Duration> {
        let previous = self.delay;
        let delay = match previous {
            None => POLLER_REPAIR_INITIAL_DELAY,
            Some(d) => (d * 2).min(POLLER_REPAIR_MAX_DELAY),
        };
        self.delay = Some(delay);
        self.next_attempt = Some(now + delay);
        self.deferrals += 1;
        let escalated = previous != Some(delay);
        let reminder =
            delay == POLLER_REPAIR_MAX_DELAY && self.deferrals % POLLER_REPAIR_REMIND_EVERY == 0;
        (escalated || reminder).then_some(delay)
    }

    /// Clear the schedule after a successful start.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Number of consecutive deferrals since the last reset.
    pub fn deferrals(&self) -> u32 {
        self.deferrals
    }

    /// The delay scheduled by the most recent deferral, if any.
    pub fn current_delay(&self) -> Option<Duration> {
        self.delay
    }

    /// Make the next attempt due immediately without clearing the schedule
    /// (tests simulate elapsed time with this).
    #[cfg(test)]
    pub(crate) fn expire(&mut self) {
        self.next_attempt = self
            .next_attempt
            .map(|_| Instant::now() - Duration::from_millis(1));
    }
}

/// RAII guard that decrements `ACTIVE_POLLER_COUNT` on drop.
///
/// Ensures the counter is always decremented even if the poller thread panics,
/// preventing permanent budget exhaustion.
struct PollerCountGuard;

impl PollerCountGuard {
    /// Atomically check the budget and increment. Returns `None` if at capacity.
    fn try_acquire() -> Option<Self> {
        let mut current = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);
        loop {
            if current >= session_id_poller_max_threads() {
                return None;
            }
            match ACTIVE_POLLER_COUNT.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(Self),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for PollerCountGuard {
    fn drop(&mut self) {
        ACTIVE_POLLER_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

const POLL_INITIAL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_MAX_INTERVAL: Duration = Duration::from_secs(60);
const POLL_BACKOFF_FACTOR: f64 = 1.5;
const POLL_STABLE_THRESHOLD: u32 = 3;

/// Manages adaptive polling intervals that back off when no changes are detected
#[derive(Debug)]
struct AdaptiveInterval {
    initial: Duration,
    current: Duration,
    max: Duration,
    backoff_factor: f64,
    stable_threshold: u32,
    stable_count: u32,
}

impl AdaptiveInterval {
    /// Create a new adaptive interval with custom parameters
    fn new(initial: Duration, max: Duration, backoff_factor: f64, stable_threshold: u32) -> Self {
        Self {
            initial,
            current: initial,
            max,
            backoff_factor,
            stable_threshold,
            stable_count: 0,
        }
    }

    fn current(&self) -> Duration {
        self.current
    }

    /// Record that no changes were detected; increases backoff if threshold is reached.
    ///
    /// Uses `Duration::from_secs_f64` for sub-second precision in the backoff calculation
    /// (e.g., 2.0s * 1.5 = 3.0s, 3.0s * 1.5 = 4.5s).
    fn record_no_change(&mut self) {
        self.stable_count += 1;
        if self.stable_count >= self.stable_threshold {
            let next_secs = self.current.as_secs_f64() * self.backoff_factor;
            let next_duration = Duration::from_secs_f64(next_secs);
            self.current = next_duration.min(self.max);
            self.stable_count = 0;
        }
    }

    /// Record that a change was detected; reset to initial interval
    fn record_change(&mut self) {
        self.current = self.initial;
        self.stable_count = 0;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionIdGuard {
    Unguarded,
    /// Pre-launch-marker compat path: OMP sessions captured before the launch
    /// marker generation existed carry no generation to CAS against, so they
    /// persist unguarded. Kept so panes launched by an older aoe stay
    /// attributable across an upgrade; new launches always emit
    /// `OmpGeneration`. Removable once no unmarked OMP pane can outlive an
    /// upgrade.
    OmpLegacy,
    OmpGeneration(String),
    /// Read from a per-instance sidecar the agent itself wrote (Pi's
    /// extension), so the observation names this pane rather than being
    /// inferred from a store. Sources that cannot do that are refused for Pi;
    /// see `session::sync`.
    InstanceSidecar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionIdObservation {
    pub(crate) sid: String,
    pub(crate) guard: SessionIdGuard,
}

pub(crate) type SessionIdPollFn =
    Box<dyn Fn(&str) -> Option<SessionIdObservation> + Send + 'static>;

impl SessionIdObservation {
    pub(crate) fn unguarded(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::Unguarded,
        }
    }

    pub(crate) fn instance_sidecar(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::InstanceSidecar,
        }
    }

    pub(crate) fn omp_legacy(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::OmpLegacy,
        }
    }
    pub(crate) fn omp(sid: String, generation: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::OmpGeneration(generation),
        }
    }
}

/// Command sent to the session poller thread
#[derive(Debug, Clone, Copy)]
enum PollCommand {
    /// Stop the poller thread.
    Stop,
    /// Forget the last report so an observation whose durable write failed or
    /// lost a CAS can be emitted again.
    RetryLast,
}

/// Resolve and observe one poll target. A dead name must remain the resolved
/// target for two consecutive ticks before it terminates the poller: a tmux
/// rename between resolution and the liveness probe makes the old name look
/// absent for one tick, while the next tick can resolve the renamed pane.
fn poll_resolved_target<T>(
    instance_id: &str,
    initial_session_name: &str,
    resolve_target: impl FnOnce(&str, &str) -> String,
    is_pane_dead: impl FnOnce(&str) -> bool,
    observe: impl FnOnce(&str) -> Option<T>,
    dead_candidate: &mut Option<String>,
) -> (String, bool, Option<T>) {
    let target = resolve_target(instance_id, initial_session_name);
    if is_pane_dead(&target) {
        let should_stop = dead_candidate.as_deref() == Some(target.as_str());
        if !should_stop {
            *dead_candidate = Some(target.clone());
        }
        return (target, should_stop, None);
    }

    *dead_candidate = None;
    let observation = observe(&target);
    (target, false, observation)
}

/// Manages polling thread lifecycle and inter-thread communication via mpsc channels.
///
/// # Cleanup
///
/// Cleanup is performed explicitly via `stop()` rather than `Drop` because
/// `Drop` alone cannot guarantee prompt shutdown. The poller thread holds
/// the `cmd_rx` receiver; when `SessionPoller` drops, the corresponding
/// `cmd_tx` sender is dropped too and `recv_timeout` returns `Disconnected`
/// immediately; so in the common case the thread exits promptly.
///
/// `stop()` sends an explicit `PollCommand::Stop` and joins the thread,
/// providing a deterministic shutdown path for callers like `Instance::kill`
/// and `Instance::restart_with_size`.
pub struct SessionPoller {
    session_name: String,
    cmd_tx: mpsc::Sender<PollCommand>,
    cmd_rx: Option<mpsc::Receiver<PollCommand>>,
    result_tx: mpsc::Sender<(String, SessionIdObservation)>,
    result_rx: Option<mpsc::Receiver<(String, SessionIdObservation)>>,
    pending_observation: Option<(String, SessionIdObservation)>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SessionPoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPoller")
            .field("session_name", &self.session_name)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

impl SessionPoller {
    /// Create a new poller (does not start the thread)
    pub fn new(session_name: String) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            session_name,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            result_tx,
            result_rx: Some(result_rx),
            pending_observation: None,
            handle: None,
        }
    }

    /// Start the polling thread with the given callbacks.
    ///
    /// Returns `true` if the thread was successfully spawned, `false` if the
    /// poller was already started, the thread budget was exhausted, or spawning failed.
    pub fn start(
        &mut self,
        instance_id: String,
        poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static>,
        on_change: Box<dyn Fn(&str) + Send + 'static>,
        initial_known: Option<String>,
    ) -> bool {
        self.start_observations(
            instance_id,
            Box::new(move |_| poll_fn().map(SessionIdObservation::unguarded)),
            on_change,
            initial_known.map(SessionIdObservation::unguarded),
        )
    }

    pub(crate) fn start_observations(
        &mut self,
        instance_id: String,
        poll_fn: SessionIdPollFn,
        on_change: Box<dyn Fn(&str) + Send + 'static>,
        initial_known: Option<SessionIdObservation>,
    ) -> bool {
        let cmd_rx = match self.cmd_rx.take() {
            Some(rx) => rx,
            None => {
                tracing::warn!(target: "session.create",
                    "Poller for {} already started, ignoring duplicate start",
                    instance_id
                );
                return false;
            }
        };

        let _guard = match PollerCountGuard::try_acquire() {
            Some(g) => g,
            None => {
                let (active, max) = session_id_poller_budget();
                tracing::warn!(target: "session.create",
                    "Session-id poller budget exhausted ({}/{}), skipping poller for {}; \
                     its session id will not refresh until another session stops \
                     (raise [session] session_id_poller_max_threads for larger fleets)",
                    active,
                    max,
                    instance_id,
                );
                self.cmd_rx = Some(cmd_rx);
                return false;
            }
        };

        let initial_session_name = self.session_name.clone();
        let thread_label = format!("aoe-poller/{}", instance_id);
        let result_tx = self.result_tx.clone();

        let handle = std::thread::Builder::new()
            .name(thread_label.clone())
            .stack_size(128 * 1024)
            .spawn(move || {
                // Rebind so the closure captures `_guard` and the counter only
                // decrements when the thread exits (including via panic). Without
                // this, `move` would not capture an unreferenced binding and the
                // counter would decrement as soon as `start()` returned.
                let _guard = _guard;

                let mut last_known = initial_known;
                let mut interval = AdaptiveInterval::new(
                    POLL_INITIAL_INTERVAL,
                    POLL_MAX_INTERVAL,
                    POLL_BACKOFF_FACTOR,
                    POLL_STABLE_THRESHOLD,
                );

                let report = |new_observation: Option<SessionIdObservation>,
                              last: &mut Option<SessionIdObservation>,
                              interval: &mut AdaptiveInterval| {
                    match new_observation {
                        Some(observation) if last.as_ref() != Some(&observation) => {
                            on_change(&observation.sid);
                            let _ =
                                result_tx.send((instance_id.clone(), observation.clone()));
                            *last = Some(observation);
                            interval.record_change();
                        }
                        _ => interval.record_no_change(),
                    }
                };

                let mut dead_candidate = None;
                let mut poll_tick = || {
                    poll_resolved_target(
                        &instance_id,
                        &initial_session_name,
                        crate::tmux::live_agent_session_name,
                        crate::tmux::utils::is_pane_dead,
                        |target| poll_fn(target),
                        &mut dead_candidate,
                    )
                };

                // Immediate first poll (e.g. pre-existing sessions loaded from disk).
                let (target, should_stop, observation) = poll_tick();
                if should_stop {
                    tracing::info!(target: "session.create", "Pane dead for {}, stopping poller", target);
                    return;
                }
                report(observation, &mut last_known, &mut interval);
                loop {
                    match cmd_rx.recv_timeout(interval.current()) {
                        Ok(PollCommand::Stop) => {
                            // Capture the pane's final observable state before
                            // the owner joins and drains this poller. Restart
                            // relies on this boundary to preserve a session
                            // switch that landed just before teardown.
                            let (_, should_stop, observation) = poll_tick();
                            if !should_stop {
                                report(observation, &mut last_known, &mut interval);
                            }
                            break;
                        }
                        Ok(PollCommand::RetryLast) => {
                            last_known = None;
                            interval.record_change();
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }

                    let (target, should_stop, observation) = poll_tick();
                    if should_stop {
                        tracing::info!(target: "session.create", "Pane dead for {}, stopping poller", target);
                        break;
                    }

                    report(observation, &mut last_known, &mut interval);
                }
            });

        match handle {
            Ok(h) => {
                self.handle = Some(h);
                true
            }
            Err(e) => {
                tracing::warn!(target: "session.create", "Failed to spawn poller thread {}: {}", thread_label, e);
                // Restore channels to allow retrying spawn
                let (cmd_tx, cmd_rx) = mpsc::channel();
                self.cmd_tx = cmd_tx;
                self.cmd_rx = Some(cmd_rx);
                let (result_tx, result_rx) = mpsc::channel();
                self.result_tx = result_tx;
                self.result_rx = Some(result_rx);
                self.pending_observation = None;
                false
            }
        }
    }

    pub(crate) fn try_recv_observation(&self) -> Option<(String, SessionIdObservation)> {
        self.result_rx.as_ref()?.try_recv().ok()
    }

    /// Drain newly queued observations into a sticky one-slot mailbox and
    /// lease the newest value without consuming it. The value remains
    /// available to a concurrent stop/final flush until a terminal outcome
    /// acknowledges it.
    pub(crate) fn latest_observation(&mut self) -> Option<(String, SessionIdObservation)> {
        while let Some(observation) = self.try_recv_observation() {
            self.pending_observation = Some(observation);
        }
        self.pending_observation.clone()
    }

    /// Acknowledge only the observation that reached a terminal outcome. A
    /// stale writer must not erase a newer correction queued in the meantime.
    pub(crate) fn acknowledge_observation(
        &mut self,
        expected: &(String, SessionIdObservation),
    ) -> bool {
        if self.pending_observation.as_ref() == Some(expected) {
            self.pending_observation = None;
            true
        } else {
            false
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_test_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::unguarded(session_id.to_string()),
            ))
            .expect("inject_test_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_sidecar_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::instance_sidecar(session_id.to_string()),
            ))
            .expect("inject_test_sidecar_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_omp_update(
        &self,
        instance_id: &str,
        session_id: &str,
        generation: &str,
    ) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::omp(session_id.to_string(), generation.to_string()),
            ))
            .expect("inject_test_omp_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_omp_legacy_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::omp_legacy(session_id.to_string()),
            ))
            .expect("inject_test_omp_legacy_update: result channel disconnected");
    }

    pub(crate) fn retry_last_observation(&self) {
        let _ = self.cmd_tx.send(PollCommand::RetryLast);
    }

    /// Stop the poller thread and wait for it to finish
    pub fn stop(&mut self) {
        let _ = self.cmd_tx.send(PollCommand::Stop);
        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.join() {
                tracing::warn!(target: "session.create", "Poller thread panicked: {:?}", e);
            }
        }
    }

    /// Check if the poller thread is running
    pub fn is_running(&self) -> bool {
        match &self.handle {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }
}

impl Default for SessionPoller {
    fn default() -> Self {
        Self::new("default".to_string())
    }
}

/// Test-only handles on the process-wide budget, for callers outside this
/// module (instance repair tests) that must drive it deterministically.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Pins the budget to "spent" (ceiling 1, one slot taken) and restores
    /// both counters on drop. Pair with `#[serial]`: the budget is global.
    pub(crate) struct BudgetExhausted {
        prev_count: u32,
        prev_max: u32,
    }

    impl BudgetExhausted {
        pub(crate) fn pin() -> Self {
            let prev_count = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);
            let prev_max = session_id_poller_max_threads();
            SESSION_ID_POLLER_MAX_THREADS.store(1, Ordering::SeqCst);
            ACTIVE_POLLER_COUNT.store(1, Ordering::SeqCst);
            Self {
                prev_count,
                prev_max,
            }
        }
    }

    impl Drop for BudgetExhausted {
        fn drop(&mut self) {
            ACTIVE_POLLER_COUNT.store(self.prev_count, Ordering::SeqCst);
            SESSION_ID_POLLER_MAX_THREADS.store(self.prev_max, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::{Arc, Mutex, MutexGuard};

    fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Restores `ACTIVE_POLLER_COUNT` to its original value on drop.
    struct CountRestorer(u32);
    impl Drop for CountRestorer {
        fn drop(&mut self) {
            ACTIVE_POLLER_COUNT.store(self.0, Ordering::SeqCst);
        }
    }

    /// Restores the configured ceiling to its original value on drop.
    struct MaxRestorer(u32);
    impl Drop for MaxRestorer {
        fn drop(&mut self) {
            SESSION_ID_POLLER_MAX_THREADS.store(self.0, Ordering::SeqCst);
        }
    }

    #[test]
    #[serial]
    fn configured_ceiling_bounds_the_budget() {
        let _restore_max = MaxRestorer(session_id_poller_max_threads());
        let _restore_count = CountRestorer(ACTIVE_POLLER_COUNT.load(Ordering::SeqCst));

        configure_session_id_poller_max_threads(3);
        assert_eq!(session_id_poller_max_threads(), 3);
        ACTIVE_POLLER_COUNT.store(2, Ordering::SeqCst);
        assert!(!session_id_poller_budget_exhausted());
        assert_eq!(session_id_poller_budget(), (2, 3));
        let guard = PollerCountGuard::try_acquire();
        assert!(
            guard.is_some(),
            "third slot is within the configured ceiling"
        );
        assert!(session_id_poller_budget_exhausted());
        assert!(
            PollerCountGuard::try_acquire().is_none(),
            "fourth slot exceeds the configured ceiling"
        );
        drop(guard);
        assert!(!session_id_poller_budget_exhausted());

        // Raising the ceiling at runtime admits the waiting session.
        ACTIVE_POLLER_COUNT.store(50, Ordering::SeqCst);
        configure_session_id_poller_max_threads(400);
        assert!(!session_id_poller_budget_exhausted());
        assert!(PollerCountGuard::try_acquire().is_some());
    }

    #[test]
    #[serial]
    fn zero_ceiling_keeps_the_default() {
        let _restore_max = MaxRestorer(session_id_poller_max_threads());
        configure_session_id_poller_max_threads(0);
        assert_eq!(
            session_id_poller_max_threads(),
            DEFAULT_SESSION_ID_POLLER_MAX_THREADS
        );
    }

    #[test]
    fn repair_backoff_doubles_to_a_minute_and_holds() {
        let mut b = PollerRepairBackoff::default();
        let now = Instant::now();
        assert!(b.due(now), "a fresh schedule is due immediately");

        let expected = [5u64, 10, 20, 40, 60, 60, 60];
        for (i, secs) in expected.iter().enumerate() {
            let logged = b.defer(now);
            assert_eq!(
                b.current_delay(),
                Some(Duration::from_secs(*secs)),
                "deferral {} should schedule {}s",
                i + 1,
                secs
            );
            assert_eq!(b.deferrals(), i as u32 + 1);
            let should_log = i < 5; // first + four escalations; then quiet
            assert_eq!(
                logged.is_some(),
                should_log,
                "deferral {} log decision",
                i + 1
            );
            assert!(!b.due(now), "just deferred: not due at the same instant");
            assert!(
                b.due(now + Duration::from_secs(*secs)),
                "due once the scheduled delay has elapsed"
            );
            assert!(
                !b.due(now + Duration::from_secs(*secs) - Duration::from_millis(1)),
                "not due one millisecond early"
            );
        }
    }

    #[test]
    fn repair_backoff_reminds_every_tenth_deferral_at_the_cap() {
        let mut b = PollerRepairBackoff::default();
        let now = Instant::now();
        // Walk to the cap: deferrals 1..=5 log (5,10,20,40,60).
        for _ in 0..5 {
            b.defer(now);
        }
        let mut logged_at = Vec::new();
        for _ in 0..25 {
            if b.defer(now).is_some() {
                logged_at.push(b.deferrals());
            }
        }
        assert_eq!(logged_at, vec![10, 20, 30]);
    }

    #[test]
    fn repair_backoff_reset_clears_the_schedule() {
        let mut b = PollerRepairBackoff::default();
        let now = Instant::now();
        b.defer(now);
        b.defer(now);
        assert!(!b.due(now));
        b.reset();
        assert_eq!(b, PollerRepairBackoff::default());
        assert!(b.due(now));
        assert_eq!(b.defer(now), Some(Duration::from_secs(5)), "restarts at 5s");
    }

    #[test]
    fn test_adaptive_interval_initial() {
        let interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        assert_eq!(interval.current(), Duration::from_secs(2));
    }

    #[test]
    fn test_adaptive_interval_record_no_change_increments_count() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        assert_eq!(interval.stable_count, 0);
        interval.record_no_change();
        assert_eq!(interval.stable_count, 1);
        interval.record_no_change();
        assert_eq!(interval.stable_count, 2);
    }

    #[test]
    fn test_adaptive_interval_backoff_at_threshold() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        interval.record_no_change();
        interval.record_no_change();
        interval.record_no_change();
        // After 3 calls: 2 * 1.5 = 3 seconds
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_adaptive_interval_multiple_backoffs() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        // First backoff: 2 -> 3
        for _ in 0..3 {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));

        // Second backoff: 3 -> 4.5 (with sub-second precision)
        for _ in 0..3 {
            interval.record_no_change();
        }
        let expected_secs = 3.0 * 1.5;
        assert_eq!(interval.current(), Duration::from_secs_f64(expected_secs));
    }

    #[test]
    fn test_adaptive_interval_respects_max() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            1, // threshold of 1 for faster test
        );
        interval.record_no_change(); // 2 * 1.5 = 3.0
        interval.record_no_change(); // 3.0 * 1.5 = 4.5
        interval.record_no_change(); // 4.5 * 1.5 = 6.75
        interval.record_no_change(); // 6.75 * 1.5 = 10.125
        interval.record_no_change(); // 10.125 * 1.5 = 15.1875
        interval.record_no_change(); // 15.1875 * 1.5 = 22.78125
        interval.record_no_change(); // 22.78125 * 1.5 = 34.171875
        interval.record_no_change(); // 34.171875 * 1.5 = 51.2578125
        interval.record_no_change(); // 51.2578125 * 1.5 = 76.88671875 > 60, capped at 60
        assert!(interval.current() <= Duration::from_secs(60));
    }

    #[test]
    fn test_adaptive_interval_record_change_resets() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        for _ in 0..3 {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));

        interval.record_change();
        assert_eq!(interval.current(), Duration::from_secs(2));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_session_poller_new() {
        let poller = SessionPoller::new("test-session".to_string());
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_stop_when_no_thread() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.stop(); // Should not panic
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_double_stop_safe() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.stop();
        poller.stop(); // Should not panic
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_drop_is_clean() {
        let poller = SessionPoller::new("test-session".to_string());
        drop(poller); // Should not panic
    }

    #[test]
    fn rename_between_resolution_and_liveness_retries_the_new_name() {
        let initial = "aoe_Old_12345678";
        let renamed = "aoe_New_12345678";
        let current = std::cell::RefCell::new(initial.to_string());
        let resolution_count = std::cell::Cell::new(0);
        let observed = std::cell::RefCell::new(Vec::new());
        let mut dead_candidate = None;

        let (target, should_stop, observation) = poll_resolved_target(
            "12345678abcdef",
            initial,
            |_, derived| {
                assert_eq!(derived, initial);
                resolution_count.set(resolution_count.get() + 1);
                current.borrow().clone()
            },
            |target| {
                assert_eq!(target, initial);
                *current.borrow_mut() = renamed.to_string();
                true
            },
            |_| -> Option<&str> {
                panic!("a name that became dead during the tick must not be observed")
            },
            &mut dead_candidate,
        );
        assert_eq!(target, initial);
        assert!(!should_stop, "one dead tick may be an in-flight rename");
        assert!(observation.is_none());

        let (target, should_stop, observation) = poll_resolved_target(
            "12345678abcdef",
            initial,
            |_, _| {
                resolution_count.set(resolution_count.get() + 1);
                current.borrow().clone()
            },
            |_| false,
            |target| {
                observed.borrow_mut().push(target.to_string());
                Some("sid-after-rename")
            },
            &mut dead_candidate,
        );
        assert_eq!(target, renamed);
        assert!(!should_stop);
        assert_eq!(observation, Some("sid-after-rename"));
        assert_eq!(observed.into_inner(), vec![renamed.to_string()]);
        assert_eq!(resolution_count.get(), 2, "one resolution per tick");
        assert!(dead_candidate.is_none());
    }

    #[test]
    fn test_adaptive_interval_with_constants() {
        let mut interval = AdaptiveInterval::new(
            POLL_INITIAL_INTERVAL,
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );
        assert_eq!(interval.current(), Duration::from_secs(2));
        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));
    }

    #[test]
    fn test_poller_detects_change() {
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = lock_unpoisoned(&call_count_clone);
            *count += 1;
            if *count <= 1 {
                Some("id-1".to_string())
            } else {
                Some("id-2".to_string())
            }
        });

        let changed_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let changed_ids_clone = changed_ids.clone();
        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |id: &str| {
            lock_unpoisoned(&changed_ids_clone).push(id.to_string());
        });

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-change".to_string(),
            poll_fn,
            on_change,
            Some("id-1".to_string()),
        );

        // Wait for the adaptive interval (2s initial) to fire at least once.
        std::thread::sleep(Duration::from_millis(2500));
        poller.retry_last_observation();
        std::thread::sleep(Duration::from_millis(100));
        poller.stop();

        let ids = lock_unpoisoned(&changed_ids);
        assert!(
            !ids.contains(&"id-1".to_string()),
            "on_change should NOT have been called with id-1 (initial known)"
        );
        assert_eq!(
            ids.iter().filter(|id| id.as_str() == "id-2").count(),
            2,
            "a failed durable write must be able to request the same observation again"
        );
    }

    #[test]
    #[serial]
    fn test_thread_budget_cap() {
        let _restore_count = CountRestorer(ACTIVE_POLLER_COUNT.load(Ordering::SeqCst));
        ACTIVE_POLLER_COUNT.store(session_id_poller_max_threads(), Ordering::SeqCst);

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-budget".to_string(),
            Box::new(|| Some("id".to_string())),
            Box::new(|_| {}),
            None,
        );

        assert!(
            !poller.is_running(),
            "poller should not have spawned when budget exhausted"
        );
        assert!(
            poller.cmd_rx.is_some(),
            "cmd_rx should be returned when budget exhausted"
        );
    }

    #[test]
    #[serial]
    fn test_poller_is_running_after_start() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-running".to_string(),
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(10));
                Some("id".to_string())
            }),
            Box::new(|_| {}),
            None,
        );

        assert!(poller.is_running(), "poller should be running after start");
        poller.stop();
    }

    #[test]
    #[serial]
    fn test_poller_cleanup_decrements_counter() {
        let poll_count = Arc::new(Mutex::new(0u32));
        let poll_count_clone = poll_count.clone();

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-cleanup".to_string(),
            Box::new(move || {
                *lock_unpoisoned(&poll_count_clone) += 1;
                Some("id".to_string())
            }),
            Box::new(|_| {}),
            None,
        );

        // Wait for the immediate first poll to run
        std::thread::sleep(Duration::from_millis(100));

        let count_before_stop = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);
        poller.stop();
        let count_after_stop = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);

        assert!(
            count_after_stop < count_before_stop,
            "counter should decrement after stop (before_stop={}, after_stop={})",
            count_before_stop,
            count_after_stop
        );
        assert!(
            *lock_unpoisoned(&poll_count) >= 2,
            "stop must perform a final poll after the immediate first poll"
        );
    }

    #[test]
    fn test_interval_exact_at_threshold() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            POLL_STABLE_THRESHOLD,
        );

        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        // 2 * 1.5 = 3
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 0);

        interval.record_no_change();
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 1);
    }

    #[test]
    fn test_interval_max_clamping_precision() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );

        for _ in 0..1000 {
            interval.record_no_change();
            assert!(
                interval.current() <= POLL_MAX_INTERVAL,
                "interval {} exceeded max {}",
                interval.current().as_secs(),
                POLL_MAX_INTERVAL.as_secs()
            );
        }
        assert_eq!(interval.current(), POLL_MAX_INTERVAL);
    }

    #[test]
    fn test_interval_change_mid_backoff() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            POLL_STABLE_THRESHOLD,
        );

        interval.record_no_change();
        interval.record_no_change();
        assert_eq!(interval.stable_count, 2);
        assert_eq!(interval.current(), Duration::from_secs(2));

        interval.record_change();
        assert_eq!(interval.current(), Duration::from_secs(2));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_poller_starts_polling_immediately() {
        let poll_count = Arc::new(Mutex::new(0u32));
        let poll_count_clone = poll_count.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = lock_unpoisoned(&poll_count_clone);
            *count += 1;
            Some("ses_polled".to_string())
        });

        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(|_| {});

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start("test-immediate".to_string(), poll_fn, on_change, None);

        std::thread::sleep(Duration::from_millis(100));

        let count = *lock_unpoisoned(&poll_count);
        assert!(
            count > 0,
            "poller should have started polling immediately (count={})",
            count
        );

        poller.stop();
    }
}
