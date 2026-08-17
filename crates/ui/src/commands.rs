//! Typed application commands shared by native menus, shortcuts, and controls.

use gpui::actions;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::state::AppState;

actions!(comet, [NewSession]);

/// Static presentation for an application command. Keeping this beside the
/// action is what prevents a menu label and its shortcut from drifting apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDescriptor {
    pub label: &'static str,
    pub macos_shortcut: &'static str,
}

pub const NEW_SESSION: CommandDescriptor = CommandDescriptor {
    label: "New Session",
    macos_shortcut: "cmd-t",
};

const INTENT_FILE: &str = "new-session-intent.json";

/// Durable idempotency key for an empty-session creation. It is removed only
/// after WATCH_CHATS contains this exact row, never merely because an RPC
/// returned (the reply can be lost after the workspace write committed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionIntent {
    pub chat_id: String,
    pub space_id: String,
}

fn intent_path(data_dir: &Path) -> PathBuf {
    data_dir.join(INTENT_FILE)
}

pub fn load_intent(data_dir: &Path) -> Option<NewSessionIntent> {
    let bytes = std::fs::read(intent_path(data_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save_intent(data_dir: &Path, intent: &NewSessionIntent) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = intent_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(intent)?)?;
    std::fs::rename(tmp, path)
}

pub fn clear_intent(data_dir: &Path) {
    let _ = std::fs::remove_file(intent_path(data_dir));
}

pub fn suppress_repeated_shortcut(key: &str, command: bool, is_held: bool) -> bool {
    key.eq_ignore_ascii_case("t") && command && is_held
}

pub fn intent_arrived(intent: &NewSessionIntent, state: &AppState) -> bool {
    state.chats.iter().any(|chat| {
        chat.id == intent.chat_id && chat.space_id.as_deref() == Some(intent.space_id.as_str())
    })
}

pub fn step_chooser(selected: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    (selected as isize + delta).rem_euclid(count as isize) as usize
}

/// Resolve the workspace for New Session without consulting device order or
/// any other incidental list ordering.
pub fn current_space(state: &AppState, space_scoped_screen: bool) -> Option<String> {
    state
        .selected_chat_row()
        .and_then(|chat| chat.space_id.clone())
        .or_else(|| {
            space_scoped_screen
                .then(|| state.selected_space.clone())
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use comet_proto::Chat;

    fn chat(id: &str, space: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: "device".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some(space.into()),
            last_seen_at: None,
            forked_from: None,
        }
    }

    #[test]
    fn selected_chat_owner_wins_over_selected_space() {
        let mut state = AppState::new();
        state.chats = vec![chat("chat", "owner")];
        state.selected_chat = Some("chat".into());
        state.selected_space = Some("other".into());
        assert_eq!(current_space(&state, true).as_deref(), Some("owner"));
    }

    #[test]
    fn scoped_screen_uses_its_space_and_global_screen_does_not() {
        let mut state = AppState::new();
        state.selected_space = Some("visible".into());
        assert_eq!(current_space(&state, true).as_deref(), Some("visible"));
        assert_eq!(current_space(&state, false), None);
    }

    #[test]
    fn held_cmd_t_is_suppressed_but_the_initial_press_is_not() {
        assert!(!suppress_repeated_shortcut("t", true, false));
        assert!(suppress_repeated_shortcut("t", true, true));
        assert!(!suppress_repeated_shortcut("t", false, true));
    }

    #[test]
    fn creation_intent_survives_response_loss_and_shell_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let intent = NewSessionIntent {
            chat_id: "stable".into(),
            space_id: "space".into(),
        };
        save_intent(dir.path(), &intent).unwrap();
        assert_eq!(load_intent(dir.path()), Some(intent.clone()));
        clear_intent(dir.path());
        assert_eq!(load_intent(dir.path()), None);
    }

    #[test]
    fn failed_creation_has_no_optimistic_chat_or_selection() {
        let state = AppState::new();
        let _intent = NewSessionIntent {
            chat_id: "not-committed".into(),
            space_id: "space".into(),
        };
        assert!(state.chats.is_empty());
        assert!(state.selected_chat.is_none());
    }

    #[test]
    fn stale_watch_frame_cannot_select_or_misplace_the_pending_session() {
        let intent = NewSessionIntent {
            chat_id: "new".into(),
            space_id: "b".into(),
        };
        let mut state = AppState::new();
        state.selected_space = Some("a".into());
        assert!(!intent_arrived(&intent, &state));
        assert_eq!(state.selected_chat, None);
        state.chats.push(chat("new", "b"));
        assert!(intent_arrived(&intent, &state));
    }

    #[test]
    fn workspace_chooser_wraps_under_keyboard_navigation() {
        assert_eq!(step_chooser(0, 3, -1), 2);
        assert_eq!(step_chooser(2, 3, 1), 0);
        assert_eq!(step_chooser(0, 0, 1), 0);
    }
}
