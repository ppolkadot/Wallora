use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Background, Border, Element, Length};
use wallora_core::services::SessionManager;
use wallora_core::storage::AppPaths;

use crate::message::{Message, ProxyModeChoice};
use crate::theme::{palette, surface_color};

pub struct SettingsViewState<'a> {
    pub paths: &'a AppPaths,
    pub session: &'a SessionManager,
    pub proxy_mode: ProxyModeChoice,
    pub proxy_url_input: &'a str,
    pub proxy_feedback: Option<&'a (bool, String)>,
}

pub fn view<'a>(state: SettingsViewState<'a>) -> Element<'a, Message> {
    let defaults = state.session.browse_defaults();
    let config_path = state.paths.config_dir.display().to_string();
    let cache_path = state.paths.cache_dir.display().to_string();
    let data_path = state.paths.data_dir.display().to_string();
    let logs_path = state.paths.logs_dir.display().to_string();

    let source_label = match (
        state.session.active_account(),
        state.session.user_settings(),
    ) {
        (Some(acct), Some(_)) => format!("Applied from upstream account ({})", acct.username),
        (Some(acct), None) => format!(
            "Signed in as {} (upstream settings not yet loaded; using guest defaults)",
            acct.username
        ),
        _ => {
            "Guest defaults (sign in under Account to load upstream Wallhaven defaults)".to_string()
        }
    };

    let purity_summary = format!(
        "SFW={} Sketchy={} NSFW={}",
        defaults.purity.sfw, defaults.purity.sketchy, defaults.purity.nsfw
    );
    let category_summary = format!(
        "General={} Anime={} People={}",
        defaults.categories.general, defaults.categories.anime, defaults.categories.people
    );
    let resolutions_summary = if defaults.resolutions.is_empty() {
        "Any".to_string()
    } else {
        defaults
            .resolutions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ratios_summary = if defaults.aspect_ratios.is_empty() {
        "Any".to_string()
    } else {
        defaults
            .aspect_ratios
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut upstream_section = column![
        text("Upstream Wallhaven defaults (read-only)")
            .size(18)
            .color(palette::TEXT_PRIMARY),
        text(source_label).size(13).color(palette::TEXT_SECONDARY),
        text(format!("Categories: {category_summary}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text(format!("Purity: {purity_summary}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text(format!(
            "Page size: {}  |  Thumb size: {}  |  Toplist range: {}",
            defaults.per_page,
            defaults.thumb_size,
            defaults.toplist_range.as_param()
        ))
        .size(13)
        .color(palette::TEXT_SECONDARY),
        text(format!("Resolutions: {resolutions_summary}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text(format!("Aspect ratios: {ratios_summary}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
    ]
    .spacing(6);

    if let Some(settings) = state.session.user_settings() {
        upstream_section = upstream_section.push(
            text(format!(
                "Blacklists: {} tags, {} users",
                settings.tag_blacklist.len(),
                settings.user_blacklist.len()
            ))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        );
    }

    upstream_section = upstream_section.push(
        row![
            button(text("Open on wallhaven.cc").size(13))
                .padding([8, 14])
                .on_press(Message::OpenUpstreamSettings),
            text("Upstream account settings are read-only in the public API; edit them on wallhaven.cc.")
                .size(12)
                .color(palette::MUTED),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    );

    // Local network proxy preferences (kept separate from upstream settings).
    let mode_row = [
        ProxyModeChoice::System,
        ProxyModeChoice::Direct,
        ProxyModeChoice::Manual,
    ]
    .into_iter()
    .fold(row![].spacing(8), |r, mode| {
        let active = state.proxy_mode == mode;
        let btn = button(text(mode.label()).size(13))
            .padding([6, 12])
            .on_press(Message::ProxyModeSelected(mode))
            .style(move |theme, status| {
                let base = if active {
                    button::primary(theme, status)
                } else {
                    button::secondary(theme, status)
                };
                button::Style {
                    background: Some(Background::Color(if active {
                        palette::ACCENT
                    } else {
                        surface_color()
                    })),
                    text_color: if active {
                        palette::CANVAS
                    } else {
                        palette::TEXT_PRIMARY
                    },
                    border: Border {
                        radius: 6.0.into(),
                        width: 1.0,
                        color: if active {
                            palette::ACCENT
                        } else {
                            palette::MUTED
                        },
                    },
                    ..base
                }
            });
        r.push(btn)
    });

    let mut proxy_section = column![
        text("Local proxy preference (separate from upstream settings)")
            .size(18)
            .color(palette::TEXT_PRIMARY),
        text("Configure an explicit HTTP(S) proxy when behind Windows PAC/WPAD or per-protocol proxies. Preserved across account sign-out and switches.")
            .size(13)
            .color(palette::TEXT_SECONDARY),
        mode_row,
    ]
    .spacing(8);

    if state.proxy_mode == ProxyModeChoice::Manual {
        proxy_section = proxy_section.push(
            text_input("http://127.0.0.1:8080", state.proxy_url_input)
                .on_input(Message::ProxyUrlChanged)
                .on_submit(Message::ApplyProxy)
                .padding(8),
        );
    }

    proxy_section = proxy_section.push(
        button(text("Apply proxy").size(13))
            .padding([8, 14])
            .on_press(Message::ApplyProxy),
    );

    if let Some((is_error, msg)) = state.proxy_feedback {
        let color = if *is_error {
            palette::DANGER
        } else {
            palette::SUCCESS
        };
        proxy_section = proxy_section.push(text(msg.clone()).size(13).color(color));
    }

    let diagnostics_section = column![
        text("Application directories & diagnostics")
            .size(16)
            .color(palette::TEXT_PRIMARY),
        text(format!("Config: {config_path}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text(format!("Data (SQLite): {data_path}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text(format!("Cache: {cache_path}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text(format!("Logs: {logs_path}"))
            .size(13)
            .color(palette::TEXT_SECONDARY),
        text("Wallora is an unofficial client and is not affiliated with Wallhaven.cc.")
            .size(13)
            .color(palette::MUTED),
    ]
    .spacing(6);

    let card = |inner| {
        container(inner)
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
            })
    };

    scrollable(
        column![
            text("Settings").size(28).color(palette::TEXT_PRIMARY),
            card(upstream_section),
            card(proxy_section),
            card(diagnostics_section),
        ]
        .spacing(14)
        .width(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}
