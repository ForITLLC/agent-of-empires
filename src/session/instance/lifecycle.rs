//! Cross-process lifecycle reservations: the generation-stamped lock that
//! keeps two aoe processes from launching or killing the same session.

use super::*;

/// One durable ownership protocol for every session lifecycle transition.
///
/// A transition acquires the per-instance lifecycle flock, then records a
/// fresh generation under `Storage::update`. Terminal launch is the ordered
/// exception: it first takes the app-global per-session title flock so title
/// writers and launch cannot derive different tmux names. The durable
/// reservation stays held through hooks, external side effects, and the
/// exact-generation commit; callers may release outer flocks for reentrant hooks.
/// `status` is presentation state and never proves ownership.
///
/// A crashed owner loses both the flock and, after the TTL, its reservation.
/// Recovery may then acquire a newer generation; exact-generation commits
/// ensure a late result can never mutate or clear that replacement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleOperation {
    Launch,
    Capture,
    Stop,
    Purge,
    Restore,
    Trash,
}

impl LifecycleOperation {
    pub(crate) fn busy_reason(self) -> String {
        format!("busy with lifecycle operation {self:?}")
    }

    pub(crate) fn already_in_progress_reason(self) -> String {
        format!("lifecycle operation {self:?} is already in progress")
    }
}

pub(crate) const NEWER_GENERATION_BUSY_REASON: &str = "busy with a newer lifecycle generation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleReservationError {
    Busy(LifecycleOperation),
    GenerationOverflow,
}

impl std::fmt::Display for LifecycleReservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(operation) => f.write_str(&operation.already_in_progress_reason()),
            Self::GenerationOverflow => f.write_str("lifecycle generation overflow"),
        }
    }
}

impl std::error::Error for LifecycleReservationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleReservation {
    pub op: LifecycleOperation,
    pub generation: u64,
    pub at: DateTime<Utc>,
    /// Process that took the reservation. A reservation whose holder is
    /// provably gone is not live, whatever its age: a crashed owner must not
    /// wedge every lifecycle op behind the TTL. Absent on legacy rows and
    /// omitted from the wire when unset, so the format is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_pid: Option<u32>,
}

impl LifecycleReservation {
    /// `Some(true)` while the recorded holder is running, `Some(false)` when
    /// it is provably gone, `None` when no holder was recorded or liveness
    /// cannot be determined here (then the TTL alone governs).
    pub fn holder_alive(&self) -> Option<bool> {
        let raw = i32::try_from(self.holder_pid?).ok()?;
        if raw <= 0 {
            return None;
        }
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            match kill(Pid::from_raw(raw), None) {
                Ok(()) | Err(Errno::EPERM) => Some(true),
                Err(Errno::ESRCH) => Some(false),
                Err(_) => None,
            }
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Live = within `ttl` AND the holder, when recorded, is still running.
    pub fn is_live(&self, now: DateTime<Utc>, ttl: chrono::Duration) -> bool {
        (now - self.at) < ttl && self.holder_alive() != Some(false)
    }

    /// Operator-facing description of who holds the reservation and for how
    /// long, so a "busy" refusal says whether the holder is even alive.
    pub fn holder_detail(&self, now: DateTime<Utc>) -> String {
        let age = (now - self.at).num_seconds().max(0);
        match (self.holder_pid, self.holder_alive()) {
            (Some(pid), Some(true)) => format!("held by pid {pid} (alive) for {age}s"),
            (Some(pid), Some(false)) => format!("held by pid {pid} (DEAD) for {age}s"),
            (Some(pid), None) => format!("held by pid {pid} (liveness unknown) for {age}s"),
            (None, _) => format!("held by an unrecorded process for {age}s"),
        }
    }
}

impl Instance {
    /// Longer than any bounded hook, teardown, or worktree move. A crashed
    /// owner cannot retain the reservation forever; a late owner is still
    /// harmless because every commit is generation-checked.
    pub const LIFECYCLE_RESERVATION_TTL: chrono::Duration = chrono::Duration::minutes(10);

    /// Acquire exclusive durable ownership of the next lifecycle generation.
    ///
    /// Even a reservation for the same operation belongs to a peer: operation
    /// kind is not an identity. A caller that already owns a reservation must
    /// retain its returned generation and use
    /// [`Self::lifecycle_reservation_is_owned`] rather than reacquiring by kind.
    pub fn try_acquire_lifecycle_reservation(
        &mut self,
        operation: LifecycleOperation,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, LifecycleReservationError> {
        if let Some(reservation) = self.lifecycle_reservation.as_ref().filter(|reservation| {
            reservation.generation == self.lifecycle_generation && reservation.is_live(now, ttl)
        }) {
            return Err(LifecycleReservationError::Busy(reservation.op));
        }

        let generation = self
            .lifecycle_generation
            .checked_add(1)
            .ok_or(LifecycleReservationError::GenerationOverflow)?;
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = Some(LifecycleReservation {
            op: operation,
            generation,
            at: now,
            holder_pid: Some(std::process::id()),
        });
        Ok(generation)
    }

    pub fn lifecycle_reservation_is_owned(
        &self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        self.lifecycle_generation == generation
            && matches!(
                &self.lifecycle_reservation,
                Some(reservation)
                    if reservation.op == operation && reservation.generation == generation
            )
    }

    pub fn has_fresh_lifecycle_reservation(&self, now: DateTime<Utc>) -> bool {
        matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.generation == self.lifecycle_generation
                    && reservation.is_live(now, Self::LIFECYCLE_RESERVATION_TTL)
        )
    }

    pub fn release_lifecycle_reservation_if_owned(
        &mut self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        if self.lifecycle_reservation_is_owned(operation, generation) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    /// Clear a crashed owner's expired reservation. The generation is
    /// deliberately retained as the monotonic cache/result revision.
    pub fn clear_expired_lifecycle_reservation(
        &mut self,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> bool {
        if matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.generation == self.lifecycle_generation
                    && !reservation.is_live(now, ttl)
        ) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    pub(super) fn commit_lifecycle_launch(
        &mut self,
        storage: &crate::session::storage::Storage,
        restart: bool,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            if !stored.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation) {
                return Ok(false);
            }
            stored.status = self.status;
            stored.idle_entered_at = self.idle_entered_at;
            stored.last_accessed_at = self.last_accessed_at;
            stored.sandbox_info = self.sandbox_info.clone();
            stored.capture_started_at = self.capture_started_at;
            if restart && stored.agent_session_id == self.agent_session_id {
                stored.resume_probe_failed_sid = self.resume_probe_failed_sid.clone();
            }
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
            Ok(true)
        })?;
        anyhow::ensure!(
            committed,
            "session {} disappeared or lost its lifecycle reservation before launch commit",
            self.id
        );
        self.lifecycle_reservation = None;
        Ok(())
    }

    pub(super) fn acquire_lifecycle_reservation(
        &mut self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
        status: Option<Status>,
    ) -> Result<u64> {
        let now = Utc::now();
        let mut acquired = None;
        storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(());
            };
            let holder_detail = stored
                .lifecycle_reservation
                .as_ref()
                .map(|reservation| reservation.holder_detail(now))
                .unwrap_or_default();
            let generation = stored
                .try_acquire_lifecycle_reservation(operation, Self::LIFECYCLE_RESERVATION_TTL, now)
                .map_err(|error| match error {
                    LifecycleReservationError::Busy(holder) => {
                        anyhow::anyhow!(
                            "session {} is {} ({holder_detail})",
                            self.id,
                            holder.busy_reason()
                        )
                    }
                    LifecycleReservationError::GenerationOverflow => {
                        anyhow::anyhow!("session {} lifecycle generation overflow", self.id)
                    }
                })?;
            if let Some(status) = status {
                stored.status = status;
                if status != Status::Idle {
                    stored.idle_entered_at = None;
                }
            }
            acquired = Some((generation, stored.lifecycle_reservation.clone()));
            Ok(())
        })?;
        let Some((generation, reservation)) = acquired else {
            anyhow::bail!("session {} no longer exists", self.id);
        };
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = reservation;
        if let Some(status) = status {
            self.status = status;
            if status != Status::Idle {
                self.idle_entered_at = None;
            }
        }
        Ok(generation)
    }

    pub(super) fn commit_lifecycle_status(
        &mut self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
        status: Status,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            if !stored.lifecycle_reservation_is_owned(operation, generation) {
                return Ok(false);
            }
            stored.status = status;
            if status != Status::Idle {
                stored.idle_entered_at = None;
            }
            stored.release_lifecycle_reservation_if_owned(operation, generation);
            Ok(true)
        })?;
        anyhow::ensure!(
            committed,
            "session {} disappeared or lost its lifecycle reservation before commit",
            self.id
        );
        self.lifecycle_reservation = None;
        self.status = status;
        if status != Status::Idle {
            self.idle_entered_at = None;
        }
        Ok(())
    }

    pub(super) fn release_lifecycle_reservation(
        &mut self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored.release_lifecycle_reservation_if_owned(operation, generation))
        })?;
        anyhow::ensure!(
            released,
            "session {} disappeared or lost its lifecycle reservation before release",
            self.id
        );
        self.lifecycle_reservation = None;
        Ok(())
    }

    /// Reacquire launch locks after user hooks, preserving the global
    /// title-before-lifecycle order and failing the reservation consistently.
    pub(super) fn reacquire_launch_locks_after_hooks(
        &mut self,
        storage: &crate::session::storage::Storage,
        hook_result: Result<()>,
    ) -> Result<(
        crate::session::storage::StorageFlock,
        crate::session::storage::StorageFlock,
    )> {
        let title_lock = match crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to reacquire instance title lock after hooks")
        {
            Ok(lock) => lock,
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                return Err(error);
            }
        };
        let lifecycle_lock = match storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to reacquire instance lifecycle lock after hooks")
        {
            Ok(lock) => lock,
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                return Err(error);
            }
        };
        self.reconcile_from_disk();
        if let Err(error) = hook_result {
            self.fail_reserved_launch(storage, &error, false);
            return Err(error);
        }
        self.ensure_reservation_current_or_fail(storage)?;
        Ok((title_lock, lifecycle_lock))
    }

    fn lifecycle_reservation_is_current(
        &self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
    ) -> Result<bool> {
        let generation = self.lifecycle_generation;
        storage.update(|instances, _groups| {
            Ok(instances
                .iter()
                .find(|instance| instance.id == self.id)
                .is_some_and(|stored| stored.lifecycle_reservation_is_owned(operation, generation)))
        })
    }

    fn reservation_is_current(&self, storage: &crate::session::storage::Storage) -> Result<bool> {
        self.lifecycle_reservation_is_current(storage, LifecycleOperation::Launch)
    }

    fn ensure_reservation_current(&self, storage: &crate::session::storage::Storage) -> Result<()> {
        if self.reservation_is_current(storage)? {
            return Ok(());
        }
        anyhow::bail!(
            "session {} changed while launch hooks were running",
            self.id
        )
    }

    fn ensure_reservation_current_or_fail(
        &mut self,
        storage: &crate::session::storage::Storage,
    ) -> Result<()> {
        match self.ensure_reservation_current(storage) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                Err(error)
            }
        }
    }

    pub(super) fn fail_reserved_launch(
        &mut self,
        storage: &crate::session::storage::Storage,
        error: &anyhow::Error,
        cleanup_pane: bool,
    ) {
        if !self.reservation_is_current(storage).unwrap_or(false) {
            return;
        }
        if cleanup_pane {
            let _ = self.kill_clean_locked();
        }
        self.last_error = Some(format!("{error:#}"));
        let _ = self.commit_lifecycle_status(storage, LifecycleOperation::Launch, Status::Error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn lifecycle_status_commit_releases_the_acquired_generation() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = crate::session::storage::Storage::new_unwatched("lifecycle-lease").unwrap();
        let mut instance = Instance::new("session", "/tmp/test");

        let missing = instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap_err();
        assert!(missing.to_string().contains("no longer exists"));

        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let generation = instance.lifecycle_generation;
        instance
            .commit_lifecycle_status(&storage, LifecycleOperation::Launch, Status::Error)
            .unwrap();

        let reloaded = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == instance.id)
            .unwrap();
        assert_eq!(reloaded.lifecycle_generation, generation);
        assert_eq!(reloaded.lifecycle_reservation, None);
        assert_eq!(reloaded.status, Status::Error);
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_reservation_rejects_busy_state_without_blocking_first_launch() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-busy-reservation";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let now = Utc::now();
        let stale = now - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
        let mut cases = [
            ("unleased", Status::Starting, None, 0, true),
            (
                "leased_peer",
                Status::Starting,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: now,
                    holder_pid: None,
                }),
                1,
                false,
            ),
            (
                "superseded",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: now,
                    holder_pid: None,
                }),
                2,
                true,
            ),
            (
                "expired",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: stale,
                    holder_pid: None,
                }),
                1,
                true,
            ),
            (
                "purge",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Purge,
                    generation: 1,
                    at: now,
                    holder_pid: None,
                }),
                1,
                false,
            ),
            (
                "restore",
                Status::Stopped,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Restore,
                    generation: 1,
                    at: now,
                    holder_pid: None,
                }),
                1,
                false,
            ),
            (
                "trash",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Trash,
                    generation: 1,
                    at: now,
                    holder_pid: None,
                }),
                1,
                false,
            ),
            (
                "capture",
                Status::Running,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Capture,
                    generation: 1,
                    at: now,
                    holder_pid: None,
                }),
                1,
                false,
            ),
            ("creating", Status::Creating, None, 0, true),
            ("idle", Status::Idle, None, 0, true),
            ("stopped", Status::Stopped, None, 0, true),
        ]
        .map(
            |(title, status, lifecycle_reservation, lifecycle_generation, allowed)| {
                let mut instance = Instance::new(title, "/tmp/test");
                instance.source_profile = profile.to_string();
                instance.status = status;
                instance.lifecycle_generation = lifecycle_generation;
                instance.lifecycle_reservation = lifecycle_reservation;
                (instance, allowed)
            },
        );
        storage
            .update(|instances, _groups| {
                instances.extend(cases.iter().map(|(instance, _)| instance.clone()));
                Ok(())
            })
            .unwrap();

        for (instance, allowed) in &mut cases {
            let result = instance.acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            );
            assert_eq!(result.is_ok(), *allowed, "{}", instance.title);
        }

        let leased = &cases[0].0;
        assert!(leased.reservation_is_current(&storage).unwrap());
        storage
            .update(|instances, _groups| {
                let peer = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == leased.id)
                    .unwrap();
                peer.lifecycle_generation += 1;
                peer.status = Status::Stopped;
                Ok(())
            })
            .unwrap();
        assert!(!leased.reservation_is_current(&storage).unwrap());

        let mut busy = Instance::new("busy-leased", "/tmp/test");
        busy.source_profile = profile.to_string();
        busy.status = Status::Starting;
        busy.lifecycle_generation = 1;
        busy.lifecycle_reservation = Some(LifecycleReservation {
            op: LifecycleOperation::Launch,
            generation: 1,
            at: Utc::now(),
            holder_pid: None,
        });
        storage
            .update(|instances, _groups| {
                instances.push(busy.clone());
                Ok(())
            })
            .unwrap();

        let began = std::time::Instant::now();
        assert!(busy.stop().unwrap_err().to_string().contains("busy"));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));

        let mut recursive_start = busy.clone();
        let began = std::time::Instant::now();
        assert!(recursive_start
            .start_with_size_opts(None, true)
            .unwrap_err()
            .to_string()
            .contains("busy"));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    #[serial_test::serial]
    fn failed_launch_releases_reservation_after_status_drift() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-fail-drift";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("drift", "/tmp/test");
        inst.source_profile = profile.to_string();
        inst.status = Status::Idle;
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();

        inst.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )
        .unwrap();
        let reserved_gen = inst.lifecycle_generation;

        // A same-generation passive status patch changes presentation state
        // without changing ownership while prepare_launch runs unlocked.
        storage
            .update(|instances, _groups| {
                let stored = instances.iter_mut().find(|i| i.id == inst.id).unwrap();
                assert_eq!(stored.lifecycle_generation, reserved_gen);
                assert!(stored.lifecycle_reservation.is_some());
                stored.status = Status::Stopped;
                Ok(())
            })
            .unwrap();

        // The launch guard still recognizes the exact-generation reservation.
        // A later launch failure must release it rather than stranding the
        // marker until its TTL.
        inst.ensure_reservation_current_or_fail(&storage).unwrap();
        let error = anyhow::anyhow!("launch failed after status drift");
        inst.fail_reserved_launch(&storage, &error, false);

        let leftover = storage
            .update(|instances, _groups| {
                Ok(instances
                    .iter()
                    .find(|instance| instance.id == inst.id)
                    .and_then(|instance| instance.lifecycle_reservation.clone()))
            })
            .unwrap();
        assert!(
            leftover.is_none(),
            "a failed launch must clear its reservation even after a same-generation status drift"
        );
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_launch_commit_keeps_reserved_generation_and_rejects_stale_or_overflowed_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage =
            crate::session::storage::Storage::new_unwatched("lifecycle-launch-commit").unwrap();
        let mut committed = Instance::new("committed", "/tmp/test");
        let mut stale = Instance::new("stale", "/tmp/test");
        let mut overflow = Instance::new("overflow", "/tmp/test");
        overflow.lifecycle_generation = u64::MAX;
        storage
            .update(|instances, _groups| {
                instances.extend([committed.clone(), stale.clone(), overflow.clone()]);
                Ok(())
            })
            .unwrap();

        committed
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let reserved_generation = committed.lifecycle_generation;
        let capture_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567);
        committed.status = Status::Running;
        committed.capture_started_at = Some(capture_floor);
        committed.commit_lifecycle_launch(&storage, false).unwrap();
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == committed.id)
            .unwrap();
        assert_eq!(committed.lifecycle_generation, reserved_generation);
        assert_eq!(disk.lifecycle_generation, committed.lifecycle_generation);
        assert_eq!(disk.status, Status::Running);
        assert_eq!(disk.capture_started_at, Some(capture_floor));

        stale
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let stale_token = stale.lifecycle_generation;
        stale.status = Status::Running;
        stale.capture_started_at =
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(9_999_999));
        storage
            .update(|instances, _groups| {
                let peer = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == stale.id)
                    .unwrap();
                peer.lifecycle_generation = stale_token + 1;
                peer.status = Status::Stopped;
                peer.capture_started_at = Some(capture_floor);
                Ok(())
            })
            .unwrap();
        let error = stale.commit_lifecycle_launch(&storage, false).unwrap_err();
        assert!(error.to_string().contains("lost its lifecycle reservation"));
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == stale.id)
            .unwrap();
        assert_eq!(stale.lifecycle_generation, stale_token);
        assert_eq!(disk.lifecycle_generation, stale_token + 1);
        assert_eq!(disk.status, Status::Stopped);
        assert_eq!(
            disk.capture_started_at,
            Some(capture_floor),
            "a stale launch token must not overwrite the winning floor"
        );

        assert!(overflow
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap_err()
            .to_string()
            .contains("overflow"));
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == overflow.id)
            .unwrap();
        assert_eq!(overflow.lifecycle_generation, u64::MAX);
        assert_eq!(overflow.status, Status::Idle);
        assert_eq!(disk.lifecycle_generation, u64::MAX);
        assert_eq!(disk.status, Status::Idle);
    }

    #[test]
    fn lifecycle_reservation_roundtrips_and_legacy_rows_default_to_none() {
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(!fresh_json.contains("lifecycle_reservation"));
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert_eq!(parsed.lifecycle_reservation, None);

        let mut instance = Instance::new("s", "/tmp/x");
        let now = Utc::now();
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("free row grants the lease");
        let json = serde_json::to_string(&instance).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(
            back.lifecycle_reservation,
            Some(LifecycleReservation {
                op: LifecycleOperation::Purge,
                generation,
                at: now,
                holder_pid: Some(std::process::id()),
            })
        );
    }

    #[test]
    fn lifecycle_reservation_excludes_peers_and_uses_generation_as_identity() {
        let now = Utc::now();
        let mut instance = Instance::new("s", "/tmp/x");
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("first operation acquires");

        for contender in [
            LifecycleOperation::Launch,
            LifecycleOperation::Stop,
            LifecycleOperation::Purge,
            LifecycleOperation::Restore,
            LifecycleOperation::Trash,
        ] {
            assert_eq!(
                instance.try_acquire_lifecycle_reservation(
                    contender,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    now + chrono::Duration::seconds(1),
                ),
                Err(LifecycleReservationError::Busy(LifecycleOperation::Purge)),
                "{contender:?} must not replace a live peer reservation",
            );
        }
        assert!(!instance
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation + 1,));
        assert!(instance.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
        assert!(
            instance.release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation)
        );
    }

    #[test]
    fn expired_lifecycle_reservation_is_recoverable_without_reusing_generation() {
        let ttl = Instance::LIFECYCLE_RESERVATION_TTL;
        let now = Utc::now();
        let mut instance = Instance::new("s", "/tmp/x");
        let old_generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                ttl,
                now - ttl - chrono::Duration::seconds(1),
            )
            .expect("first operation acquires");
        let new_generation = instance
            .try_acquire_lifecycle_reservation(LifecycleOperation::Restore, ttl, now)
            .expect("expired lease can be replaced");

        assert!(new_generation > old_generation);
        assert!(!instance
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, old_generation,));
        assert!(
            instance.lifecycle_reservation_is_owned(LifecycleOperation::Restore, new_generation,)
        );
    }

    // ── WO#1641 3a: a reservation dies with its holder process ───────────────
    const DEAD_PID: u32 = 2_000_000_000; // above pid_max on Linux and macOS

    fn reservation_held_by(holder_pid: Option<u32>, at: DateTime<Utc>) -> LifecycleReservation {
        LifecycleReservation {
            op: LifecycleOperation::Launch,
            generation: 1,
            at,
            holder_pid,
        }
    }

    #[test]
    fn reservation_with_dead_holder_is_not_live_before_ttl() {
        let now = Utc::now();
        let dead = reservation_held_by(Some(DEAD_PID), now);
        assert_eq!(dead.holder_alive(), Some(false));
        assert!(!dead.is_live(now, Instance::LIFECYCLE_RESERVATION_TTL));
        assert!(
            dead.holder_detail(now).contains("DEAD"),
            "{}",
            dead.holder_detail(now)
        );
    }

    #[test]
    fn reservation_with_live_holder_stays_live_until_ttl() {
        let now = Utc::now();
        let ttl = Instance::LIFECYCLE_RESERVATION_TTL;
        let live = reservation_held_by(Some(std::process::id()), now);
        assert_eq!(live.holder_alive(), Some(true));
        assert!(live.is_live(now, ttl));
        assert!(!live.is_live(now + ttl + chrono::Duration::seconds(1), ttl));
        assert!(live.holder_detail(now).contains("alive"));
    }

    #[test]
    fn legacy_reservation_without_holder_keeps_ttl_semantics() {
        let now = Utc::now();
        let ttl = Instance::LIFECYCLE_RESERVATION_TTL;
        let legacy = reservation_held_by(None, now);
        assert_eq!(legacy.holder_alive(), None);
        assert!(legacy.is_live(now, ttl));
        assert!(!legacy.is_live(now + ttl + chrono::Duration::seconds(1), ttl));

        let json = r#"{"op":"launch","generation":1,"at":"2026-01-01T00:00:00Z"}"#;
        let parsed: LifecycleReservation = serde_json::from_str(json).expect("legacy row parses");
        assert_eq!(parsed.holder_pid, None);
        assert!(
            !serde_json::to_string(&parsed)
                .unwrap()
                .contains("holder_pid"),
            "unset holder must not change the wire format"
        );
        let recorded = reservation_held_by(Some(std::process::id()), now);
        assert!(serde_json::to_string(&recorded)
            .unwrap()
            .contains("holder_pid"));
    }

    #[test]
    fn dead_holder_reservation_yields_to_a_new_acquire() {
        let now = Utc::now();
        let mut instance = Instance::new("dead-holder", "/tmp/test");
        instance.lifecycle_generation = 1;
        instance.lifecycle_reservation = Some(reservation_held_by(Some(DEAD_PID), now));
        assert!(!instance.has_fresh_lifecycle_reservation(now));
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("a dead holder must not wedge the next lifecycle op");
        assert_eq!(generation, 2);
        assert_eq!(
            instance
                .lifecycle_reservation
                .as_ref()
                .and_then(|r| r.holder_pid),
            Some(std::process::id())
        );
    }

    #[test]
    fn live_holder_reservation_still_blocks() {
        let now = Utc::now();
        let mut instance = Instance::new("live-holder", "/tmp/test");
        instance.lifecycle_generation = 1;
        instance.lifecycle_reservation = Some(reservation_held_by(Some(std::process::id()), now));
        assert!(instance.has_fresh_lifecycle_reservation(now));
        assert_eq!(
            instance.try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            ),
            Err(LifecycleReservationError::Busy(LifecycleOperation::Launch))
        );
        assert!(
            !instance.clear_expired_lifecycle_reservation(Instance::LIFECYCLE_RESERVATION_TTL, now)
        );
    }

    #[test]
    fn dead_holder_reservation_is_cleared_as_expired() {
        let now = Utc::now();
        let mut instance = Instance::new("dead-holder-clear", "/tmp/test");
        instance.lifecycle_generation = 1;
        instance.lifecycle_reservation = Some(reservation_held_by(Some(DEAD_PID), now));
        assert!(
            instance.clear_expired_lifecycle_reservation(Instance::LIFECYCLE_RESERVATION_TTL, now)
        );
        assert_eq!(instance.lifecycle_reservation, None);
        assert_eq!(
            instance.lifecycle_generation, 1,
            "generation is retained as the revision"
        );
    }
}
