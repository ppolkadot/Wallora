use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Background, Border, Element, Length};
use wallora_core::domain::{CacheCapError, HARD_MAX_CACHE_CAP_BYTES};
use wallora_core::services::cache::CacheStats;
use wallora_core::services::{ImageGuard, SessionManager};
use wallora_core::storage::AppPaths;

use crate::message::{Message, ProxyModeChoice};
use crate::theme::{palette, surface_color};

pub struct SettingsViewState<'a> {
    pub paths: &'a AppPaths,
    pub session: &'a SessionManager,
    pub proxy_mode: ProxyModeChoice,
    pub proxy_url_input: &'a str,
    pub proxy_feedback: Option<&'a (bool, String)>,
    pub cache_available: bool,
    pub cache_cap_input: &'a str,
    pub cache_cap_feedback: Option<&'a (bool, String)>,
    pub cache_cap_busy: bool,
    pub cache_stats: Option<&'a CacheStats>,
    pub image_guard: &'a ImageGuard,
}

const BYTES_PER_GB: u64 = 1_000_000_000;
const BYTES_PER_MB: u64 = 1_000_000;

/// Parses a decimal gigabyte amount (for example `2`, `2.5`, `0.25 GB`) into bytes.
///
/// 1 GB is 1,000,000,000 bytes, so the 10 GB hard maximum is exactly `10`. Amounts too large for
/// `u64` saturate so the caller reports them as exceeding the hard maximum.
pub(crate) fn parse_cache_cap_gb(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_suffix("GB")
        .or_else(|| trimmed.strip_suffix("gb"))
        .or_else(|| trimmed.strip_suffix("Gb"))
        .unwrap_or(trimmed)
        .trim();
    let invalid = || "Enter the cache size in GB, for example 2 or 2.5.".to_string();
    if trimmed.is_empty() {
        return Err(invalid());
    }
    let (whole, fraction) = match trimmed.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (trimmed, ""),
    };
    if (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    if fraction.len() > 9 {
        return Err("Use at most 9 decimal places.".to_string());
    }
    let whole_gb = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().unwrap_or(u64::MAX)
    };
    let mut padded = fraction.to_string();
    while padded.len() < 9 {
        padded.push('0');
    }
    let fraction_bytes = padded.parse::<u64>().map_err(|_| invalid())?;
    Ok(whole_gb
        .saturating_mul(BYTES_PER_GB)
        .saturating_add(fraction_bytes))
}

/// Formats a byte count as exact decimal GB for the cap input (`2000000000` -> `2`).
pub(crate) fn format_cap_gb(bytes: u64) -> String {
    let whole = bytes / BYTES_PER_GB;
    let fraction = bytes % BYTES_PER_GB;
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = format!("{fraction:09}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// Formats a byte count for status text (`1.50 GB`, `12.3 MB`, `512 B`).
pub(crate) fn format_size(bytes: u64) -> String {
    if bytes >= BYTES_PER_GB {
        format!("{:.2} GB", bytes as f64 / BYTES_PER_GB as f64)
    } else if bytes >= BYTES_PER_MB {
        format!("{:.1} MB", bytes as f64 / BYTES_PER_MB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// User-facing text for a rejected cache cap request.
pub(crate) fn describe_cache_cap_error(error: &CacheCapError) -> String {
    match error {
        CacheCapError::Zero => "The cache size must be greater than zero.".to_string(),
        CacheCapError::ExceedsHardMax { .. } => format!(
            "The cache size cannot exceed {} GB.",
            format_cap_gb(HARD_MAX_CACHE_CAP_BYTES)
        ),
    }
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

    let mut cache_section = column![
        text("Image cache (thumbnails and previews)")
            .size(18)
            .color(palette::TEXT_PRIMARY),
        text(format!(
            "Lowering the size evicts the least recently used images. Hard maximum: {} GB. Applied wallpapers and saved downloads are never stored here.",
            format_cap_gb(HARD_MAX_CACHE_CAP_BYTES)
        ))
        .size(13)
        .color(palette::TEXT_SECONDARY),
    ]
    .spacing(8);

    if state.cache_available {
        match state.cache_stats {
            Some(stats) => {
                cache_section = cache_section.push(
                    text(format!(
                        "Using {} of {} ({} images, {} writing, {} awaiting deletion)",
                        format_size(stats.total_charged_bytes),
                        format_size(stats.cap_bytes),
                        stats.entry_count,
                        stats.live_reservations,
                        stats.pending_deletes,
                    ))
                    .size(13)
                    .color(palette::TEXT_SECONDARY),
                );
            }
            None => {
                cache_section = cache_section.push(
                    text("Cache status is loading...")
                        .size(13)
                        .color(palette::MUTED),
                );
            }
        }
    } else {
        cache_section = cache_section.push(
            text("The image cache is unavailable in this window, so images are not cached. A saved size applies the next time the cache opens.")
                .size(13)
                .color(palette::WARNING),
        );
    }

    let mut cap_input = text_input("2", state.cache_cap_input)
        .on_input(Message::CacheCapInputChanged)
        .padding(8)
        .width(Length::Fixed(120.0));
    let mut apply = button(text("Apply cache size").size(13)).padding([8, 14]);
    if !state.cache_cap_busy {
        cap_input = cap_input.on_submit(Message::ApplyCacheCap);
        apply = apply.on_press(Message::ApplyCacheCap);
    }
    cache_section = cache_section.push(
        row![
            text("Cache size (GB)")
                .size(13)
                .color(palette::TEXT_PRIMARY),
            cap_input,
            apply,
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    );

    if let Some((is_error, msg)) = state.cache_cap_feedback {
        let color = if *is_error {
            palette::DANGER
        } else {
            palette::SUCCESS
        };
        cache_section = cache_section.push(text(msg.clone()).size(13).color(color));
    }

    cache_section = cache_section.push(
        text(format!(
            "Decoder limits: {} concurrent decodes, {} MiB of decoded images in flight.",
            state.image_guard.decode_concurrency(),
            state.image_guard.decoded_bytes_budget() / (1024 * 1024)
        ))
        .size(12)
        .color(palette::MUTED),
    );

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
            card(cache_section),
            card(diagnostics_section),
        ]
        .spacing(14)
        .width(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_cap_parser_accepts_decimal_gigabytes() {
        for (raw, bytes) in [
            ("2", 2_000_000_000),
            ("2.5", 2_500_000_000),
            (" 0.25 GB ", 250_000_000),
            (".5", 500_000_000),
            ("10", 10_000_000_000),
            ("10.000000000", 10_000_000_000),
            ("0.000000001", 1),
            ("3gb", 3_000_000_000),
        ] {
            assert_eq!(parse_cache_cap_gb(raw), Ok(bytes), "{raw:?}");
        }
    }

    #[test]
    fn cache_cap_parser_rejects_malformed_input_and_saturates_huge_values() {
        for raw in [
            "", "  ", "abc", "1.2.3", "-1", "+1", "1e3", ".", "2 GB GB", "1,5",
        ] {
            assert!(parse_cache_cap_gb(raw).is_err(), "{raw:?}");
        }
        assert!(parse_cache_cap_gb("1.0000000001").is_err());
        assert_eq!(parse_cache_cap_gb("99999999999999999999999"), Ok(u64::MAX));
        assert_eq!(
            parse_cache_cap_gb("18446744073.709551616"),
            Ok(u64::MAX),
            "values just past u64 must saturate"
        );
    }

    #[test]
    fn cache_cap_formatting_round_trips_exactly() {
        for bytes in [1, 1_500_000_000, 2_000_000_000, 10_000_000_000, 123_456_789] {
            assert_eq!(parse_cache_cap_gb(&format_cap_gb(bytes)), Ok(bytes));
        }
        assert_eq!(format_cap_gb(2_000_000_000), "2");
        assert_eq!(format_cap_gb(2_500_000_000), "2.5");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(12_300_000), "12.3 MB");
        assert_eq!(format_size(1_500_000_000), "1.50 GB");
    }
}
