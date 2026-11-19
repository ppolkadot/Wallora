use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Background, Border, Element, Length};
use wallora_core::domain::SecretString;
use wallora_core::services::SessionManager;

use crate::message::Message;
use crate::theme::{palette, surface_color};

pub struct AccountViewState<'a> {
    pub session: &'a SessionManager,
    pub username_input: &'a str,
    pub api_key_input: &'a SecretString,
    pub sign_in_busy: bool,
    pub refresh_busy: bool,
}

pub fn view<'a>(state: AccountViewState<'a>) -> Element<'a, Message> {
    let mut content = column![
        text("Account").size(28).color(palette::TEXT_PRIMARY),
        text("Manage your Wallhaven API key session. Keys are stored only in Windows Credential Manager.")
            .size(14)
            .color(palette::TEXT_SECONDARY),
    ]
    .spacing(14)
    .width(Length::Fill);

    // Active session card.
    if let Some(account) = state.session.active_account() {
        let verification_color = if account.verification.is_verified() {
            palette::SUCCESS
        } else {
            palette::WARNING
        };

        let mut actions = row![].spacing(10).align_y(Alignment::Center);
        let refresh_btn = button(
            text(if state.refresh_busy {
                "Refreshing..."
            } else {
                "Refresh & verify"
            })
            .size(13),
        )
        .padding([8, 14]);
        let refresh_btn = if state.refresh_busy {
            refresh_btn
        } else {
            refresh_btn.on_press(Message::RefreshAccount { reverify: true })
        };
        actions = actions.push(refresh_btn);
        actions = actions.push(
            button(text("Switch to Guest").size(13))
                .padding([8, 14])
                .on_press(Message::SwitchToGuest),
        );
        actions = actions.push(
            button(text("Sign out").size(13))
                .padding([8, 14])
                .on_press(Message::SignOut),
        );

        let mut card = column![
            text(format!("Signed in as {}", account.username))
                .size(18)
                .color(palette::TEXT_PRIMARY),
            text(format!(
                "Username verification: {}",
                account.verification.label()
            ))
            .size(14)
            .color(verification_color),
            text(account.verification.explanation())
                .size(13)
                .color(palette::TEXT_SECONDARY),
            text(format!(
                "Session generation: {}  |  Account ID: {}",
                state.session.scope().generation,
                account.id.as_str()
            ))
            .size(12)
            .color(palette::MUTED),
            actions,
        ]
        .spacing(8);

        // Authenticated own collections list (names, counts, visibility only).
        card = card.push(
            text("Own collections (list only)")
                .size(16)
                .color(palette::TEXT_PRIMARY),
        );
        match state.session.own_collections() {
            Some([]) => {
                card = card.push(
                    text("No collections returned for this account.")
                        .size(13)
                        .color(palette::TEXT_SECONDARY),
                );
            }
            Some(list) => {
                for col in list {
                    let visibility = if col.public { "Public" } else { "Private" };
                    card = card.push(
                        text(format!(
                            "• {} — {} items, {} views ({visibility})",
                            col.label, col.count, col.views
                        ))
                        .size(13)
                        .color(palette::TEXT_SECONDARY),
                    );
                }
            }
            None => {
                card = card.push(
                    text("Own collection list not loaded yet.")
                        .size(13)
                        .color(palette::MUTED),
                );
            }
        }
        if !account.verification.can_access_own_collection_items() {
            card = card.push(
                text("Own collection items and Favorites mapping require a verified username (Task 6a).")
                    .size(12)
                    .color(palette::MUTED),
            );
        }

        content = content.push(container(card).padding(16).width(Length::Fill).style(|_| {
            container::Style {
                background: Some(Background::Color(surface_color())),
                border: Border {
                    radius: 8.0.into(),
                    width: 1.0,
                    color: palette::MUTED,
                },
                ..Default::default()
            }
        }));
    } else {
        content = content.push(
            container(
                column![
                    text("Browsing as Guest")
                        .size(18)
                        .color(palette::TEXT_PRIMARY),
                    text("Guest sessions never send an API key. Sign in below with your Wallhaven username and API key.")
                        .size(13)
                        .color(palette::TEXT_SECONDARY),
                ]
                .spacing(6),
            )
            .padding(16)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(surface_color())),
                border: Border {
                    radius: 8.0.into(),
                    width: 1.0,
                    color: palette::MUTED,
                },
                ..Default::default()
            }),
        );
    }

    // Saved accounts switcher (when other accounts exist).
    let active_id = state.session.active_account().map(|a| &a.id);
    let other_accounts: Vec<_> = state
        .session
        .accounts()
        .iter()
        .filter(|a| Some(&a.id) != active_id)
        .collect();
    if !other_accounts.is_empty() {
        let mut switcher =
            column![text("Saved accounts").size(16).color(palette::TEXT_PRIMARY)].spacing(8);
        for acct in other_accounts {
            switcher = switcher.push(
                row![
                    text(format!("{} ({})", acct.username, acct.verification.label()))
                        .size(13)
                        .color(palette::TEXT_SECONDARY),
                    button(text("Switch").size(13))
                        .padding([6, 12])
                        .on_press(Message::SwitchAccount(acct.id.clone())),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        content = content.push(
            container(switcher)
                .padding(16)
                .width(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Background::Color(surface_color())),
                    border: Border {
                        radius: 8.0.into(),
                        width: 1.0,
                        color: palette::MUTED,
                    },
                    ..Default::default()
                }),
        );
    }

    if !state.session.can_sign_in() {
        return scrollable(content).height(Length::Fill).into();
    }

    // Sign-in form.
    let sign_in_btn = button(
        text(if state.sign_in_busy {
            "Verifying..."
        } else {
            "Sign in"
        })
        .size(14),
    )
    .padding([8, 18]);
    let sign_in_btn = if state.sign_in_busy {
        sign_in_btn
    } else {
        sign_in_btn.on_press(Message::SubmitSignIn)
    };

    let form = container(
        column![
            text("Sign in with Wallhaven API key")
                .size(16)
                .color(palette::TEXT_PRIMARY),
            text("Username").size(13).color(palette::TEXT_SECONDARY),
            text_input("Wallhaven username", state.username_input)
                .on_input(Message::UsernameChanged)
                .padding(8),
            text("API key").size(13).color(palette::TEXT_SECONDARY),
            text_input("Paste your API key", state.api_key_input.expose_secret())
                .secure(true)
                .on_input(Message::ApiKeyChanged)
                .on_submit(Message::SubmitSignIn)
                .padding(8),
            sign_in_btn,
        ]
        .spacing(8),
    )
    .padding(16)
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(surface_color())),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: palette::MUTED,
        },
        ..Default::default()
    });

    content = content.push(form);

    scrollable(content).height(Length::Fill).into()
}
