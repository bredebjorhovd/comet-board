//! Settings → Members (gh#76): the workspace roster, the invite-by-email form,
//! and the invitations nobody has redeemed yet. Adding a teammate used to mean
//! creating an `organization_membership` by hand in the WorkOS dashboard; the
//! edge routes behind `ListMembers`/`InviteMember` do it from here.
//!
//! Only an admin sees the invite form and the pending list — the edge decides
//! that (`canInvite`), the UI just believes it; a plain member who forces the
//! call still gets a 403 with the reason, surfaced in the error strip.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use std::time::Duration;

use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Wire mirrors + pure helpers
// ---------------------------------------------------------------------------

/// One member (tolerant local mirror of the engine's ListMembers reply —
/// `{members: [{id, userId, email?, role?}], canInvite}`).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberRow {
    #[serde(default)]
    pub user_id: String,
    /// Absent when the edge could not resolve the address — the row still
    /// renders, by id.
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

/// The roster plus whether this caller may change it.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    #[serde(default)]
    pub members: Vec<MemberRow>,
    #[serde(default)]
    pub can_invite: bool,
}

/// One outstanding invitation (mirror of ListInvites).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    /// WorkOS's hosted accept link — copyable, for a teammate who is not on
    /// this desktop (or never got the mail).
    #[serde(default)]
    pub accept_url: Option<String>,
}

pub fn parse_roster(value: &serde_json::Value) -> Roster {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Parse a ListInvites reply tolerantly (accepts a bare array too).
pub fn parse_invites(value: &serde_json::Value) -> Vec<InviteRow> {
    let list = value.get("invites").unwrap_or(value);
    serde_json::from_value(list.clone()).unwrap_or_default()
}

/// Local pre-check, deliberately the same loose shape the edge applies: it
/// keeps a typo from costing a round trip, it does not decide anything.
pub fn invite_email_valid(email: &str) -> bool {
    let trimmed = email.trim();
    let Some((local, domain)) = trimmed.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !trimmed.contains(char::is_whitespace)
        && !domain.contains('@')
}

/// Human role label. WorkOS role slugs are per-environment config, so anything
/// unrecognized is shown as-is rather than guessed at.
pub fn role_label(role: Option<&str>) -> String {
    match role.map(str::trim).filter(|r| !r.is_empty()) {
        None => "Member".to_string(),
        Some("admin") => "Admin".to_string(),
        Some("owner") => "Owner".to_string(),
        Some("member") => "Member".to_string(),
        Some(other) => other.to_string(),
    }
}

/// What a member row is called: their address, or the bare user id when the
/// edge could not resolve one.
pub fn member_label(member: &MemberRow) -> String {
    match member.email.as_deref().map(str::trim) {
        Some(email) if !email.is_empty() => email.to_string(),
        _ => member.user_id.clone(),
    }
}

/// Avatar letter for a row (uppercased first character; "?" for nothing).
pub fn initial(label: &str) -> String {
    label
        .trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// "Expires in 6d" / "Expires in 5h" / "Expired" — None when WorkOS sent no
/// expiry (or an unparseable one; a bad timestamp must not read as expired).
pub fn format_expiry(expires_at: Option<&str>, now: DateTime<Utc>) -> Option<String> {
    let at = DateTime::parse_from_rfc3339(expires_at?.trim())
        .ok()?
        .with_timezone(&Utc);
    let secs = at.signed_duration_since(now).num_seconds();
    Some(if secs <= 0 {
        "Expired".to_string()
    } else if secs < 3600 {
        format!("Expires in {}m", secs.div_euclid(60).max(1))
    } else if secs < 86_400 {
        format!("Expires in {}h", secs / 3600)
    } else {
        format!("Expires in {}d", secs / 86_400)
    })
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

pub struct MembersPage {
    state: Entity<AppState>,
    roster: Loadable<Roster>,
    invites: Loadable<Vec<InviteRow>>,
    email_input: Entity<ComposerInput>,
    inviting: bool,
    /// Invitation whose Revoke is in flight.
    busy_invite: Option<String>,
    /// Invitation whose link chip currently reads "Copied".
    copied: Option<String>,
    notice: Option<SharedString>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    invites_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    _events: Subscription,
}

impl MembersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let email_input = cx.new(|cx| ComposerInput::new("teammate@company.com", cx));
        let events = cx.subscribe(&email_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_invite(cx);
            }
        });
        let mut page = Self {
            state,
            roster: Loadable::Idle,
            invites: Loadable::Idle,
            email_input,
            inviting: false,
            busy_invite: None,
            copied: None,
            notice: None,
            error: None,
            load_task: None,
            invites_task: None,
            action_task: None,
            copy_task: None,
            _events: events,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.roster = Loadable::Error("Engine not connected".into());
            return;
        };
        self.roster = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_MEMBERS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.roster = match result {
                    Ok(value) => Loadable::Ready(parse_roster(&value)),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                // Pending invitations are an admin's business only; asking as a
                // plain member would just collect a 403.
                if page.roster.ready().is_some_and(|r| r.can_invite) {
                    page.load_invites(cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn load_invites(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.invites = Loadable::Loading;
        self.invites_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_INVITES, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.invites = match result {
                    Ok(value) => Loadable::Ready(parse_invites(&value)),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_invite(&mut self, cx: &mut Context<Self>) {
        if self.inviting {
            return;
        }
        let email = self.email_input.read(cx).text().trim().to_string();
        if !invite_email_valid(&email) {
            self.error = Some("Enter a valid email address".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.inviting = true;
        self.error = None;
        self.notice = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::INVITE_MEMBER, serde_json::json!({ "email": email }))
                .await;
            this.update(cx, |page, cx| {
                page.inviting = false;
                match result {
                    Ok(_) => {
                        page.email_input
                            .update(cx, |input, cx| input.set_text(String::new(), cx));
                        page.notice = Some(format!("Invitation sent to {email}").into());
                        page.load_invites(cx);
                    }
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn revoke(&mut self, invitation_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy_invite = Some(invitation_id.clone());
        self.error = None;
        self.notice = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REVOKE_INVITE,
                    serde_json::json!({ "invitationId": invitation_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy_invite = None;
                match result {
                    Ok(_) => page.load_invites(cx),
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn copy_link(&mut self, invitation_id: String, url: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        self.copied = Some(invitation_id);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The avatar tile: the row's initial in the same 36px frame the other
    /// settings pages give an icon.
    fn avatar(theme: &Theme, label: &str, muted: bool) -> gpui::Div {
        div()
            .flex_none()
            .size(px(36.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.white_alpha(0.03))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(if muted {
                theme.text_muted.opacity(0.7)
            } else {
                theme.text_muted
            })
            .child(SharedString::from(initial(label)))
    }

    fn render_invite_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inviting = self.inviting;
        let input = self.email_input.clone();
        crate::settings::widgets::section_card(&theme)
            .child(
                div()
                    .px(px(20.0))
                    .py(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from("Invite a teammate")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(36.0))
                                    .flex()
                                    .items_center()
                                    .px(px(12.0))
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.bg)
                                    .text_size(px(13.0))
                                    .child(input),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if inviting { "Sending…" } else { "Send invite" },
                                )
                                .id("invite-send")
                                .h(px(36.0))
                                .flex()
                                .items_center()
                                .when(inviting, |el| el.opacity(0.5))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_invite(cx)),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from(
                                "They get an email with a join link. Anyone in the workspace can \
                                 open its shared sessions and devices.",
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_invites(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let busy = self.busy_invite.clone();
        let copied = self.copied.clone();
        let body: AnyElement = match &self.invites {
            Loadable::Idle | Loadable::Loading => div()
                .px(px(20.0))
                .py(px(14.0))
                .child(popover::skeleton_rows("invites-skeleton", &theme, 2))
                .into_any_element(),
            Loadable::Error(message) => popover::error_row(&theme, message)
                .px(px(20.0))
                .py(px(14.0))
                .into_any_element(),
            Loadable::Ready(rows) if rows.is_empty() => div()
                .px(px(20.0))
                .py(px(24.0))
                .text_center()
                .text_size(px(13.0))
                .text_color(theme.text_muted.opacity(0.6))
                .child(SharedString::from("No pending invitations"))
                .into_any_element(),
            Loadable::Ready(rows) => div()
                .flex()
                .flex_col()
                .children(rows.iter().enumerate().map(|(ix, invite)| {
                    let pending = busy.as_deref() == Some(invite.id.as_str());
                    let link_copied = copied.as_deref() == Some(invite.id.as_str());
                    let revoke_id = invite.id.clone();
                    let mut meta: Vec<AnyElement> = vec![
                        div()
                            .child(SharedString::from("Invited"))
                            .into_any_element(),
                    ];
                    if let Some(expiry) = format_expiry(invite.expires_at.as_deref(), now) {
                        meta.push(div().child(SharedString::from(expiry)).into_any_element());
                    }
                    if let Some(url) = invite.accept_url.clone() {
                        let copy_id = invite.id.clone();
                        meta.push(
                            div()
                                .id(("invite-link", ix))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text_muted))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_link(copy_id.clone(), url.clone(), cx);
                                }))
                                .child(SharedString::from(if link_copied {
                                    "Link copied"
                                } else {
                                    "Copy join link"
                                }))
                                .into_any_element(),
                        );
                    }
                    widgets::card_row(&theme, ix == 0)
                        .child(Self::avatar(&theme, &invite.email, true))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(&theme, invite.email.clone()))
                                .child(widgets::meta_line(&theme, meta)),
                        )
                        .child(widgets::badge(&theme, "Pending"))
                        .child(
                            widgets::ghost_action(&theme)
                                .id(("invite-revoke", ix))
                                .opacity(0.7)
                                .when(pending, |el| el.opacity(0.4))
                                .hover(widgets::ghost_hover(&theme))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.revoke(revoke_id.clone(), cx);
                                }))
                                .child(SharedString::from(if pending {
                                    "Revoking…"
                                } else {
                                    "Revoke"
                                })),
                        )
                        .into_any_element()
                }))
                .into_any_element(),
        };
        widgets::section_card(&theme).child(body).into_any_element()
    }
}

impl Render for MembersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let me = self
            .state
            .read(cx)
            .auth_user()
            .map(|user| user.email.to_lowercase());
        let can_invite = self.roster.ready().is_some_and(|r| r.can_invite);
        let count = self.roster.ready().map(|r| r.members.len()).unwrap_or(0);

        let roster: AnyElement = match &self.roster {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .child(
                    div()
                        .px(px(20.0))
                        .py(px(14.0))
                        .child(popover::skeleton_rows("members-skeleton", &theme, 3)),
                )
                .into_any_element(),
            Loadable::Error(message) => widgets::section_card(&theme)
                .child(
                    div()
                        .px(px(20.0))
                        .py(px(14.0))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(popover::error_row(&theme, message))
                        .child(
                            widgets::ghost_action(&theme)
                                .id("members-retry")
                                .hover(widgets::ghost_hover(&theme))
                                .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                                .child(SharedString::from("Retry")),
                        ),
                )
                .into_any_element(),
            Loadable::Ready(view) if view.members.is_empty() => widgets::section_card(&theme)
                .child(
                    div()
                        .px(px(20.0))
                        .py(px(40.0))
                        .text_center()
                        .text_size(px(14.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from("No members yet")),
                )
                .into_any_element(),
            Loadable::Ready(view) => widgets::section_card(&theme)
                .children(view.members.iter().enumerate().map(|(ix, member)| {
                    let label = member_label(member);
                    let is_me = me
                        .as_deref()
                        .is_some_and(|mine| label.to_lowercase() == mine);
                    widgets::card_row(&theme, ix == 0)
                        .child(Self::avatar(&theme, &label, false))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(&theme, label))
                                .child(widgets::meta_line(
                                    &theme,
                                    vec![
                                        div()
                                            .child(SharedString::from(role_label(
                                                member.role.as_deref(),
                                            )))
                                            .into_any_element(),
                                    ],
                                )),
                        )
                        .when(is_me, |el| el.child(widgets::badge(&theme, "You")))
                        .into_any_element()
                }))
                .into_any_element(),
        };

        div()
            .id("members-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(
                        &theme,
                        "Members",
                        (count > 0).then_some(count),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Who belongs to this workspace, and who has been invited.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(message)
                                .id("members-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .when_some(self.notice.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .px(px(16.0))
                                .py(px(10.0))
                                .rounded(px(12.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.white_alpha(0.03))
                                .text_size(px(12.5))
                                .text_color(theme.text_muted)
                                .child(message),
                        )
                    })
                    .when(can_invite, |el| el.child(self.render_invite_form(cx)))
                    .child(roster)
                    .when(can_invite, |el| {
                        el.child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from("Pending invitations")),
                        )
                        .child(self.render_invites(cx))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn roster_and_invites_parse_tolerantly() {
        let roster = parse_roster(&serde_json::json!({
            "members": [
                { "id": "om_1", "userId": "user_1", "email": "ada@example.com", "role": "admin" },
                { "id": "om_2", "userId": "user_2" }
            ],
            "canInvite": true
        }));
        assert!(roster.can_invite);
        assert_eq!(roster.members.len(), 2);
        assert_eq!(roster.members[1].email, None, "email is optional");
        // A shape we don't recognize must not panic or half-fill the page.
        assert_eq!(parse_roster(&serde_json::json!("nope")), Roster::default());
        assert!(!parse_roster(&serde_json::json!({})).can_invite);

        let invites = parse_invites(&serde_json::json!({
            "invites": [{ "id": "inv_1", "email": "new@example.com" }]
        }));
        assert_eq!(invites.len(), 1);
        // Bare array (and junk) tolerated, like `parse_orgs`.
        assert_eq!(
            parse_invites(&serde_json::json!([{ "id": "inv_2", "email": "x@y.z" }])).len(),
            1
        );
        assert!(parse_invites(&serde_json::json!("nope")).is_empty());
    }

    #[test]
    fn email_pre_check_matches_the_edge_shape() {
        assert!(invite_email_valid("ada@example.com"));
        assert!(invite_email_valid(" ada+comet@sub.example.co.uk "));
        assert!(!invite_email_valid("ada@example"));
        assert!(!invite_email_valid("@example.com"));
        assert!(!invite_email_valid("ada example@x.com"));
        assert!(!invite_email_valid("ada@@x.com"));
        assert!(!invite_email_valid(""));
    }

    #[test]
    fn labels() {
        assert_eq!(role_label(None), "Member");
        assert_eq!(role_label(Some("admin")), "Admin");
        assert_eq!(role_label(Some("owner")), "Owner");
        // Per-environment slugs are shown, never guessed at.
        assert_eq!(role_label(Some("billing-admin")), "billing-admin");

        let with_email = MemberRow {
            user_id: "user_1".into(),
            email: Some("ada@example.com".into()),
            role: None,
        };
        assert_eq!(member_label(&with_email), "ada@example.com");
        let without = MemberRow {
            user_id: "user_2".into(),
            email: None,
            role: None,
        };
        assert_eq!(member_label(&without), "user_2");
        assert_eq!(initial("ada@example.com"), "A");
        assert_eq!(initial(""), "?");
    }

    #[test]
    fn expiry_line() {
        let now = Utc::now();
        let at = |d: TimeDelta| (now + d).to_rfc3339();
        assert_eq!(
            format_expiry(Some(&at(TimeDelta::days(6))), now).as_deref(),
            Some("Expires in 6d")
        );
        assert_eq!(
            format_expiry(Some(&at(TimeDelta::hours(5))), now).as_deref(),
            Some("Expires in 5h")
        );
        assert_eq!(
            format_expiry(Some(&at(-TimeDelta::hours(1))), now).as_deref(),
            Some("Expired")
        );
        assert_eq!(format_expiry(None, now), None);
        // An unparseable timestamp says nothing rather than "Expired" — the
        // invitation is probably fine.
        assert_eq!(format_expiry(Some("soon"), now), None);
    }
}
