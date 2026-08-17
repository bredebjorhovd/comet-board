//! Typed application commands shared by native menus, shortcuts, and controls.

use gpui::actions;

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
}
