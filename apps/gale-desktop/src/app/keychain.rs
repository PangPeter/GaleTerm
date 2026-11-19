//! Vault → Keychain: credentials remembered in the system keychain.

use gale_core::vault::{CredentialKey, CredentialKind};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    label::Label,
    ActiveTheme, Icon, IconName, Sizable as _,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::GaleApp;
use crate::i18n;
use crate::remote_session::runtime;

impl GaleApp {
    pub(super) fn reload_credentials(&mut self) {
        self.credentials = self.vault.as_ref().map(|v| v.list()).unwrap_or_default();
    }

    /// Remove `key` from the keychain (off the UI thread: the OS may ask).
    fn forget_credential(&mut self, key: CredentialKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let task = runtime().spawn_blocking(move || vault.delete(&key));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => this.notify_user(SharedString::from(err.to_string()), cx),
                    Err(err) => this.notify_user(SharedString::from(err.to_string()), cx),
                }
                this.reload_credentials();
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_keychain(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let header = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                Label::new(i18n::tr("app.keychain"))
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new(if self.vault.is_some() {
                    i18n::tr("app.keychain_hint")
                } else {
                    i18n::tr("app.keychain_unavailable")
                })
                .text_sm()
                .text_color(theme.muted_foreground),
            );

        let rows = self
            .credentials
            .iter()
            .map(|saved| {
                let key = saved.key.clone();
                let (icon, kind) = match key.kind() {
                    CredentialKind::Password => (IconName::User, i18n::tr("app.credential_password")),
                    CredentialKind::Passphrase => {
                        (IconName::FileText, i18n::tr("app.credential_passphrase"))
                    }
                };
                let saved_at = saved
                    .saved_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string();
                div()
                    .h(px(56.))
                    .px_4()
                    .rounded(px(10.))
                    .bg(theme.secondary)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(Icon::new(icon).text_color(theme.primary))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(key.target().to_string()),
                            )
                            .child(
                                Label::new(format!(
                                    "{} · {kind} · {saved_at}",
                                    key.scheme().to_uppercase()
                                ))
                                .text_xs()
                                .text_color(theme.muted_foreground),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("forget-{}", key.as_str())))
                            .ghost()
                            .small()
                            .icon(IconName::Delete)
                            .label(i18n::tr("app.forget"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.forget_credential(key.clone(), window, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .id("keychain-page")
            .size_full()
            .overflow_y_scroll()
            .px_6()
            .py_5()
            .flex()
            .flex_col()
            .gap_5()
            .child(header)
            .when(rows.is_empty(), |s| {
                s.child(self.empty_state(
                    IconName::User,
                    i18n::tr("app.keychain_empty"),
                    i18n::tr("app.keychain_empty_hint"),
                    None,
                    cx,
                ))
            })
            .child(div().max_w(px(720.)).flex().flex_col().gap_2().children(rows))
    }
}
