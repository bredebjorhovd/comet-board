//! Composed New Session interaction coverage. These tests mount the real
//! `Shell`, dispatch its registered GPUI action/shortcut, and speak the same
//! in-memory RPC envelopes as production while controlling WATCH_CHATS order.

use super::*;
use async_trait::async_trait;
use futures::{StreamExt, channel::mpsc, stream};
use gpui::{Action as _, KeyDownEvent, Keystroke, MenuItem, TestAppContext, VisualTestContext};
use std::sync::{Arc, Mutex};

use comet_proto::{Chat, ChatCreationState, Space};
use comet_rpc::{RpcError, RpcReply, RpcService, methods};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateReply {
    Success,
    DefinitiveFailure,
    CommitThenLoseReply,
}

struct FakeNewSessionRpc {
    chats: Mutex<Vec<Chat>>,
    spaces: Vec<Space>,
    chat_watchers: Mutex<Vec<mpsc::UnboundedSender<serde_json::Value>>>,
    reply: Mutex<CreateReply>,
    create_calls: Mutex<Vec<(String, String)>>,
    finalize_calls: Mutex<Vec<serde_json::Value>>,
}

impl FakeNewSessionRpc {
    fn new(chats: Vec<Chat>, spaces: Vec<Space>) -> Arc<Self> {
        Arc::new(Self {
            chats: Mutex::new(chats),
            spaces,
            chat_watchers: Mutex::new(Vec::new()),
            reply: Mutex::new(CreateReply::Success),
            create_calls: Mutex::new(Vec::new()),
            finalize_calls: Mutex::new(Vec::new()),
        })
    }

    fn set_reply(&self, reply: CreateReply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn publish_chats(&self) {
        let value = serde_json::to_value(self.chats.lock().unwrap().clone()).unwrap();
        self.chat_watchers
            .lock()
            .unwrap()
            .retain(|tx| tx.unbounded_send(value.clone()).is_ok());
    }

    fn create(&self, chat_id: &str, space_id: &str) {
        let mut chats = self.chats.lock().unwrap();
        if chats.iter().any(|chat| chat.id == chat_id) {
            return;
        }
        chats.push(chat(chat_id, space_id, ChatCreationState::Draft));
        drop(chats);
        self.publish_chats();
    }
}

#[async_trait]
impl RpcService for FakeNewSessionRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::WATCH_CHATS => {
                let (tx, rx) = mpsc::unbounded();
                tx.unbounded_send(
                    serde_json::to_value(self.chats.lock().unwrap().clone()).unwrap(),
                )
                .unwrap();
                self.chat_watchers.lock().unwrap().push(tx);
                Ok(RpcReply::Stream(rx.boxed()))
            }
            methods::WATCH_SPACES => Ok(RpcReply::Stream(
                stream::once(futures::future::ready(
                    serde_json::to_value(&self.spaces).unwrap(),
                ))
                .chain(stream::pending())
                .boxed(),
            )),
            methods::WATCH_SESSIONS | methods::WATCH_DEVICES => empty_watch(),
            methods::AUTH_STATUS => watch(serde_json::json!({"state":"signedIn"})),
            methods::UPDATE_STATUS => watch(serde_json::json!({})),
            methods::WATCH_BOARD => watch(serde_json::json!([])),
            methods::LOCAL_DEVICE => Ok(RpcReply::Value(
                serde_json::json!({"deviceId":"device","version":"test"}),
            )),
            methods::EDGE_HEALTH => Ok(RpcReply::Value(serde_json::json!({}))),
            methods::LIST_HARNESSES => Ok(RpcReply::Value(serde_json::json!([]))),
            methods::LIST_SKILLS => Ok(RpcReply::Value(serde_json::json!([]))),
            methods::WATCH_DOC_MESSAGES | methods::WATCH_DOC_QUEUE => empty_watch(),
            methods::MUTATE => match params["op"].as_str().unwrap_or_default() {
                "createChat" => {
                    let id = params["chatId"].as_str().unwrap().to_string();
                    let space = params["spaceId"].as_str().unwrap().to_string();
                    self.create_calls
                        .lock()
                        .unwrap()
                        .push((id.clone(), space.clone()));
                    let reply = *self.reply.lock().unwrap();
                    match reply {
                        CreateReply::Success => {
                            self.create(&id, &space);
                            Ok(RpcReply::Value(serde_json::json!({})))
                        }
                        CreateReply::DefinitiveFailure => {
                            Err(RpcError::Failed("workspace is read-only".into()))
                        }
                        CreateReply::CommitThenLoseReply => {
                            self.create(&id, &space);
                            *self.reply.lock().unwrap() = CreateReply::Success;
                            Err(RpcError::Transport("reply lost".into()))
                        }
                    }
                }
                "confirmChatAbsent" => {
                    let id = params["chatId"].as_str().unwrap_or_default();
                    if self.chats.lock().unwrap().iter().any(|chat| chat.id == id) {
                        Err(RpcError::Failed("chat exists".into()))
                    } else {
                        Ok(RpcReply::Value(serde_json::json!({})))
                    }
                }
                "finalizeChatDraft" => {
                    self.finalize_calls.lock().unwrap().push(params.clone());
                    let id = params["chatId"].as_str().unwrap();
                    if let Some(chat) = self.chats.lock().unwrap().iter_mut().find(|c| c.id == id) {
                        chat.creation_state = ChatCreationState::Ready;
                        chat.cwd = params["cwd"].as_str().map(str::to_string);
                        chat.branch = params["branch"].as_str().map(str::to_string);
                        chat.config = serde_json::from_value(params["config"].clone()).ok();
                    }
                    self.publish_chats();
                    Ok(RpcReply::Value(serde_json::json!({})))
                }
                _ => Ok(RpcReply::Value(serde_json::json!({}))),
            },
            methods::QUEUE_COMMAND => Ok(RpcReply::Value(serde_json::json!({}))),
            _ => Ok(RpcReply::Value(serde_json::json!([]))),
        }
    }
}

fn watch(value: serde_json::Value) -> Result<RpcReply, RpcError> {
    Ok(RpcReply::Stream(
        stream::once(futures::future::ready(value))
            .chain(stream::pending())
            .boxed(),
    ))
}

fn empty_watch() -> Result<RpcReply, RpcError> {
    watch(serde_json::json!([]))
}

fn chat(id: &str, space: &str, creation_state: ChatCreationState) -> Chat {
    Chat {
        creation_state,
        id: id.into(),
        device_id: "device".into(),
        title: None,
        archived: false,
        cwd: Some(format!("/{space}")),
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

fn space(id: &str) -> Space {
    Space {
        id: id.into(),
        device_id: "device".into(),
        name: Some(id.into()),
        path: format!("/{id}"),
        git_detected: false,
        git_checked_at: None,
        checkout_id: None,
        branch: None,
        created_at: Utc::now(),
    }
}

struct Rig {
    cx: VisualTestContext,
    shell: gpui::Entity<Shell>,
    state: gpui::Entity<AppState>,
    rpc: Arc<FakeNewSessionRpc>,
    runtime: Arc<tokio::runtime::Runtime>,
    _dir: tempfile::TempDir,
}

fn rig(cx: &mut TestAppContext, chats: Vec<Chat>, spaces: Vec<Space>) -> Rig {
    let rpc = FakeNewSessionRpc::new(chats.clone(), spaces.clone());
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        cx.set_global(Theme::dark());
        crate::composer::init(cx);
        crate::terminal::panel::init(cx);
        crate::app_menus::init(cx);
    });
    let (state, runtime) = cx.update(|cx| {
        let state = cx.new(|_| AppState::new());
        let (handle, runtime) = crate::state::EngineHandle::for_test_service(rpc.clone());
        state.update(cx, |state, cx| {
            state.chats = chats;
            state.spaces = spaces;
            state.auth = Some(comet_proto::AuthState::SignedIn {
                user: comet_proto::UserProfile {
                    id: "user".into(),
                    email: "user@example.com".into(),
                    name: Some("User".into()),
                },
                org_id: Some("org".into()),
            });
            state.attach_test_engine(handle, cx);
        });
        (state, runtime)
    });
    let boot = EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: 0,
        edge_url: String::new(),
        edge_token: None,
        org_id: None,
        workos_client_id: None,
        default_harness: comet_proto::HarnessId::Mock,
    };
    let shell_slot = Arc::new(Mutex::new(None));
    let slot = shell_slot.clone();
    let state_for_window = state.clone();
    let window = cx.add_window(move |_, cx| {
        let shell = Shell::new(state_for_window, boot, cx);
        *slot.lock().unwrap() = Some(cx.entity());
        shell
    });
    let shell = shell_slot.lock().unwrap().take().unwrap();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.cx.refresh().unwrap();
    drain(&visual, &runtime);
    visual.update(|window, cx| {
        shell
            .read(cx)
            .composer
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    });
    Rig {
        cx: visual,
        shell,
        state,
        rpc,
        runtime,
        _dir: dir,
    }
}

impl Rig {
    fn selection(&self) -> (Option<String>, Option<String>) {
        self.cx.read(|cx| {
            let state = self.state.read(cx);
            (state.selected_chat.clone(), state.selected_space.clone())
        })
    }
}

fn drain(cx: &VisualTestContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..128 {
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
    }
}

#[gpui::test]
fn cmd_t_creates_one_sibling_and_focuses_composer(cx: &mut TestAppContext) {
    let mut rig = rig(
        cx,
        vec![chat("existing", "alpha", ChatCreationState::Ready)],
        vec![space("alpha")],
    );
    rig.state.update(&mut rig.cx, |state, cx| {
        state.select_chat(Some("existing".into()), cx)
    });
    rig.cx.cx.refresh().unwrap();
    drain(&rig.cx, &rig.runtime);
    rig.cx.update(|window, cx| {
        rig.shell
            .read(cx)
            .composer
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    });
    rig.cx
        .simulate_keystrokes(&commands::NEW_SESSION.effective_combo(&UiSettings::default().keymap));
    drain(&rig.cx, &rig.runtime);
    let calls = rig.rpc.create_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "alpha");
    drop(calls);
    rig.rpc.publish_chats();
    drain(&rig.cx, &rig.runtime);
    assert_eq!(rig.selection().1.as_deref(), Some("alpha"));
    assert!(
        rig.selection()
            .0
            .as_deref()
            .is_some_and(|id| id != "existing")
    );
    assert!(rig.cx.update(|window, cx| {
        rig.shell
            .read(cx)
            .composer
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
    }));

    rig.cx.cx.refresh().unwrap();
    drain(&rig.cx, &rig.runtime);
    let held = KeyDownEvent {
        keystroke: Keystroke::parse(
            &commands::NEW_SESSION.effective_combo(&UiSettings::default().keymap),
        )
        .unwrap(),
        is_held: true,
        prefer_character_input: false,
    };
    rig.cx.simulate_event(held);
    drain(&rig.cx, &rig.runtime);
    assert_eq!(rig.rpc.create_calls.lock().unwrap().len(), 1);
}

#[gpui::test]
fn scoped_screen_and_keyboard_chooser_keep_explicit_workspace(cx: &mut TestAppContext) {
    let mut scoped = rig(cx, vec![], vec![space("alpha"), space("beta")]);
    scoped.state.update(&mut scoped.cx, |state, cx| {
        state.selected_space = Some("beta".into());
        cx.notify();
    });
    scoped.shell.update(&mut scoped.cx, |shell, cx| {
        shell.board_open = true;
        shell.new_session(None, cx);
    });
    drain(&scoped.cx, &scoped.runtime);
    assert_eq!(scoped.rpc.create_calls.lock().unwrap()[0].1, "beta");

    let mut chooser = rig(cx, vec![], vec![space("alpha"), space("beta")]);
    chooser.state.update(&mut chooser.cx, |state, cx| {
        state.selected_space = None;
        state.selected_chat = None;
        cx.notify();
    });
    chooser
        .shell
        .update(&mut chooser.cx, |shell, cx| shell.new_session(None, cx));
    drain(&chooser.cx, &chooser.runtime);
    assert_eq!(
        chooser
            .shell
            .read_with(&chooser.cx, |shell, _| shell.new_session_chooser),
        Some(0)
    );
    chooser.shell.update(&mut chooser.cx, |shell, cx| {
        assert!(shell.handle_new_session_chooser_key("down", cx));
        assert!(shell.handle_new_session_chooser_key("enter", cx));
    });
    drain(&chooser.cx, &chooser.runtime);
    assert_eq!(chooser.rpc.create_calls.lock().unwrap()[0].1, "beta");
}

#[gpui::test]
fn failures_do_not_orphan_or_duplicate_and_menu_uses_same_action(cx: &mut TestAppContext) {
    let mut rejected = rig(cx, vec![], vec![space("alpha")]);
    rejected.rpc.set_reply(CreateReply::DefinitiveFailure);
    rejected.state.update(&mut rejected.cx, |state, cx| {
        state.selected_space = Some("alpha".into());
        cx.notify();
    });
    rejected.shell.update(&mut rejected.cx, |shell, cx| {
        shell.board_open = true;
        shell.new_session(None, cx);
    });
    drain(&rejected.cx, &rejected.runtime);
    assert!(rejected.rpc.chats.lock().unwrap().is_empty());
    assert!(rejected.selection().0.is_none());

    let mut ambiguous = rig(cx, vec![], vec![space("alpha")]);
    ambiguous.rpc.set_reply(CreateReply::CommitThenLoseReply);
    ambiguous.state.update(&mut ambiguous.cx, |state, cx| {
        state.selected_space = Some("alpha".into());
        cx.notify();
    });
    ambiguous.shell.update(&mut ambiguous.cx, |shell, cx| {
        shell.board_open = true;
        shell.new_session(None, cx);
    });
    drain(&ambiguous.cx, &ambiguous.runtime);
    assert!(ambiguous.shell.read_with(&ambiguous.cx, |shell, _| {
        shell.new_session_task.is_none() && shell.new_session_intent.is_some()
    }));
    ambiguous.shell.update(&mut ambiguous.cx, |shell, cx| {
        shell.last_new_session_action = None;
        shell.new_session(None, cx)
    });
    drain(&ambiguous.cx, &ambiguous.runtime);
    assert_eq!(ambiguous.rpc.chats.lock().unwrap().len(), 1);
    let calls = ambiguous.rpc.create_calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, calls[1].0);
    drop(calls);

    let MenuItem::Action { action, .. } = commands::NEW_SESSION.menu_item() else {
        panic!("New Session menu entry must dispatch an action");
    };
    assert_eq!(action.name(), commands::NEW_SESSION.action().name());
}

#[gpui::test]
fn first_send_finalizes_the_draft_with_its_checkout_and_config(cx: &mut TestAppContext) {
    let mut draft = chat("draft", "alpha", ChatCreationState::Draft);
    draft.cwd = Some("/alpha".into());
    draft.config = Some(comet_proto::ChatConfig {
        harness: comet_proto::HarnessId::Mock,
        model: Some("mock-model".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
        account: None,
        push_repo: None,
        push_contract: None,
        git_author: None,
        turn_limits: comet_proto::TurnLimits::default(),
        mcp_servers: Vec::new(),
    });
    let expected = draft.config.clone().unwrap();
    let mut rig = rig(cx, vec![draft], vec![space("alpha")]);
    rig.state.update(&mut rig.cx, |state, cx| {
        state.select_chat(Some("draft".into()), cx)
    });
    rig.shell
        .read_with(&rig.cx, |shell, _| shell.composer.clone())
        .update(&mut rig.cx, |composer, cx| {
            composer.send_for_interaction_test("hello", cx)
        });
    drain(&rig.cx, &rig.runtime);

    let calls = rig.rpc.finalize_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["op"], "finalizeChatDraft");
    assert_eq!(calls[0]["cwd"], "/alpha");
    assert_eq!(
        serde_json::from_value::<comet_proto::ChatConfig>(calls[0]["config"].clone()).unwrap(),
        expected
    );
    drop(calls);
    assert_eq!(
        rig.rpc.chats.lock().unwrap()[0].creation_state,
        ChatCreationState::Ready
    );
}
