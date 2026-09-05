//! WO#1953 / WO#1980-1 — the per-session `keep` flag on the TUI surface:
//! archive, snooze and trash at the cursor do NOT refuse the human — they
//! open a confirm ("kept since <when> by <who> — <op> anyway?"); Yes clears
//! the flag and proceeds in one step, No leaves the row exactly as it was.
//! A group archive (a sweep) still skips kept members; the row renders a
//! `⚓ ` marker so the flag is visible.

use super::*;

/// Keep the row under the cursor on BOTH surfaces the view consults: the
/// in-memory snapshot (what the pre-lock refusal reads) and the profile's
/// sessions.json (what `apply_user_action` reloads under the lifecycle lock).
fn keep_selected(env: &mut TestEnv) -> String {
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();
    env.view
        .mutate_instance(&id, |inst| inst.keep(Some("test:seed")));
    let profile = env.view.get_instance(&id).unwrap().source_profile.clone();
    env.view
        .storages
        .get(&profile)
        .unwrap()
        .update(|instances, _| {
            instances
                .iter_mut()
                .find(|i| i.id == id)
                .unwrap()
                .keep(Some("test:seed"));
            Ok(())
        })
        .unwrap();
    id
}

fn on_disk(env: &TestEnv, id: &str) -> Instance {
    let profile = env.view.get_instance(id).unwrap().source_profile.clone();
    env.view
        .storages
        .get(&profile)
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|i| i.id == id)
        .unwrap()
}

fn assert_kept_confirm(env: &TestEnv, op: &str) {
    assert!(
        env.view.info_dialog.is_none(),
        "{op}: a human at the TUI gets a confirm, not a refusal"
    );
    let dialog = env
        .view
        .confirm_dialog
        .as_ref()
        .unwrap_or_else(|| panic!("{op} on a kept row must open the confirm dialog"));
    assert_eq!(dialog.action(), "kept_override");
    assert_eq!(dialog.title_for_test(), "Kept session");
    let msg = dialog.message_for_test();
    assert!(msg.contains("kept since"), "{op}: {msg}");
    assert!(msg.contains("test:seed"), "{op}: names who kept it: {msg}");
    assert!(msg.to_lowercase().contains("anyway"), "{op}: {msg}");
    assert!(msg.to_lowercase().contains(op), "{op}: {msg}");
}

fn press(env: &mut TestEnv, c: char) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), None);
}

#[test]
#[serial]
fn archive_at_cursor_asks_before_touching_a_kept_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);

    env.view.toggle_archive_at_cursor().unwrap();

    let inst = env.view.get_instance(&id).unwrap();
    assert!(
        !inst.is_archived(),
        "nothing happens until the human answers"
    );
    assert!(inst.is_kept());
    assert_kept_confirm(&env, "archive");
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(id.as_str()),
        "cursor stays on the row"
    );
}

#[test]
#[serial]
fn confirming_the_kept_archive_clears_the_flag_and_archives_in_one_step() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);
    env.view.toggle_archive_at_cursor().unwrap();
    assert_kept_confirm(&env, "archive");

    press(&mut env, 'y');

    assert!(env.view.confirm_dialog.is_none(), "dialog closes on yes");
    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_kept(), "yes clears the keep flag");
    assert!(inst.is_archived(), "and the archive proceeds");
    let disk = on_disk(&env, &id);
    assert!(!disk.is_kept() && disk.is_archived(), "both persisted");
}

#[test]
#[serial]
fn cancelling_the_kept_archive_leaves_the_row_untouched() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);
    env.view.toggle_archive_at_cursor().unwrap();
    assert_kept_confirm(&env, "archive");

    press(&mut env, 'n');

    assert!(env.view.confirm_dialog.is_none());
    let inst = env.view.get_instance(&id).unwrap();
    assert!(inst.is_kept() && !inst.is_archived());
    let disk = on_disk(&env, &id);
    assert!(disk.is_kept() && !disk.is_archived());
    // a later, unrelated confirm must not replay the kept override
    assert!(env.view.pending_kept_override.is_none());
}

#[test]
#[serial]
fn snooze_asks_then_confirms_on_a_kept_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);

    let status = env.view.snooze_session_for(&id, 30).unwrap();

    assert!(status.is_none(), "no 'Snoozed' status line until confirmed");
    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_snoozed() && inst.is_kept());
    assert_kept_confirm(&env, "snooze");

    press(&mut env, 'y');

    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_kept() && inst.is_snoozed(), "yes clears + snoozes");
    let disk = on_disk(&env, &id);
    assert!(!disk.is_kept() && disk.is_snoozed());
}

#[test]
#[serial]
fn trash_asks_then_confirms_on_a_kept_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);

    env.view.trash_session_by_id(&id);

    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_trashed() && inst.is_kept());
    assert!(
        inst.lifecycle_reservation.is_none(),
        "no Trash reservation may be taken before the human answers"
    );
    assert_kept_confirm(&env, "trash");
    assert!(
        !on_disk(&env, &id).is_trashed(),
        "trash must not persist yet"
    );

    press(&mut env, 'y');

    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_kept(), "yes clears the flag");
    assert!(
        inst.is_trashed() || inst.lifecycle_reservation.is_some(),
        "and the trash proceeds (applied, or reserved for the async worker)"
    );
    assert!(!on_disk(&env, &id).is_kept(), "clear persisted");
}

#[test]
#[serial]
fn group_archive_skips_kept_members_and_archives_the_rest() {
    let mut env = create_test_env_with_sessions(3);
    let kept = keep_selected(&mut env);
    let others: Vec<String> = env
        .view
        .instances
        .keys()
        .filter(|k| **k != kept)
        .cloned()
        .collect();
    // The fixture is Manual-grouped and every session sits at group path "",
    // so selecting that header spans all three rows.
    env.view.selected_group = Some(String::new());

    env.view.archive_selected_group().unwrap();

    assert!(!env.view.get_instance(&kept).unwrap().is_archived());
    assert!(!on_disk(&env, &kept).is_archived());
    for id in &others {
        assert!(
            env.view.get_instance(id).unwrap().is_archived(),
            "non-kept member {id} must archive"
        );
    }
}

#[test]
fn kept_rows_carry_the_anchor_marker_in_the_title() {
    let mut inst = Instance::new("kept-me", "/tmp/k");
    assert_eq!(
        super::super::render::row_title_for_test(&inst, false, false),
        "kept-me"
    );
    inst.keep(Some("tui"));
    assert_eq!(
        super::super::render::row_title_for_test(&inst, false, false),
        "⚓ kept-me"
    );
    // Keep wins over the favorite star: the flag is the more consequential
    // state (it blocks sweeps), so it must not be hidden behind `*`.
    inst.favorite();
    assert_eq!(
        super::super::render::row_title_for_test(&inst, true, true),
        "⚓ kept-me"
    );
}
