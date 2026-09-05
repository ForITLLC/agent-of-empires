//! User-facing triage state: archive, trash, favorite, pin, snooze, unread,
//! color, and the idle bookkeeping they read.

use super::*;

/// The MVP palette for the per-session color label. Kept deliberately small and status-oriented.
pub const SESSION_COLORS: &[&str] = &["red", "amber", "green"];

/// True when `color` is a member of the [`SESSION_COLORS`] palette.
pub fn is_valid_session_color(color: &str) -> bool {
    SESSION_COLORS.contains(&color)
}

/// Mutually-exclusive lifecycle bucket a session belongs to, computed by
/// `Instance::effective_bucket()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBucket {
    Active,
    Archived,
    Trashed,
}

/// The refusal a kept session hands back for a sweep-class operation
/// (WO#1953). One value, rendered by every surface: the API serialises it as
/// the 409 body (`to_json`), the CLI/TUI print `message()`. Naming the flag,
/// the session, the op and the exact clear command is the contract — there
/// is no `--force`, so the message must tell the operator the only way
/// through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepRefused {
    pub session_id: String,
    pub title: String,
    pub op: String,
    pub kept_at: DateTime<Utc>,
    pub kept_by: Option<String>,
}

impl KeepRefused {
    /// The one-line HUMAN override for this op (WO#1980-1). The flag exists
    /// to stop sweeps, scripts and API callers; a person who reads the
    /// refusal must be able to act in one command. `--confirm-kept` clears
    /// the flag (logged who/when) and proceeds. There is still no `--force`.
    pub fn override_command(&self) -> String {
        match self.op.as_str() {
            "snooze" => format!(
                "aoe session snooze {} --minutes <n> --confirm-kept",
                self.session_id
            ),
            "trash" | "remove" | "delete" => format!("aoe rm {} --confirm-kept", self.session_id),
            other => format!("aoe session {other} {} --confirm-kept", self.session_id),
        }
    }

    pub fn message(&self) -> String {
        format!(
            "refused: session {} ({}) is kept (keep flag set {}{}); `{}` is blocked. \
             Clear it first: aoe session keep --off {} \
             — or, as a person, do it in one step: {}",
            self.session_id,
            self.title,
            self.kept_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            self.kept_by
                .as_deref()
                .map(|b| format!(" by {b}"))
                .unwrap_or_default(),
            self.op,
            self.session_id,
            self.override_command(),
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": "session_kept",
            "message": self.message(),
            "session_id": self.session_id,
            "title": self.title,
            "op": self.op,
            "kept_at": self.kept_at,
            "kept_by": self.kept_by,
            "clear_with": format!("aoe session keep --off {}", self.session_id),
            "override_with": self.override_command(),
            "override_field": "confirm_kept",
        })
    }
}

impl Instance {
    /// Stamp `last_accessed_at` to the current time AND wake the session from any sink state.
    pub fn touch_last_accessed(&mut self) {
        self.last_accessed_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
        self.idle_dormant_since = None;
    }

    /// Whether this session's structured view worker was auto-stopped for inactivity and should not
    /// be respawned by the reconciler until the user wakes it.
    pub fn is_idle_dormant(&self) -> bool {
        self.idle_dormant_since.is_some()
    }

    /// Mark the session dormant after its structured view worker was auto-stopped
    /// for inactivity. Idempotent: re-marking refreshes the timestamp.
    pub fn mark_idle_dormant(&mut self) {
        self.idle_dormant_since = Some(Utc::now());
    }

    /// Whether this session should render as "dormant" (worker auto-stopped for inactivity,
    /// resumable) rather than with its raw `status`.
    pub fn is_shown_dormant(&self) -> bool {
        self.is_idle_dormant() && self.status != Status::Stopped
    }

    /// Mark the session archived. Archived sessions sink to the bottom of the Attention sort and
    /// render in italic+dim style, but remain visible.
    pub fn archive(&mut self) {
        if let Some(r) = self.keep_refusal("archive") {
            tracing::warn!(target: "session.keep", "{}", r.message());
            return;
        }
        self.archived_at = Some(Utc::now());
        self.favorited_at = None;
        self.snoozed_until = None;
        self.pinned_at = None;
        self.settle_archived_status();
    }

    /// Idle is the resting state an archived row can truthfully claim; see `archive`.
    pub(crate) fn settle_archived_status(&mut self) {
        if matches!(
            self.status,
            Status::Running | Status::Waiting | Status::Starting
        ) {
            self.status = Status::Idle;
        }
    }

    pub fn unarchive(&mut self) {
        self.archived_at = None;
        self.idle_dormant_since = None;
    }

    /// True for the single fleet-manager session ("AoE-Commander"). Used to
    /// pin it to the absolute top of every sidebar view regardless of status
    /// tier or group membership (see `HomeView::build_flat_items`).
    /// `favorite`/`pinned_at` only pin within a status tier and sink when the
    /// row goes idle; the commander must stay top-visible in every state.
    /// Title-based so it tracks the session the user sees as the commander.
    pub fn is_commander(&self) -> bool {
        self.title == "AoE-Commander"
    }

    /// Pin rank for the commander lane: `Some(0)` for the Claude
    /// `AoE-Commander` (exact title), `Some(1)` for a manager-lane twin titled
    /// `AoE-Commander-<runtime>` (2026-09-08: `AoE-Commander-Codex`, the same
    /// brief on OpenAI Codex), `None` for every other session. Ranked rows are
    /// hoisted to the top of every view in rank order (see
    /// `pin_commander_first`). `is_commander` stays the exact match on purpose:
    /// it is the relay/routing identity of the one Claude Commander, and
    /// widening it would re-target the twins.
    pub fn commander_pin_rank(&self) -> Option<u8> {
        if self.is_commander() {
            return Some(0);
        }
        match self.title.strip_prefix("AoE-Commander-") {
            Some(runtime) if !runtime.is_empty() => Some(1),
            _ => None,
        }
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// Soft-delete the session into the trash bucket. Stops the live session (handled by the
    /// caller.
    pub fn trash(&mut self) {
        if let Some(r) = self.keep_refusal("trash") {
            tracing::warn!(target: "session.keep", "{}", r.message());
            return;
        }
        if self.trashed_at.is_none() {
            self.trashed_at = Some(Utc::now());
        }
    }

    /// Restore a trashed session back to its prior bucket (active or
    /// archived, depending on the preserved sibling flags). Idempotent.
    pub fn untrash(&mut self) {
        self.trashed_at = None;
    }

    pub fn is_trashed(&self) -> bool {
        self.trashed_at.is_some()
    }

    /// The mutually-exclusive lifecycle bucket a session renders in. Precedence is `Trashed >
    /// Archived > Active`.
    pub fn effective_bucket(&self) -> SessionBucket {
        if self.is_trashed() {
            SessionBucket::Trashed
        } else if self.is_archived() {
            SessionBucket::Archived
        } else {
            SessionBucket::Active
        }
    }

    /// Mark the session favorite. Sibling of `archive`, with opposite semantics.
    pub fn favorite(&mut self) {
        self.favorited_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
    }

    pub fn unfavorite(&mut self) {
        self.favorited_at = None;
    }

    pub fn is_favorited(&self) -> bool {
        self.favorited_at.is_some()
    }

    /// Set (or clear, with `None`) the per-session color label. Only a value in the
    /// [`SESSION_COLORS`] palette is accepted.
    pub fn set_color(&mut self, color: Option<String>) -> Result<(), String> {
        match color {
            None => self.color = None,
            Some(c) => {
                if !is_valid_session_color(&c) {
                    return Err(format!(
                        "invalid color {:?}; expected one of: {}, or none",
                        c,
                        SESSION_COLORS.join(", ")
                    ));
                }
                self.color = Some(c);
            }
        }
        Ok(())
    }

    /// Read the agent-raised urgent flag from `attention.json`.
    pub fn is_urgent(&self) -> bool {
        if self.is_archived() || self.is_snoozed() {
            return false;
        }
        crate::hooks::read_hook_urgent(&self.id)
    }

    /// Temporarily defer this session for `minutes`; sets `snoozed_until` to `Utc::now() +
    /// minutes`.
    pub fn snooze(&mut self, minutes: u32) {
        if let Some(r) = self.keep_refusal("snooze") {
            tracing::warn!(target: "session.keep", "{}", r.message());
            return;
        }
        self.snoozed_until = Some(Utc::now() + chrono::Duration::minutes(minutes as i64));
        self.pinned_at = None;
    }

    pub fn unsnooze(&mut self) {
        self.snoozed_until = None;
    }

    /// True if the session carries the unread marker.
    pub fn is_unread(&self) -> bool {
        self.unread
    }

    /// Mark the session unread. Used both by the auto-mark on a finished turn (`Running -> Idle`)
    /// and the manual "Mark as unread" action.
    pub fn mark_unread(&mut self) {
        self.unread = true;
    }

    /// Clear the unread marker. Used whenever the user engages with the session (open/attach,
    /// live-send, click, dwell) and by the explicit "Mark as read" action.
    pub fn mark_read(&mut self) {
        self.unread = false;
    }

    /// Manual toggle (`U`): read -> unread; unread -> read.
    pub fn toggle_unread(&mut self) {
        self.unread = !self.unread;
    }

    /// True if `snoozed_until` is set AND in the future. Expired snoozes return false so the row
    /// naturally rejoins the main sort on the next render.
    pub fn is_snoozed(&self) -> bool {
        self.snoozed_until.map(|t| t > Utc::now()).unwrap_or(false)
    }

    /// Combined "don't bother me" sink-state check: trashed, snoozed, or archived.
    pub fn is_dismissed(&self) -> bool {
        self.is_trashed() || self.is_snoozed() || self.is_archived()
    }

    /// Remaining snooze duration as a `chrono::Duration`, or `None` if the
    /// session isn't snoozed (or the timestamp has already expired).
    pub fn snooze_remaining(&self) -> Option<chrono::Duration> {
        self.snoozed_until.and_then(|t| {
            let delta = t - Utc::now();
            if delta > chrono::Duration::zero() {
                Some(delta)
            } else {
                None
            }
        })
    }

    /// Mark this session pinned. Pin is a web-only surfacing primitive.
    pub fn pin(&mut self) {
        self.pinned_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
    }

    pub fn unpin(&mut self) {
        self.pinned_at = None;
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned_at.is_some()
    }

    /// WO#1953: mark this session as kept. Idempotent — a second `keep`
    /// preserves the first stamp and setter so the refusal keeps naming the
    /// person who actually asked for it.
    pub fn keep(&mut self, by: Option<&str>) {
        if self.kept_at.is_none() {
            self.kept_at = Some(Utc::now());
            self.kept_by = by.map(str::to_string);
        }
    }

    /// Clear the keep flag. The daemon logs who/when at its call site
    /// (`session.keep` target); this is the pure state change.
    pub fn unkeep(&mut self) {
        self.kept_at = None;
        self.kept_by = None;
    }

    pub fn is_kept(&self) -> bool {
        self.kept_at.is_some()
    }

    /// The refusal a kept session hands back for a sweep-class `op`
    /// (`archive`, `snooze`, `trash`, `remove`, …), or `None` when the row is
    /// not kept. Every refusing surface (API 409, CLI error, TUI dialog,
    /// placement scripts) derives its wording from this one value so the
    /// operator always sees the same flag, id, op and clear command.
    pub fn keep_refusal(&self, op: &str) -> Option<KeepRefused> {
        self.kept_at.map(|kept_at| KeepRefused {
            session_id: self.id.clone(),
            title: self.title.clone(),
            op: op.to_string(),
            kept_at,
            kept_by: self.kept_by.clone(),
        })
    }

    /// Time elapsed since this session most recently transitioned into `Idle`.
    pub fn idle_age(&self) -> Option<std::time::Duration> {
        if self.status != Status::Idle {
            return None;
        }
        let since = self.idle_entered_at?;
        (Utc::now() - since).to_std().ok()
    }

    /// True iff this session should keep the machine awake: it is active (`Running`, `Waiting`,
    /// `Starting`, or `Creating`), or it went idle less than `window` ago.
    pub fn has_recent_activity(&self, window: std::time::Duration) -> bool {
        matches!(
            self.status,
            Status::Running | Status::Waiting | Status::Starting | Status::Creating
        ) || matches!(self.idle_age(), Some(age) if age < window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> Instance {
        Instance::new("test", "/tmp/test")
    }

    #[test]
    fn set_color_accepts_only_the_palette() {
        let mut inst = inst();
        for c in SESSION_COLORS {
            inst.set_color(Some((*c).to_string())).unwrap();
            assert_eq!(inst.color.as_deref(), Some(*c));
        }
        inst.set_color(None).unwrap();
        assert_eq!(inst.color, None);

        inst.set_color(Some("green".to_string())).unwrap();
        let err = inst.set_color(Some("chartreuse".to_string())).unwrap_err();
        assert!(err.contains("chartreuse"), "{err}");
        assert_eq!(inst.color.as_deref(), Some("green"));

        for (color, valid) in [
            ("red", true),
            ("amber", true),
            ("green", true),
            ("blue", false),
            ("", false),
            ("Red", false),
        ] {
            assert_eq!(is_valid_session_color(color), valid, "{color}");
        }
    }

    #[test]
    fn triage_mutators_keep_their_exclusivity_rules() {
        type Action = fn(&mut Instance);
        type Check = fn(&Instance) -> bool;
        let archived: Check = Instance::is_archived;
        let snoozed: Check = Instance::is_snoozed;
        let dormant: Check = Instance::is_idle_dormant;
        let favorited: Check = Instance::is_favorited;
        let pinned: Check = Instance::is_pinned;
        let touch: Action = Instance::touch_last_accessed;
        // (label, setup, action, [(check, expected after action)])
        let cases: &[(&str, &[Action], Action, &[(Check, bool)])] = &[
            (
                "touch wakes archive",
                &[|i| i.archive()],
                touch,
                &[(archived, false)],
            ),
            (
                "touch wakes snooze",
                &[|i| i.snooze(30)],
                touch,
                &[(snoozed, false)],
            ),
            (
                "touch wakes dormancy",
                &[|i| i.mark_idle_dormant()],
                touch,
                &[(dormant, false)],
            ),
            (
                "touch keeps favorite",
                &[|i| i.favorite()],
                touch,
                &[(favorited, true)],
            ),
            ("touch keeps pin", &[|i| i.pin()], touch, &[(pinned, true)]),
            (
                "unarchive wakes dormancy",
                &[|i| i.archive(), |i| i.mark_idle_dormant()],
                |i| i.unarchive(),
                &[(archived, false), (dormant, false)],
            ),
            (
                "archive clears snooze",
                &[|i| i.snooze(15)],
                |i| i.archive(),
                &[(archived, true), (snoozed, false)],
            ),
            (
                "archive clears pin",
                &[|i| i.pin()],
                |i| i.archive(),
                &[(archived, true), (pinned, false)],
            ),
            (
                "pin clears archive",
                &[|i| i.archive()],
                |i| i.pin(),
                &[(pinned, true), (archived, false), (snoozed, false)],
            ),
            (
                "pin clears snooze",
                &[|i| i.snooze(15)],
                |i| i.pin(),
                &[(pinned, true), (snoozed, false)],
            ),
            (
                "snooze clears pin",
                &[|i| i.pin()],
                |i| i.snooze(30),
                &[(snoozed, true), (pinned, false)],
            ),
            (
                "pin keeps favorite",
                &[|i| i.favorite()],
                |i| i.pin(),
                &[(pinned, true), (favorited, true)],
            ),
            (
                "favorite keeps pin",
                &[|i| i.pin()],
                |i| i.favorite(),
                &[(pinned, true), (favorited, true)],
            ),
            (
                "mark dormant",
                &[],
                |i| i.mark_idle_dormant(),
                &[(dormant, true)],
            ),
        ];
        for (label, setup, action, checks) in cases {
            let mut inst = inst();
            setup.iter().for_each(|step| step(&mut inst));
            action(&mut inst);
            for (check, expected) in checks.iter() {
                assert_eq!(check(&inst), *expected, "{label}");
            }
        }
        let mut touched = inst();
        touched.touch_last_accessed();
        assert!(touched.last_accessed_at.is_some());
    }

    #[test]
    fn unread_marker_is_idempotent_toggles_and_skips_serialization_when_false() {
        let mut inst = inst();
        assert!(!inst.is_unread());
        for (step, expected) in [
            (Instance::mark_unread as fn(&mut Instance), true),
            (Instance::mark_unread, true),
            (Instance::mark_read, false),
            (Instance::mark_read, false),
            (Instance::toggle_unread, true),
            (Instance::toggle_unread, false),
        ] {
            step(&mut inst);
            assert_eq!(inst.is_unread(), expected);
        }
        assert!(serde_json::to_value(&inst).unwrap().get("unread").is_none());
        inst.unread = true;
        let json = serde_json::to_value(&inst).unwrap();
        assert_eq!(json["unread"], serde_json::json!(true));
        assert!(serde_json::from_value::<Instance>(json).unwrap().unread);
    }

    #[test]
    fn dormancy_presents_only_on_an_idle_row() {
        for (status, marked, shown) in [
            (Status::Idle, true, true),
            // A deliberate Stop also marks dormant but presents as stopped.
            (Status::Stopped, true, false),
            (Status::Idle, false, false),
            (Status::Running, false, false),
        ] {
            let mut inst = inst();
            inst.status = status;
            if marked {
                inst.mark_idle_dormant();
            }
            assert_eq!(inst.is_shown_dormant(), shown, "{status:?} {marked}");
        }
    }

    #[test]
    fn trash_wins_the_bucket_and_preserves_decorations() {
        let mut inst = inst();
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
        let json = serde_json::to_string(&inst).unwrap();
        assert!(!json.contains("trashed_at"));
        inst.favorite();
        inst.pin();
        inst.trash();
        assert!(inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Trashed);
        assert!(inst.is_favorited() && inst.is_pinned());
        let back: Instance = serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        assert!(back.is_trashed());
        inst.untrash();
        assert!(!inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
        assert!(inst.is_favorited() && inst.is_pinned());

        let mut archived = self::inst();
        archived.archive();
        assert_eq!(archived.effective_bucket(), SessionBucket::Archived);
        archived.trash();
        assert_eq!(archived.effective_bucket(), SessionBucket::Trashed);
        archived.untrash();
        assert_eq!(archived.effective_bucket(), SessionBucket::Archived);
    }

    #[test]
    fn idle_age_and_recent_activity() {
        let window = std::time::Duration::from_secs(15 * 60);
        let ago = |secs: i64| Some(Utc::now() - chrono::Duration::seconds(secs));
        // (status, idle_entered_at, idle age present, recent activity)
        for (status, entered, has_age, recent) in [
            (Status::Running, ago(60), false, Some(true)),
            (Status::Waiting, None, false, Some(true)),
            (Status::Starting, None, false, Some(true)),
            (Status::Creating, None, false, Some(true)),
            (Status::Stopped, None, false, Some(false)),
            (Status::Error, None, false, Some(false)),
            (Status::Unknown, None, false, Some(false)),
            (Status::Deleting, None, false, Some(false)),
            (Status::Idle, None, false, Some(false)),
            (Status::Idle, ago(60), true, Some(true)),
            (Status::Idle, ago(30 * 60), true, Some(false)),
            // A future timestamp (clock skew) clamps to no age.
            (Status::Idle, ago(-60), false, None),
        ] {
            let mut inst = inst();
            inst.status = status;
            inst.idle_entered_at = entered;
            assert_eq!(inst.idle_age().is_some(), has_age, "{status:?} {entered:?}");
            if let Some(recent) = recent {
                assert_eq!(
                    inst.has_recent_activity(window),
                    recent,
                    "{status:?} {entered:?}"
                );
            }
        }
        let mut inst = inst();
        inst.status = Status::Idle;
        inst.idle_entered_at = ago(5);
        let age = inst.idle_age().unwrap().as_secs();
        assert!((4..=30).contains(&age));
    }

    #[test]
    fn archive_settles_only_live_interaction_statuses() {
        for (status, expected) in [
            (Status::Running, Status::Idle),
            (Status::Waiting, Status::Idle),
            (Status::Starting, Status::Idle),
            (Status::Idle, Status::Idle),
            (Status::Stopped, Status::Stopped),
            (Status::Error, Status::Error),
            (Status::Unknown, Status::Unknown),
        ] {
            let mut inst = inst();
            inst.status = status;
            inst.archive();
            assert!(inst.is_archived());
            assert_eq!(inst.status, expected, "{status:?}");
        }
    }
}

/// WO#1953 — the per-session `keep` flag. A kept session is one the operator
/// has said must never be swept: `archive`, `snooze`, `trash`/`remove` and
/// every auto-archive placement script REFUSE it until the flag is cleared
/// explicitly (`aoe session keep --off <id>`). There is no `--force`.
#[cfg(test)]
mod keep_tests {
    use super::*;

    #[test]
    fn keep_sets_kept_at_and_by_and_unkeep_clears_both() {
        let mut inst = Instance::new("s", "/tmp/x");
        assert!(!inst.is_kept());
        assert!(inst.kept_at.is_none() && inst.kept_by.is_none());

        inst.keep(Some("cli:ben@mini"));
        assert!(inst.is_kept());
        assert!(inst.kept_at.is_some());
        assert_eq!(inst.kept_by.as_deref(), Some("cli:ben@mini"));

        inst.unkeep();
        assert!(!inst.is_kept());
        assert!(inst.kept_at.is_none() && inst.kept_by.is_none());
    }

    #[test]
    fn keep_is_idempotent_and_preserves_the_first_stamp() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.keep(Some("first"));
        let first = inst.kept_at;
        inst.keep(Some("second"));
        assert_eq!(inst.kept_at, first, "re-keeping must not restamp");
        assert_eq!(inst.kept_by.as_deref(), Some("first"));
    }

    /// WO#1980-1: the refusal must ALSO name the human's one-line override,
    /// per op, so a person who hits it never has to guess the flag.
    #[test]
    fn keep_refusal_names_the_human_override_per_op() {
        let mut inst = Instance::new("t", "/tmp/t");
        inst.keep(Some("api:test"));
        let id = inst.id.clone();
        let r = inst.keep_refusal("archive").unwrap();
        assert_eq!(
            r.override_command(),
            format!("aoe session archive {id} --confirm-kept")
        );
        assert!(
            r.message().contains(&r.override_command()),
            "{}",
            r.message()
        );
        assert!(
            r.message().contains("aoe session keep --off"),
            "{}",
            r.message()
        );
        let j = r.to_json();
        assert_eq!(j["override_with"], r.override_command());
        assert_eq!(j["override_field"], "confirm_kept");
        assert_eq!(
            inst.keep_refusal("snooze").unwrap().override_command(),
            format!("aoe session snooze {id} --minutes <n> --confirm-kept")
        );
        assert_eq!(
            inst.keep_refusal("trash").unwrap().override_command(),
            format!("aoe rm {id} --confirm-kept")
        );
        assert_eq!(
            inst.keep_refusal("remove").unwrap().override_command(),
            format!("aoe rm {id} --confirm-kept")
        );
    }

    #[test]
    fn keep_refusal_names_the_flag_the_op_and_the_clear_command() {
        let mut inst = Instance::new("s", "/tmp/x");
        assert!(
            inst.keep_refusal("archive").is_none(),
            "unkept → no refusal"
        );

        inst.keep(Some("api:test"));
        let r = inst.keep_refusal("archive").expect("kept → refusal");
        assert_eq!(r.session_id, inst.id);
        assert_eq!(r.op, "archive");
        let msg = r.message();
        assert!(msg.contains("kept"), "message names the flag: {msg}");
        assert!(msg.contains(&inst.id), "message names the session: {msg}");
        assert!(msg.contains("archive"), "message names the op: {msg}");
        assert!(
            msg.contains(&format!("aoe session keep --off {}", inst.id)),
            "message names the clear command: {msg}"
        );
        let json = r.to_json();
        assert_eq!(json["error"], "session_kept");
        assert_eq!(json["session_id"], inst.id);
        assert_eq!(json["op"], "archive");
        assert_eq!(json["kept_by"], "api:test");
    }

    #[test]
    fn archive_snooze_and_trash_are_no_ops_on_a_kept_row() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.keep(Some("t"));

        inst.archive();
        assert!(!inst.is_archived(), "archive() must not touch a kept row");
        inst.snooze(30);
        assert!(!inst.is_snoozed(), "snooze() must not touch a kept row");
        inst.trash();
        assert!(!inst.is_trashed(), "trash() must not touch a kept row");
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
        assert!(inst.is_kept(), "the flag survives every refused op");
    }

    #[test]
    fn touch_last_accessed_and_pin_leave_keep_alone() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.keep(Some("t"));
        inst.touch_last_accessed();
        assert!(inst.is_kept(), "engagement must not clear keep");
        inst.pin();
        assert!(inst.is_kept());
        inst.unpin();
        assert!(inst.is_kept());
        inst.favorite();
        inst.unfavorite();
        assert!(inst.is_kept());
    }

    #[test]
    fn unkeep_then_archive_works_again() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.keep(Some("t"));
        inst.unkeep();
        inst.archive();
        assert!(inst.is_archived());
    }

    #[test]
    fn kept_fields_round_trip_through_serde_and_default_absent() {
        let mut inst = Instance::new("s", "/tmp/x");
        let bare = serde_json::to_value(&inst).unwrap();
        assert!(bare.get("kept_at").is_none(), "absent when unset");
        assert!(bare.get("kept_by").is_none(), "absent when unset");

        inst.keep(Some("who"));
        let v = serde_json::to_value(&inst).unwrap();
        assert!(v.get("kept_at").is_some());
        assert_eq!(v["kept_by"], "who");
        let back: Instance = serde_json::from_value(v).unwrap();
        assert!(back.is_kept());
        assert_eq!(back.kept_by.as_deref(), Some("who"));

        // Older rows without the field deserialize as not-kept.
        let mut legacy = serde_json::to_value(&Instance::new("l", "/tmp/l")).unwrap();
        legacy.as_object_mut().unwrap().remove("kept_at");
        let legacy: Instance = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.is_kept());
    }
}
