//! The fork menu, and the strip that says a chat is a fork (gh#425).
//!
//! Two destinations, and the whole design problem is that they must never read
//! as one choice with a checkbox. A **shared checkout** fork is another
//! read-only conversation over the same files — a reviewer that watches the
//! original agent's edits appear underneath it. An **isolated checkout** fork
//! owns a new worktree and a new branch, and nothing it does can reach the
//! source's files. So the menu gives each one a row, a sentence of its own, and
//! never a default: the dialog opens with neither selected and the confirm
//! button stays inert until one is.
//!
//! The other honesty problem is memory. Why a fork carries a copy of the
//! visible transcript is decided by facts the
//! engine owns, so this module never decides it: it *predicts* with
//! [`comet_proto::decide_context`], the same function the host runs, and then
//! replaces the prediction with what actually happened as soon as the fork
//! lands. Predicting with a second implementation would be how the two come to
//! disagree, and the disagreement would be a promise about memory that the fork
//! breaks.

use super::*;
use comet_engine::registry::HarnessDescriptor;
use comet_proto::view::fork as fork_view;
use comet_proto::{
    ForkCheckout, ForkContext, ForkPoint, ForkRequest, ForkResult, HarnessId, Model,
};

/// Open fork menu.
pub(super) struct ForkDialog {
    /// The chat being forked, and the message it is being forked at.
    pub(super) chat_id: String,
    pub(super) message_id: String,
    /// Chosen destination — `None` until the person picks one. There is no
    /// default on purpose: the two are not variants of one action.
    pub(super) checkout: Option<ForkCheckout>,
    /// Harness override; `None` keeps the source's.
    pub(super) harness: Option<HarnessId>,
    /// Model override; `None` keeps the harness's own default.
    pub(super) model: Option<String>,
    pub(super) harnesses: Loadable<Vec<HarnessDescriptor>>,
    pub(super) models: Loadable<Vec<Model>>,
    pub(super) submitting: bool,
    pub(super) error: Option<SharedString>,
    /// What the fork turned out to be, once it has landed — shown in place of
    /// the prediction, because this is the sentence that is actually true.
    pub(super) landed: Option<ForkResult>,
}

impl ForkDialog {
    /// The harness this fork will run on, given the source chat's.
    fn effective_harness(&self, source: Option<HarnessId>) -> Option<HarnessId> {
        self.harness.or(source)
    }
}

impl Shell {
    /// Open the fork menu on a message of the selected chat.
    pub(super) fn open_fork(&mut self, message_id: String, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        self.fork_dialog = Some(ForkDialog {
            chat_id,
            message_id,
            checkout: None,
            harness: None,
            model: None,
            harnesses: Loadable::Idle,
            models: Loadable::Idle,
            submitting: false,
            error: None,
            landed: None,
        });
        self.load_fork_harnesses(cx);
        cx.notify();
    }

    /// Which harnesses the fork's host device can run — asked of that device,
    /// because "available" is a fact about the box the fork will run on and not
    /// about the laptop the menu was opened from.
    fn load_fork_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.fork_dialog.as_mut() else {
            return;
        };
        if !matches!(dialog.harnesses, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.fork_target(cx);
        if let Some(dialog) = self.fork_dialog.as_mut() {
            dialog.harnesses = Loadable::Loading;
        }
        self.fork_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({});
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert("targetDeviceId".into(), serde_json::json!(target));
            }
            let result = engine.client().call(methods::LIST_HARNESSES, params).await;
            this.update(cx, |shell, cx| {
                if let Some(dialog) = shell.fork_dialog.as_mut() {
                    dialog.harnesses = match result {
                        Ok(value) => {
                            match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                                Ok(list) => Loadable::Ready(list),
                                Err(err) => Loadable::Error(err.to_string()),
                            }
                        }
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                shell.load_fork_models(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The models of whichever harness the fork will run on.
    fn load_fork_models(&mut self, cx: &mut Context<Self>) {
        let source = self.fork_source_harness(cx);
        let Some(dialog) = self.fork_dialog.as_ref() else {
            return;
        };
        let Some(harness) = dialog.effective_harness(source) else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.fork_target(cx);
        if let Some(dialog) = self.fork_dialog.as_mut() {
            dialog.models = Loadable::Loading;
        }
        self.fork_models_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({ "harness": harness });
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert("targetDeviceId".into(), serde_json::json!(target));
            }
            let result = engine.client().call(methods::LIST_MODELS, params).await;
            this.update(cx, |shell, cx| {
                if let Some(dialog) = shell.fork_dialog.as_mut() {
                    dialog.models = match result {
                        Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                            Ok(models) => Loadable::Ready(models),
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The device hosting the chat being forked, when it is not this one.
    fn fork_target(&self, cx: &App) -> Option<String> {
        let dialog = self.fork_dialog.as_ref()?;
        let state = self.state.read(cx);
        let host = state
            .chats
            .iter()
            .find(|c| c.id == dialog.chat_id)
            .map(|c| c.device_id.clone())?;
        (state.local_device_id.as_deref() != Some(host.as_str())).then_some(host)
    }

    fn fork_source_harness(&self, cx: &App) -> Option<HarnessId> {
        let dialog = self.fork_dialog.as_ref()?;
        self.state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == dialog.chat_id)
            .and_then(|c| c.config.as_ref())
            .map(|config| config.harness)
    }

    /// What the fork will carry, as far as this side can tell — the same
    /// decision the host will make, over the facts this side can see.
    fn fork_prediction(&self, cx: &App) -> Option<(ForkContext, Option<comet_proto::CopyReason>)> {
        let dialog = self.fork_dialog.as_ref()?;
        let checkout = dialog.checkout?;
        let state = self.state.read(cx);
        let chat = state.chats.iter().find(|c| c.id == dialog.chat_id)?;
        let at_tail = state
            .transcript
            .last()
            .is_some_and(|entry| entry.id == dialog.message_id);
        let source_harness = chat.config.as_ref().map(|config| config.harness);
        Some(comet_proto::decide_context(ForkPoint {
            at_tail,
            same_harness: dialog.harness.is_none() || dialog.harness == source_harness,
            same_checkout: checkout == ForkCheckout::Shared,
            session_id: chat.harness_session_id.as_deref(),
        }))
    }

    /// Send it. The reply replaces the prediction, and the new chat is
    /// selected — a fork you cannot find is a fork you did not make.
    fn submit_fork(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.fork_dialog.as_ref().and_then(|dialog| {
            (!dialog.submitting).then_some(())?;
            Some(ForkRequest {
                chat_id: dialog.chat_id.clone(),
                message_id: Some(dialog.message_id.clone()),
                checkout: dialog.checkout?,
                harness: dialog.harness,
                model: dialog.model.clone(),
                account: None,
                title: None,
                prompt: None,
            })
        }) else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(dialog) = self.fork_dialog.as_mut() {
                dialog.error = Some("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        let target = self.fork_target(cx);
        if let Some(dialog) = self.fork_dialog.as_mut() {
            dialog.submitting = true;
            dialog.error = None;
        }
        self.fork_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert("targetDeviceId".into(), serde_json::json!(target));
            }
            let result = engine.client().call(methods::FORK_CHAT, params).await;
            this.update(cx, |shell, cx| {
                let Some(dialog) = shell.fork_dialog.as_mut() else {
                    return;
                };
                dialog.submitting = false;
                match result.map_err(|err| err.to_string()).and_then(|value| {
                    serde_json::from_value::<ForkResult>(value).map_err(|e| e.to_string())
                }) {
                    Ok(fork) => {
                        let chat_id = fork.chat_id.clone();
                        dialog.landed = Some(fork);
                        shell
                            .state
                            .update(cx, |state, cx| state.select_chat(Some(chat_id), cx));
                    }
                    Err(err) => dialog.error = Some(err.into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The fork menu as a modal card.
    pub(super) fn render_fork_dialog(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let source_title = self
            .fork_dialog
            .as_ref()
            .and_then(|dialog| {
                self.state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == dialog.chat_id)
                    .and_then(|c| c.title.clone())
            })
            .unwrap_or_else(|| "this session".into());
        let prediction = self.fork_prediction(cx);
        let source_harness = self.fork_source_harness(cx);
        let dialog = self.fork_dialog.as_ref()?;

        // Landed: the card stops offering choices and states the outcome.
        if let Some(fork) = &dialog.landed {
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Forked"))
                .child(
                    div()
                        .mt(px(6.0))
                        .child(popover::dialog_body(&theme, fork.note())),
                )
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    match fork.branch.as_deref() {
                        Some(branch) if fork.lineage.checkout == ForkCheckout::Isolated => {
                            format!("Working in {} on {branch}.", fork.cwd)
                        }
                        _ => format!("Working in {}, beside the original.", fork.cwd),
                    },
                )))
                .child(
                    div().mt(px(16.0)).flex().flex_row().justify_end().child(
                        popover::btn_primary(&theme, "Open it")
                            .id("fork-done")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.fork_dialog = None;
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element();
            return Some(popover::modal("fork-dialog", viewport, card));
        }

        let chosen = dialog.checkout;
        let mut card = popover::dialog_card(&theme)
            .w(px(420.0))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.fork_dialog = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, "Fork this conversation"))
            .child(div().mt(px(6.0)).child(popover::dialog_body(
                &theme,
                format!(
                    "A new session that starts from this message of \u{201C}{}\u{201D}.",
                    transcript::single_line(&source_title)
                ),
            )));

        for option in [ForkCheckout::Shared, ForkCheckout::Isolated] {
            let active = chosen == Some(option);
            let key = match option {
                ForkCheckout::Shared => "fork-shared",
                ForkCheckout::Isolated => "fork-isolated",
            };
            card =
                card.child(
                    popover::menu_row(&theme, active, key)
                        .id(key)
                        .mt(px(10.0))
                        .flex_col()
                        .items_start()
                        .gap(px(2.0))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    icon(match option {
                                        ForkCheckout::Shared => icons::USERS_GROUP,
                                        ForkCheckout::Isolated => icons::FOLDER_WITH_FILES,
                                    })
                                    .size(px(14.0))
                                    .text_color(if active { theme.text } else { theme.text_muted }),
                                )
                                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(
                                    SharedString::from(fork_view::destination_title(option)),
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(option.explanation())),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(dialog) = this.fork_dialog.as_mut() {
                                dialog.checkout = Some(option);
                            }
                            cx.notify();
                        })),
                );
        }

        // What it will carry — the prediction, in the same words the result
        // will use. Only once a destination is chosen: before that there is no
        // answer, and inventing one would be the lie this whole feature is
        // about not telling.
        if let Some((context, reason)) = prediction {
            let mut line = context.explanation().to_string();
            if let Some(reason) = reason {
                line.push_str(" (");
                line.push_str(reason.label());
                line.push(')');
            }
            card = card.child(
                div()
                    .mt(px(12.0))
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(Theme::RADIUS_ROW))
                    .bg(theme.white_alpha(0.04))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(line)),
            );
        }

        // Harness / model override: a fork is how you ask a different agent, or
        // a stronger model, what it makes of the same work.
        if let Loadable::Ready(harnesses) = &dialog.harnesses
            && harnesses.len() > 1
        {
            let effective = dialog.effective_harness(source_harness);
            let mut row = div()
                .mt(px(12.0))
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from("Run with")),
                );
            for descriptor in harnesses {
                let id = descriptor.id;
                let active = effective == Some(id);
                row = row.child(
                    chip(
                        &theme,
                        &descriptor.name,
                        active,
                        format!("fork-harness-{id:?}"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(dialog) = this.fork_dialog.as_mut() {
                            dialog.harness = Some(id);
                            dialog.model = None;
                            dialog.models = Loadable::Idle;
                        }
                        this.load_fork_models(cx);
                        cx.notify();
                    })),
                );
            }
            card = card.child(row);
        }
        if let Loadable::Ready(models) = &dialog.models
            && !models.is_empty()
        {
            let mut row = div()
                .mt(px(8.0))
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from("Model")),
                );
            for model in models.iter().take(6) {
                let id = model.id.clone();
                let active = dialog.model.as_deref() == Some(id.as_str());
                row = row.child(
                    chip(&theme, &model.label, active, format!("fork-model-{id}")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            if let Some(dialog) = this.fork_dialog.as_mut() {
                                dialog.model = (!active).then(|| id.clone());
                            }
                            cx.notify();
                        }),
                    ),
                );
            }
            card = card.child(row);
        }

        if let Some(error) = &dialog.error {
            card = card.child(
                div()
                    .mt(px(12.0))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }

        let submitting = dialog.submitting;
        let ready = chosen.is_some() && !submitting;
        card = card.child(
            div()
                .mt(px(16.0))
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(
                    popover::btn_ghost(&theme, "Cancel", "fork-cancel")
                        .id("fork-cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.fork_dialog = None;
                            cx.notify();
                        })),
                )
                .child(
                    popover::btn_primary(&theme, if submitting { "Forking…" } else { "Fork" })
                        .id("fork-confirm")
                        .when(!ready, |el| el.opacity(0.4))
                        .on_click(cx.listener(|this, _, _, cx| this.submit_fork(cx))),
                ),
        );

        Some(popover::modal(
            "fork-dialog",
            viewport,
            card.into_any_element(),
        ))
    }

    /// The strip above the transcript of a chat that IS a fork: where it came
    /// from, which checkout it stands in, and what it was given. Clicking it
    /// opens the source.
    ///
    /// Always on screen rather than tucked into a menu, because it changes how
    /// everything below it should be read: in a shared checkout, the diff this
    /// session shows is not only this session's work.
    pub(super) fn render_lineage_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let state = self.state.read(cx);
        let chat = state
            .chats
            .iter()
            .find(|c| Some(c.id.as_str()) == state.selected_chat.as_deref())?;
        let lineage = chat.forked_from.clone()?;
        let source_title = state
            .chats
            .iter()
            .find(|c| c.id == lineage.source_chat_id)
            .and_then(|c| c.title.clone());
        let source_id = lineage.source_chat_id.clone();
        let source_exists = state.chats.iter().any(|c| c.id == lineage.source_chat_id);
        let summary = lineage.summary(source_title.as_deref());
        let shared = lineage.checkout == ForkCheckout::Shared;
        Some(
            div()
                .id("fork-lineage")
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .px(px(48.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(theme.border)
                // A shared-checkout fork is the one a reader can be misled by,
                // so it is the one that gets the warmer tint: the two must not
                // look the same at a glance either.
                .bg(if shared {
                    theme.white_alpha(0.05)
                } else {
                    theme.white_alpha(0.02)
                })
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(theme.text_muted)
                .when(source_exists, |el| {
                    el.cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.state.update(cx, |state, cx| {
                                state.select_chat(Some(source_id.clone()), cx)
                            });
                            cx.notify();
                        }))
                })
                .child(
                    icon(icons::GIT_BRANCH)
                        .size(px(13.0))
                        .text_color(theme.text_subtle),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(summary)),
                )
                .into_any_element(),
        )
    }
}

/// A small selectable pill — the harness/model overrides.
fn chip(
    theme: &Theme,
    label: &str,
    active: bool,
    key: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let key = key.into();
    div()
        .id(key)
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(Theme::RADIUS_CHIP))
        .border_1()
        .border_color(if active {
            theme.white_alpha(0.24)
        } else {
            theme.white_alpha(0.08)
        })
        .bg(if active {
            theme.white_alpha(0.10)
        } else {
            theme.white_alpha(0.03)
        })
        .text_size(px(Theme::TEXT_DENSE))
        .text_color(if active { theme.text } else { theme.text_muted })
        .cursor_pointer()
        .child(SharedString::from(label.to_string()))
}
