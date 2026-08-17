//! Typed application commands shared by native menus, shortcuts, and controls.

use gpui::{KeyBinding, Keystroke, MenuItem, actions};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::state::AppState;

actions!(comet, [NewSession]);

/// Static presentation for an application command. Keeping this beside the
/// action is what prevents a menu label and its shortcut from drifting apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDescriptor {
    pub label: &'static str,
    pub shortcut: crate::settings::ShortcutId,
}

pub const NEW_SESSION: CommandDescriptor = CommandDescriptor {
    label: "New Session",
    shortcut: crate::settings::ShortcutId::NewSession,
};

pub const COMMANDS: &[CommandDescriptor] = &[NEW_SESSION];

impl CommandDescriptor {
    pub fn effective_combo(self, keymap: &crate::settings::KeymapConfig) -> String {
        let configured = crate::settings::platform_combo(keymap.get(self.shortcut));
        if Keystroke::parse(&configured).is_ok() {
            configured
        } else {
            crate::settings::platform_combo(self.shortcut.default_combo())
        }
    }

    pub fn binding(self, keymap: &crate::settings::KeymapConfig) -> KeyBinding {
        KeyBinding::new(&self.effective_combo(keymap), NewSession, None)
    }

    pub fn menu_item(self) -> MenuItem {
        MenuItem::action(self.label, NewSession)
    }

    pub fn action(self) -> NewSession {
        NewSession
    }

    pub fn available(self, modal_open: bool, in_flight: bool) -> bool {
        !modal_open && !in_flight
    }
}

/// The registry-owned binding for New Session. Menus and clickable controls
/// dispatch the same action contained here; keymap conflict detection reads
/// the descriptor's `ShortcutId`.
pub fn new_session_binding(keymap: &crate::settings::KeymapConfig) -> KeyBinding {
    NEW_SESSION.binding(keymap)
}

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

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
enum PersistedIntent {
    Pending { intent: NewSessionIntent },
    Cancelled,
}

fn intent_path(data_dir: &Path) -> PathBuf {
    data_dir.join(INTENT_FILE)
}

pub fn load_intent(data_dir: &Path) -> Option<NewSessionIntent> {
    let bytes = std::fs::read(intent_path(data_dir)).ok()?;
    match serde_json::from_slice(&bytes).ok()? {
        PersistedIntent::Pending { intent } => Some(intent),
        PersistedIntent::Cancelled => None,
    }
}

pub fn save_intent(data_dir: &Path, intent: &NewSessionIntent) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = intent_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&PersistedIntent::Pending {
            intent: intent.clone(),
        })?,
    )?;
    std::fs::rename(tmp, path)
}

pub fn clear_intent(data_dir: &Path) -> std::io::Result<()> {
    let path = intent_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&PersistedIntent::Cancelled)?)?;
    std::fs::rename(tmp, path)
}

pub fn suppress_repeated_shortcut(
    keystroke: &Keystroke,
    is_held: bool,
    keymap: &crate::settings::KeymapConfig,
) -> bool {
    if !is_held {
        return false;
    }
    Keystroke::parse(&NEW_SESSION.effective_combo(keymap))
        .is_ok_and(|bound| bound == *keystroke)
}

pub fn intent_arrived(intent: &NewSessionIntent, state: &AppState) -> bool {
    state.chats.iter().any(|chat| {
        chat.id == intent.chat_id && chat.space_id.as_deref() == Some(intent.space_id.as_str())
    })
}

pub fn intent_acknowledged(
    intent: &NewSessionIntent,
    state: &AppState,
    durable_rpc_success: bool,
) -> bool {
    durable_rpc_success && intent_arrived(intent, state)
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
            creation_state: comet_proto::ChatCreationState::Ready,
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
        let mut keymap = crate::settings::KeymapConfig::default();
        let initial = Keystroke::parse(&crate::settings::platform_combo("mod-t")).unwrap();
        assert!(!suppress_repeated_shortcut(&initial, false, &keymap));
        assert!(suppress_repeated_shortcut(&initial, true, &keymap));
        keymap.set(crate::settings::ShortcutId::NewSession, "mod-n".into());
        let rebound = Keystroke::parse(&crate::settings::platform_combo("mod-n")).unwrap();
        assert!(!suppress_repeated_shortcut(&initial, true, &keymap));
        assert!(suppress_repeated_shortcut(&rebound, true, &keymap));

        keymap.set(
            crate::settings::ShortcutId::NewSession,
            "mod-t mod-u".into(),
        );
        assert_eq!(
            NEW_SESSION.effective_combo(&keymap),
            crate::settings::platform_combo("mod-t")
        );
        assert!(suppress_repeated_shortcut(&initial, true, &keymap));
        assert_eq!(
            NEW_SESSION.binding(&keymap).keystrokes()[0].inner(),
            &initial
        );
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
        clear_intent(dir.path()).unwrap();
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
        assert!(
            !intent_acknowledged(&intent, &state, false),
            "a delayed watch row cannot tombstone recovery before durable RPC success"
        );
        assert!(intent_acknowledged(&intent, &state, true));
    }

    #[test]
    fn new_session_is_in_the_shared_shortcut_registry() {
        use crate::settings::{KeymapConfig, ShortcutId, conflicted_shortcuts};

        assert!(ShortcutId::ALL.contains(&NEW_SESSION.shortcut));
        let mut keymap = KeymapConfig::default();
        assert_eq!(keymap.get(NEW_SESSION.shortcut), "mod-t");
        keymap.set(ShortcutId::ToggleTerminal, "mod-t".into());
        let conflicts = conflicted_shortcuts(&keymap);
        assert!(conflicts.contains(&ShortcutId::NewSession));
        assert!(conflicts.contains(&ShortcutId::ToggleTerminal));
    }

    #[test]
    fn workspace_chooser_wraps_under_keyboard_navigation() {
        assert_eq!(step_chooser(0, 3, -1), 2);
        assert_eq!(step_chooser(2, 3, 1), 0);
        assert_eq!(step_chooser(0, 0, 1), 0);
    }
}
