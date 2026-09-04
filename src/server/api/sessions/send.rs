//! Send-message, paste-image, and read-output endpoints.

use super::*;

// ============================================================================
// Send + read-output endpoints
//
// Together these are the minimum primitive an external orchestrator needs to
// run an aoe session as a controlled subagent: push a prompt in, read the
// pane back. Mirrors what the TUI's send-message dialog and pane preview do,
// without requiring keyboard or websocket attach.
// ============================================================================

#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    /// Whether to auto-revive a dead/stopped session before sending. Defaults
    /// to `true`; set to `false` for fail-loud behavior (parity with the
    /// `--no-revive` CLI flag).
    #[serde(default = "default_revive")]
    pub revive: bool,
    /// When the composer holds an operator's unsent draft, park the message
    /// on the session's server-owned prompt queue (202, `queued: true`)
    /// instead of refusing it (423 `parked_draft`); the daemon delivers it
    /// as its own turn once the composer clears (`server::send_queue`).
    /// Defaults to `true`; `false` restores the plain refusal (parity with
    /// `aoe send --no-queue`).
    #[serde(default = "default_queue")]
    pub queue: bool,
    /// Free-text sender label recorded on the queued row (`origin_device`),
    /// so `aoe session queue <id>` can say who is waiting. Optional.
    #[serde(default)]
    pub sender: Option<String>,
}

fn default_revive() -> bool {
    true
}

fn default_queue() -> bool {
    true
}

/// Queue ids minted for parked sends: `send-` + 12 hex, short enough to
/// type into `aoe session queue drop <id> <qid>`.
fn new_send_queue_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("send-{}", &raw[..12])
}

/// Why a parked send could NOT be queued; the caller then answers with the
/// plain 423 refusal, annotated.
#[derive(Debug, PartialEq, Eq)]
enum NotQueued {
    /// The queue is at its depth cap (`depth` rows).
    Full(usize),
    /// The message exceeds the per-row text cap.
    TooLarge,
    /// The session vanished between the refusal and the enqueue.
    Gone,
}

/// Park a refused send on the session's server-owned prompt queue. The
/// caller holds the instance lock; the submission guard is claimed here in
/// the same order the drain uses (instance lock, then submission), so the
/// row can never be appended inside a drain's snapshot-to-send window
/// (#3621). Returns the row and its 1-based FIFO position.
async fn queue_parked_send(
    state: &Arc<AppState>,
    id: &str,
    message: String,
    sender: Option<String>,
) -> Result<(crate::acp::state::QueuedPromptEntry, usize), NotQueued> {
    if message.len() > crate::server::api::queue::MAX_QUEUED_TEXT_BYTES {
        return Err(NotQueued::TooLarge);
    }
    let _submission = state.session_service.prompt_submission(id).await;
    let depth = state
        .session_service
        .queued_prompts_snapshot(id)
        .await
        .len();
    if depth >= crate::server::api::queue::MAX_QUEUED_PROMPTS_PER_SESSION {
        return Err(NotQueued::Full(depth));
    }
    let origin = sender
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "api send".to_string());
    let entry = state
        .session_service
        .enqueue_prompt(
            id,
            new_send_queue_id(),
            message,
            Vec::new(),
            Some(origin),
            chrono::Utc::now().to_rfc3339(),
        )
        .await
        .ok_or(NotQueued::Gone)?;
    let position = state
        .session_service
        .queued_prompts_snapshot(id)
        .await
        .iter()
        .position(|e| e.id == entry.id)
        .map(|p| p + 1)
        .unwrap_or(depth + 1);
    Ok((entry, position))
}

/// The wire contract for a send parked on the queue instead of refused.
///
/// 202 Accepted, not 423: the message is now the daemon's to deliver, and
/// an HTTP client that treats every 4xx as failure would retry and queue
/// it twice. Existing callers keep branching on `sent` (still `false`) and
/// `error` (still `parked_draft`), so a consumer that never learned
/// `queued` reads it as not-delivered-yet, and one that has looks at
/// `queue_id` / `position`. `retry_safe` is `false` for the same reason
/// (a resend double-queues). Draft size only, never its text.
fn queued_send_parts(
    refusal: &crate::tmux::ParkedDraftRefusal,
    entry: &crate::acp::state::QueuedPromptEntry,
    position: usize,
) -> (StatusCode, serde_json::Value) {
    (
        StatusCode::ACCEPTED,
        serde_json::json!({
            "error": "parked_draft",
            "reason": "operator_draft_in_composer",
            "sent": false,
            "queued": true,
            "queue_id": entry.id,
            "position": position,
            "queued_at": entry.created_at,
            "retry_safe": false,
            "delivery": "auto_when_composer_clear",
            "draft_chars": refusal.chars,
            "draft_lines": refusal.lines,
            "detail": format!(
                "{refusal} Queued as {} at position {position}: the daemon delivers it as \
                 its own turn once the composer is clear. Do not resend.",
                entry.id
            ),
        }),
    )
}

#[derive(Debug)]
enum SendKeysError {
    NotRunning,
    ResumeFailed(String),
    Transient(Status),
    StructuredView,
    /// The composer held an operator's unsent draft; NOTHING was typed.
    /// Distinct from `Tmux` because delivery state is CERTAIN (not sent)
    /// and a retry is safe once the composer clears.
    ParkedDraft(crate::tmux::ParkedDraftRefusal),
    /// The text was typed but its submitting Enter never registered after
    /// the resend budget: typed, not submitted.
    SubmitUnconfirmed(crate::tmux::SubmitUnconfirmed),
    Tmux(anyhow::Error),
}

/// The HTTP status + body for a failed send. Pure so the wire contract is
/// unit-testable without a pane: callers (fleet tooling, `aoe send` over
/// the daemon) branch on `error` and `sent`, and a parked-draft refusal
/// must never be mistakable for a transport failure.
fn send_error_parts(err: &SendKeysError) -> (StatusCode, serde_json::Value) {
    match err {
        SendKeysError::NotRunning => (
            StatusCode::CONFLICT,
            serde_json::json!({"error": "session_not_running"}),
        ),
        SendKeysError::ResumeFailed(sid) => (
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": "resume_failed",
                "message": format!("Resume failed for sid {sid}; preserved for explicit retry"),
                "resume_session_id": sid,
            }),
        ),
        SendKeysError::Transient(status) => (
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": "session_transient",
                "status": format!("{status:?}"),
            }),
        ),
        SendKeysError::StructuredView => (
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "acp_mode_unsupported"}),
        ),
        // 423 Locked: the resource (the composer) is held by someone else.
        // Never the draft's text — only its size, so the operator can
        // recognise their own half-written message.
        SendKeysError::ParkedDraft(refusal) => (
            StatusCode::LOCKED,
            serde_json::json!({
                "error": "parked_draft",
                "reason": "operator_draft_in_composer",
                "sent": false,
                "retry_safe": true,
                "retry_when": "composer_clear",
                "draft_chars": refusal.chars,
                "draft_lines": refusal.lines,
                "detail": refusal.to_string(),
            }),
        ),
        // 502: the pane (the upstream) accepted the text but not the submit.
        // A retry of the SAME message submits the parked copy instead of
        // double-pasting, so it is safe to retry.
        SendKeysError::SubmitUnconfirmed(unconfirmed) => (
            StatusCode::BAD_GATEWAY,
            serde_json::json!({
                "error": "submit_unconfirmed",
                "sent": false,
                "typed": true,
                "retry_safe": true,
                "attempts": unconfirmed.attempts,
                "prior_machine_message": unconfirmed.prior_machine_message,
                "detail": unconfirmed.to_string(),
            }),
        ),
        SendKeysError::Tmux(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": "tmux_error"}),
        ),
    }
}

/// Ok carries the urgent-flag acknowledgement so the response can say
/// whether delivery cleared (`cleared`), preserved (`kept`) or found no
/// (`absent`) urgent flag — see `hooks::ack_hook_urgent_on_send`.
type SendKeysResult = Result<
    (EnsureReadyOutcome, Instance, crate::hooks::UrgentAck),
    Box<(Instance, EnsureReadyOutcome, SendKeysError)>,
>;

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SendMessageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Terminal keystroke injection: CityHall sessions are structured-view only
    // (the composer drives the agent via the ACP prompt route), so close this
    // explicitly rather than leaning on the downstream StructuredView error.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    if req.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message_empty"})),
        )
            .into_response();
    }

    // Serialize concurrent sends (and other tmux mutations) for this id.
    // Without this, two POSTs racing against the same session would issue
    // overlapping `tmux send-keys -l` invocations and the bytes can interleave
    // inside the pane.
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let sync_base = instance.clone();
    let tool = instance.tool.clone();
    let message = req.message;
    let revive = req.revive;
    let queue_on_parked = req.queue;
    let sender = req.sender;
    let message_for_queue = message.clone();
    let send_result = tokio::task::spawn_blocking(move || -> SendKeysResult {
        // Revive the pane before sending. Without this, a send to a dead
        // pane silently writes keystrokes to a corpse with no agent.
        // Skipped when the caller opts out via `revive: false`.
        //
        // The closure surfaces both `inst_owned` AND the
        // `EnsureReadyOutcome` on the Err arm so the caller can sync
        // post-resume-path mutations (`agent_session_id`, failure marker,
        // and `retroactive_capture_excludes`) back to live state regardless
        // of which failure path fires. The
        // outcome lets the caller distinguish cascade-fired
        // (`Respawned`/`Started`) from the no-op `AlreadyAlive` path
        // so a sync only happens when there's actual cascade state to
        // propagate; this avoids clobbering live `last_error` on the
        // `revive=false + NotRunning` path where `started` is
        // unmutated.
        let mut inst_owned = instance;
        let outcome = if revive {
            match inst_owned.ensure_pane_ready() {
                Ok(o) => o,
                Err(e) => {
                    let mapped = match e {
                        EnsureReadyError::Transient(s) => SendKeysError::Transient(s),
                        EnsureReadyError::StructuredView => SendKeysError::StructuredView,
                        EnsureReadyError::Tmux(e) => SendKeysError::Tmux(e),
                    };
                    // ensure_pane_ready did not mutate user-visible
                    // state via the outcome path. Tag as AlreadyAlive
                    // so the outer match's `did_work` flag stays
                    // false. `EnsureReadyError::Tmux` may be either
                    // pre-cascade (tmux_session() / start_with_size
                    // subprocess failure: `inst_owned` unmutated) or
                    // post-resume-path (mutations committed).
                    // The Tmux outer arm syncs unconditionally and
                    // covers both shapes; the others (Transient /
                    // StructuredView) bail before any mutation.
                    return Err(Box::new((
                        inst_owned,
                        EnsureReadyOutcome::AlreadyAlive,
                        mapped,
                    )));
                }
            }
        } else {
            EnsureReadyOutcome::AlreadyAlive
        };
        if let EnsureReadyOutcome::ResumeFailed { sid } = &outcome {
            return Err(Box::new((
                inst_owned,
                outcome.clone(),
                SendKeysError::ResumeFailed(sid.clone()),
            )));
        }
        let tmux_session = match inst_owned.tmux_session() {
            Ok(s) => s,
            Err(e) => return Err(Box::new((inst_owned, outcome, SendKeysError::Tmux(e)))),
        };
        if !tmux_session.exists() {
            return Err(Box::new((inst_owned, outcome, SendKeysError::NotRunning)));
        }
        let delay = crate::agents::send_keys_enter_delay(&tool);
        if let Err(e) = tmux_session.send_keys_verified(&message, delay, &tool) {
            // Typed refusals cross the anyhow boundary intact; everything
            // else is a transport failure.
            let mapped = match e.downcast::<crate::tmux::ParkedDraftRefusal>() {
                Ok(refusal) => SendKeysError::ParkedDraft(refusal),
                Err(e) => match e.downcast::<crate::tmux::SubmitUnconfirmed>() {
                    Ok(unconfirmed) => SendKeysError::SubmitUnconfirmed(unconfirmed),
                    Err(e) => SendKeysError::Tmux(e),
                },
            };
            return Err(Box::new((inst_owned, outcome, mapped)));
        }
        // Delivery is the authoritative "someone is handling this": clear the
        // hook-written urgent flag here, on the daemon, so every sender (TUI,
        // CLI, web, relay, Commander) acks it the same way regardless of the
        // target session's own hooks. Sticky kinds survive machine traffic
        // (see `hooks::ack_hook_urgent_on_send`).
        let urgent_ack = crate::hooks::ack_hook_urgent_on_send(&inst_owned.id, &message);
        Ok((outcome, inst_owned, urgent_ack))
    })
    .await;

    match send_result {
        Ok(Ok((outcome, started, urgent_ack))) => {
            // ensure_pane_ready mutated `started` (status, agent_session_id,
            // last_start_time, last_error) on the clone. Sync those back to
            // the live entry so the next request sees a coherent view;
            // without this, a rapid follow-up could generate a fresh
            // `agent_session_id` and orphan the prior Claude conversation.
            // See `apply_post_restart_sync`. Also stamp last_accessed_at so
            // the activity column reflects API-driven interaction.
            let mut instances = state.instances.write().await;
            let profile = if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                if !matches!(outcome, EnsureReadyOutcome::AlreadyAlive) {
                    apply_post_restart_sync(i, &sync_base, &started);
                }
                i.touch_last_accessed();
                i.source_profile.clone()
            } else {
                // Session was deleted between the send and the stamp; nothing
                // left to persist.
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"sent": true, "urgent_ack": urgent_ack.as_str()})),
                )
                    .into_response();
            };
            drop(instances);
            let id_for_save = id.clone();
            let sync_base_for_save = sync_base.clone();
            let started_for_save = started.clone();
            let outcome_already_alive = matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            tokio::task::spawn_blocking(move || {
                if let Ok(storage) = Storage::new(&profile, state.file_watch.clone()) {
                    if let Err(e) = storage.update(|all, _groups| {
                        if let Some(disk_inst) = all.iter_mut().find(|i| i.id == id_for_save) {
                            if !outcome_already_alive {
                                apply_post_restart_sync(
                                    disk_inst,
                                    &sync_base_for_save,
                                    &started_for_save,
                                );
                            }
                            disk_inst.touch_last_accessed();
                        }
                        Ok(())
                    }) {
                        tracing::warn!(target: "http.api.sessions", "send_message: persist failed: {e}");
                    }
                }
            });
            (
                StatusCode::OK,
                Json(serde_json::json!({"sent": true, "urgent_ack": urgent_ack.as_str()})),
            )
                .into_response()
        }
        Ok(Err(boxed)) => {
            let (started, outcome, send_err) = *boxed;
            // ensure_pane_ready did mutate state when the outcome is
            // anything other than AlreadyAlive. `Started` and `Respawned`
            // touch fields the live entry needs to reflect (fresh sid from
            // acquire, last_start_time, etc.). Sync only when work happened.
            let did_work = !matches!(outcome, EnsureReadyOutcome::AlreadyAlive);
            match &send_err {
                SendKeysError::NotRunning => {
                    // External kill or remain-on-exit-off crash can race
                    // ensure_pane_ready's Alive decision against the
                    // tmux_session.exists() check. Propagate resume-path
                    // state when applicable; use the narrow sync helper to
                    // leave status and last_error untouched (NotRunning is
                    // recoverable; `started.status = Starting` from
                    // finalize_launch would briefly mis-paint a broken pane).
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                        }
                    }
                }
                SendKeysError::ResumeFailed(_) => {
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        apply_post_restart_sync(i, &sync_base, &started);
                    }
                }
                SendKeysError::Transient(_) | SendKeysError::StructuredView => {}
                SendKeysError::ParkedDraft(refusal) => {
                    // Nothing was typed and the session is healthy: no status
                    // or last_error paint. A refusal is not a session error;
                    // painting one would tell every observer the pane broke
                    // when a human is simply mid-sentence in it.
                    tracing::info!(target: "http.api.sessions", "send_message: refused for {id}: {refusal}");
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                        }
                    }
                    if queue_on_parked {
                        // The message is NOT lost: park it on the session's
                        // server-owned prompt queue. `server::send_queue`
                        // delivers it as its own turn once the composer
                        // clears, and the row survives a daemon restart.
                        match queue_parked_send(&state, &id, message_for_queue, sender).await {
                            Ok((entry, position)) => {
                                tracing::info!(target: "http.api.sessions",
                                    "send_message: queued for {id} as {} (position {position})", entry.id);
                                let (status, body) = queued_send_parts(refusal, &entry, position);
                                return (status, Json(body)).into_response();
                            }
                            Err(NotQueued::Gone) => {
                                return (
                                    StatusCode::NOT_FOUND,
                                    Json(serde_json::json!({"error": "not_found"})),
                                )
                                    .into_response();
                            }
                            Err(why) => {
                                // Plain refusal, annotated with why the
                                // queue would not take it.
                                tracing::warn!(target: "http.api.sessions",
                                    "send_message: refused for {id} and NOT queued: {why:?}");
                                let (status, mut body) = send_error_parts(&send_err);
                                body["queued"] = serde_json::json!(false);
                                match why {
                                    NotQueued::Full(depth) => {
                                        body["queue_full"] = serde_json::json!(true);
                                        body["queue_depth"] = serde_json::json!(depth);
                                    }
                                    NotQueued::TooLarge => {
                                        body["queue_too_large"] = serde_json::json!(true);
                                    }
                                    NotQueued::Gone => unreachable!(),
                                }
                                return (status, Json(body)).into_response();
                            }
                        }
                    }
                }
                SendKeysError::SubmitUnconfirmed(unconfirmed) => {
                    tracing::warn!(target: "http.api.sessions", "send_message: {id}: {unconfirmed}");
                    if did_work {
                        let mut instances = state.instances.write().await;
                        if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                            apply_cascade_state_sync(i, &sync_base, &started);
                        }
                    }
                }
                SendKeysError::Tmux(e) => {
                    tracing::error!(target: "http.api.sessions", "send_message: tmux error for {id}: {e}");
                    let msg = e.to_string();
                    // Sync cascade-mutated fields back to live state. Mirror
                    // `ensure_session`'s Err arm: full sync, then override
                    // `status` and `last_error` so observers don't see
                    // `Status::Starting` (set by `finalize_launch`) on a
                    // broken session. Tmux Err is the
                    // catch-all for both pre-cascade tmux failures (where
                    // `started` is unmutated and the sync is a no-op) and
                    // post-resume-path failures (where durable resume state
                    // must be copied back from the clone).
                    let mut instances = state.instances.write().await;
                    if let Some(i) = instances.iter_mut().find(|i| i.id == id) {
                        if apply_post_restart_sync(i, &sync_base, &started) {
                            i.status = crate::session::Status::Error;
                            i.last_error = Some(msg);
                        }
                    }
                }
            }
            let (status, body) = send_error_parts(&send_err);
            (status, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "send_message: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

/// Max decoded size of a pasted image (5 MiB). Claude Code caps image
/// attachments around this size; the route body limit in `build_router`
/// leaves headroom for base64's ~33% overhead plus JSON framing.
const MAX_PASTE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Directory, relative to the session worktree, holding images pasted into
/// the live terminal. It lives inside the worktree so a Docker-sandboxed
/// pane, which mounts the worktree but cannot see the host temp dir, can
/// still read the file. A self-ignoring `.gitignore` keeps the blobs out of
/// git. See #2678.
const PASTE_IMAGE_DIR: &str = ".aoe-pasted-images";

#[derive(Deserialize)]
pub struct PasteImageRequest {
    /// Client-declared MIME. Advisory only: the extension and the
    /// accept/reject decision come from magic-byte sniffing, never this field.
    #[serde(default)]
    pub mime_type: String,
    /// Standard-base64 image bytes.
    pub data: String,
}

fn paste_image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Write the decoded blob into the worktree's paste-image dir and return the
/// host path plus the generated file name. Sync (filesystem I/O); call from a
/// blocking pool.
fn write_paste_image(
    project_path: &str,
    bytes: &[u8],
    ext: &str,
) -> std::io::Result<(std::path::PathBuf, String)> {
    let dir = std::path::Path::new(project_path).join(PASTE_IMAGE_DIR);
    std::fs::create_dir_all(&dir)?;
    // A `.gitignore` of `*` also ignores itself, so the whole directory stays
    // invisible to `git add` with no git subprocess.
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    let file_name = format!("aoe-paste-{}.{}", uuid::Uuid::new_v4(), ext);
    let path = dir.join(&file_name);
    // create_new: uuid names never collide; fail loud if the impossible happens.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    Ok((path, file_name))
}

/// Map the host paste-image file to the path the tmux pane reads. Non-sandboxed
/// panes share the host filesystem, so the absolute host path is correct. A
/// sandboxed pane mounts the worktree under a container path (`/workspace/...`);
/// reuse `compute_volume_paths` so the pasted path matches that mount.
fn pane_visible_paste_path(project_path: &str, is_sandboxed: bool, file_name: &str) -> String {
    if is_sandboxed {
        if let Ok((_, working_dir)) = crate::session::config::container_config::compute_volume_paths(
            std::path::Path::new(project_path),
            project_path,
        ) {
            return format!("{working_dir}/{PASTE_IMAGE_DIR}/{file_name}");
        }
    }
    std::path::Path::new(project_path)
        .join(PASTE_IMAGE_DIR)
        .join(file_name)
        .to_string_lossy()
        .to_string()
}

/// Save a clipboard image pasted into the live terminal and return the path
/// the tmux pane can read, so the CLI agent (e.g. Claude Code) attaches it.
/// See #2678.
pub async fn paste_image(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PasteImageRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    use base64::Engine as _;

    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Allowed for the CityHall composer, but only against a structured
    // session: a plain/terminal target would let a locked-down client write
    // into another session's worktree.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data.as_bytes()) {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_base64"})),
            )
                .into_response();
        }
    };
    if bytes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "empty"})),
        )
            .into_response();
    }
    if bytes.len() > MAX_PASTE_IMAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "too_large"})),
        )
            .into_response();
    }
    let Some(mime) = crate::server::api::acp::sniff_image_mime(&bytes) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "not_an_image"})),
        )
            .into_response();
    };
    let ext = paste_image_extension(mime);

    let project_path = instance.project_path.clone();
    let is_sandboxed = instance.is_sandboxed();
    let write_project = project_path.clone();
    let (host_path, file_name) =
        match tokio::task::spawn_blocking(move || write_paste_image(&write_project, &bytes, ext))
            .await
        {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: write failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!(target: "http.api.sessions", "paste_image: join failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "write_failed"})),
                )
                    .into_response();
            }
        };

    // Best-effort TTL cleanup: the file only needs to outlive the agent
    // reading it. A detached task keeps the worktree from accumulating blobs
    // without any teardown bookkeeping.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        let _ = tokio::fs::remove_file(&host_path).await;
    });

    let pane_path = pane_visible_paste_path(&project_path, is_sandboxed, &file_name);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "path": pane_path })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default = "default_output_lines")]
    pub lines: u32,
    #[serde(default = "default_output_format")]
    pub format: String,
}

fn default_output_lines() -> u32 {
    200
}

fn default_output_format() -> String {
    "text".to_string()
}

enum CaptureError {
    NotRunning,
    Tmux(anyhow::Error),
}

pub async fn read_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<OutputQuery>,
) -> impl IntoResponse {
    // Raw terminal pane content: CityHall hides the terminal UI + WS relay, so
    // this read must be closed too or the pane is reachable by session id.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let lines = (q.lines as usize).clamp(1, 2000);
    let want_ansi = match q.format.as_str() {
        "ansi" => true,
        "text" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "format_invalid",
                    "allowed": ["text", "ansi"]
                })),
            )
                .into_response();
        }
    };

    let instances = state.instances.read().await;
    let Some(instance) = instances.iter().find(|i| i.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    drop(instances);

    let capture_result = tokio::task::spawn_blocking(move || -> Result<String, CaptureError> {
        let tmux_session = instance.tmux_session().map_err(CaptureError::Tmux)?;
        if !tmux_session.exists() {
            return Err(CaptureError::NotRunning);
        }
        let raw = tmux_session
            .capture_pane(lines)
            .map_err(CaptureError::Tmux)?;
        if want_ansi {
            Ok(raw)
        } else {
            Ok(crate::tmux::utils::strip_ansi(&raw))
        }
    })
    .await;

    match capture_result {
        Ok(Ok(content)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": id,
                "lines": lines,
                "format": q.format,
                "content": content,
            })),
        )
            .into_response(),
        Ok(Err(CaptureError::NotRunning)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "session_not_running"})),
        )
            .into_response(),
        Ok(Err(CaptureError::Tmux(e))) => {
            tracing::error!(target: "http.api.sessions", "read_output: tmux error for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "tmux_error"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "read_output: blocking task panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod send_output_tests {
    use super::*;

    #[test]
    fn output_query_default_constants() {
        assert_eq!(default_output_lines(), 200);
        assert_eq!(default_output_format(), "text");
    }

    #[test]
    fn send_message_request_requires_message_field() {
        let r: Result<SendMessageRequest, _> = serde_json::from_str("{}");
        assert!(r.is_err(), "missing message must reject");
    }

    #[test]
    fn send_message_request_accepts_message() {
        let r: SendMessageRequest = serde_json::from_str("{\"message\":\"hello\"}").unwrap();
        assert_eq!(r.message, "hello");
    }
}

#[cfg(test)]
mod paste_image_tests {
    use super::*;
    use tempfile::tempdir;

    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    #[test]
    fn extension_from_sniffed_mime() {
        assert_eq!(paste_image_extension("image/png"), "png");
        assert_eq!(paste_image_extension("image/jpeg"), "jpg");
        assert_eq!(paste_image_extension("image/gif"), "gif");
        assert_eq!(paste_image_extension("image/webp"), "webp");
    }

    #[test]
    fn write_paste_image_lands_in_worktree_and_ignores_itself() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let (path, name) = write_paste_image(&project, PNG_1PX, "png").unwrap();

        assert!(path.exists(), "image file must be written");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            PNG_1PX,
            "bytes must round-trip"
        );
        assert!(name.starts_with("aoe-paste-") && name.ends_with(".png"));
        let gitignore = dir.path().join(PASTE_IMAGE_DIR).join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(gitignore).unwrap(),
            "*\n",
            "dir must self-ignore so pasted blobs never reach git"
        );
    }

    #[test]
    fn non_sandboxed_pane_path_is_absolute_host_path() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let pane = pane_visible_paste_path(&project, false, "aoe-paste-x.png");

        let expected = dir
            .path()
            .join(PASTE_IMAGE_DIR)
            .join("aoe-paste-x.png")
            .to_string_lossy()
            .to_string();
        assert_eq!(pane, expected);
    }

    #[test]
    fn sandboxed_pane_path_uses_container_mount() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let dir_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let pane = pane_visible_paste_path(&project, true, "aoe-paste-x.png");

        // A non-git worktree mounts under /workspace/<dir-name>; the pasted
        // path must be the container-visible path, not the host path.
        assert_eq!(
            pane,
            format!("/workspace/{dir_name}/{PASTE_IMAGE_DIR}/aoe-paste-x.png")
        );
    }
}

/// `POST /api/sessions/{id}/urgent_ack` — explicit acknowledgement of the
/// hook-written urgent flag (WO#1832). The pane-watchdog sidecar stamps
/// `urgent` on a capped / logged-out row; until this route the only thing
/// that stripped it was a delivered `send` (`ack_hook_urgent_on_send`), and
/// `PATCH {urgent:false}` is 422 — so a stale flag on a healthy row could only
/// be cleared by messaging the session. An explicit ack clears sticky kinds
/// too: it is the human (or the Commander) seeing the row.
/// Response: `{"id", "urgent_ack": "cleared"|"absent"|"kept", "urgent": bool}`.
pub async fn urgent_ack_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let known = {
        let instances = state.instances.read().await;
        instances.iter().any(|i| i.id == id)
    };
    if !known {
        return crate::server::api::session_not_found();
    }
    let ack = crate::hooks::ack_hook_urgent(&id);
    let urgent = crate::hooks::read_hook_urgent(&id);
    Json(serde_json::json!({
        "id": id,
        "urgent_ack": ack.as_str(),
        "urgent": urgent,
    }))
    .into_response()
}

#[cfg(test)]
mod send_error_contract_tests {
    use super::*;

    /// A parked-draft refusal is 423 Locked with a machine-readable body
    /// that says NOT SENT, safe to retry, and when — and carries the draft's
    /// size, never its text.
    #[test]
    fn parked_draft_is_423_locked_and_withholds_the_draft() {
        let draft = "yes, authorize the vendor bump and send the escalation emails";
        let refusal = crate::tmux::ParkedDraftRefusal::from_draft(draft);
        let (status, body) = send_error_parts(&SendKeysError::ParkedDraft(refusal));
        assert_eq!(status, StatusCode::LOCKED);
        assert_eq!(body["error"], "parked_draft");
        assert_eq!(body["reason"], "operator_draft_in_composer");
        assert_eq!(body["sent"], false);
        assert_eq!(body["retry_safe"], true);
        assert_eq!(body["retry_when"], "composer_clear");
        assert_eq!(body["draft_chars"], draft.chars().count());
        assert_eq!(body["draft_lines"], 1);
        let wire = body.to_string();
        assert!(
            !wire.contains("vendor"),
            "refusal body leaked the draft: {wire}"
        );
        assert!(body["detail"].as_str().unwrap().contains("NOT SENT"));
    }

    /// A parked send that was QUEUED is 202 Accepted: still `sent: false`
    /// and `error: parked_draft` for callers that branch on those, plus the
    /// queue row's id and position; a resend is flagged unsafe (it would
    /// double-queue). The draft's text is still withheld.
    #[test]
    fn queued_parked_send_is_202_with_queue_id_and_position() {
        let draft = "yes, authorize the vendor bump and send the escalation emails";
        let refusal = crate::tmux::ParkedDraftRefusal::from_draft(draft);
        let entry = crate::acp::state::QueuedPromptEntry {
            id: "send-0123456789ab".to_string(),
            seq: 7,
            text: "STATUS: shipped".to_string(),
            attachments: Vec::new(),
            created_at: "2026-09-04T08:30:00Z".to_string(),
            origin_device: Some("aoe send".to_string()),
        };
        let (status, body) = queued_send_parts(&refusal, &entry, 2);
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["error"], "parked_draft");
        assert_eq!(body["sent"], false);
        assert_eq!(body["queued"], true);
        assert_eq!(body["queue_id"], "send-0123456789ab");
        assert_eq!(body["position"], 2);
        assert_eq!(body["retry_safe"], false);
        assert_eq!(body["delivery"], "auto_when_composer_clear");
        assert_eq!(body["draft_chars"], draft.chars().count());
        let wire = body.to_string();
        assert!(
            !wire.contains("vendor"),
            "queued body leaked the draft: {wire}"
        );
        assert!(body["detail"]
            .as_str()
            .unwrap()
            .contains("send-0123456789ab"));
    }

    #[test]
    fn send_queue_ids_are_short_and_prefixed() {
        let a = new_send_queue_id();
        let b = new_send_queue_id();
        assert!(a.starts_with("send-") && a.len() == 17, "{a}");
        assert_ne!(a, b);
    }

    /// The request defaults: queueing is on unless the caller opts out.
    #[test]
    fn send_request_defaults_to_queue() {
        let req: SendMessageRequest = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert!(req.queue);
        assert!(req.sender.is_none());
        let req: SendMessageRequest =
            serde_json::from_str(r#"{"message":"hi","queue":false,"sender":"for-dev"}"#).unwrap();
        assert!(!req.queue);
        assert_eq!(req.sender.as_deref(), Some("for-dev"));
    }

    /// Typed-but-unsubmitted is 502 and says so: sent=false, typed=true.
    #[test]
    fn submit_unconfirmed_is_502_typed_not_sent() {
        let (status, body) = send_error_parts(&SendKeysError::SubmitUnconfirmed(
            crate::tmux::SubmitUnconfirmed {
                attempts: 3,
                prior_machine_message: false,
            },
        ));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"], "submit_unconfirmed");
        assert_eq!(body["sent"], false);
        assert_eq!(body["typed"], true);
        assert_eq!(body["retry_safe"], true);
        assert_eq!(body["attempts"], 3);
    }

    /// The pre-existing arms keep their wire shape.
    #[test]
    fn legacy_arms_unchanged() {
        let (s, b) = send_error_parts(&SendKeysError::NotRunning);
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::CONFLICT, Some("session_not_running"))
        );
        let (s, b) = send_error_parts(&SendKeysError::ResumeFailed("abc".into()));
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(b["error"], "resume_failed");
        assert_eq!(b["resume_session_id"], "abc");
        let (s, b) = send_error_parts(&SendKeysError::Transient(Status::Starting));
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::CONFLICT, Some("session_transient"))
        );
        assert_eq!(b["status"], "Starting");
        let (s, b) = send_error_parts(&SendKeysError::StructuredView);
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("acp_mode_unsupported"))
        );
        let (s, b) = send_error_parts(&SendKeysError::Tmux(anyhow::anyhow!("no server")));
        assert_eq!(
            (s, b["error"].as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, Some("tmux_error"))
        );
        assert!(
            b.get("sent").is_none(),
            "transport failure must not claim a delivery state"
        );
    }
}
