//! WO#1953 — the per-session `keep` flag on the TUI surface: archive,
//! snooze and trash at the cursor refuse a kept row with an info dialog
//! that names the flag and the clear command; a group archive skips kept
//! members; the row renders a `⚓ ` marker so the flag is visible.

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

fn assert_kept_dialog(env: &TestEnv, id: &str, op: &str) {
    let dialog = env
        .view
        .info_dialog
        .as_ref()
        .unwrap_or_else(|| panic!("{op} on a kept row must open the info dialog"));
    assert_eq!(dialog.title(), "Kept session");
    let msg = dialog.message();
    assert!(msg.contains("kept"), "{op}: {msg}");
    assert!(msg.contains(op), "{op}: {msg}");
    assert!(
        msg.contains(&format!("aoe session keep --off {id}")),
        "{op}: {msg}"
    );
}

#[test]
#[serial]
fn archive_at_cursor_refuses_a_kept_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);

    env.view.toggle_archive_at_cursor().unwrap();

    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_archived(), "kept row must not archive");
    assert!(inst.is_kept());
    assert_kept_dialog(&env, &id, "archive");
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(id.as_str()),
        "cursor stays on the refused row"
    );
}

#[test]
#[serial]
fn snooze_refuses_a_kept_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);

    let status = env.view.snooze_session_for(&id, 30).unwrap();

    assert!(
        status.is_none(),
        "no 'Snoozed' status line for a refused op"
    );
    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_snoozed() && inst.is_kept());
    assert_kept_dialog(&env, &id, "snooze");
}

#[test]
#[serial]
fn trash_refuses_a_kept_session() {
    let mut env = create_test_env_with_sessions(2);
    let id = keep_selected(&mut env);

    env.view.trash_session_by_id(&id);

    let inst = env.view.get_instance(&id).unwrap();
    assert!(!inst.is_trashed() && inst.is_kept());
    assert!(
        inst.lifecycle_reservation.is_none(),
        "no Trash reservation may be taken on a kept row"
    );
    assert_kept_dialog(&env, &id, "trash");
    assert!(!on_disk(&env, &id).is_trashed(), "trash must not persist");
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
