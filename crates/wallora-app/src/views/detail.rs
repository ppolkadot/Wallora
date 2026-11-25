use std::path::Path;

use iced::widget::{
    Space, button, column, container, image as iced_image, row, scrollable, stack, text,
};
use iced::{Alignment, Background, Border, Color, ContentFit, Element, Length};
use wallora_core::domain::{Category, Purity, Wallpaper, WallpaperId};
use wallora_core::provider::ProviderError;
use wallora_core::services::DetailUnsupportedAction;

use crate::message::{DetailCopyTarget, GalleryKind, Message, Route};
use crate::theme::{card_style, palette, pill_button_style};

const BYTES_PER_GB: u64 = 1_000_000_000;
const BYTES_PER_MB: u64 = 1_000_000;
const BYTES_PER_KB: u64 = 1_000;

pub enum DetailPreviewView<'a> {
    Loading {
        fallback_used: bool,
    },
    Loaded {
        handle: &'a iced_image::Handle,
        stale: bool,
        fallback_used: bool,
    },
    Failed {
        message: &'a str,
    },
}

pub enum DetailDownloadView<'a> {
    Idle,
    PickingLocation,
    Downloading {
        target: &'a Path,
        bytes_written: u64,
        total_bytes: Option<u64>,
    },
    Saved {
        path: &'a Path,
        bytes_written: u64,
        collision_renamed: bool,
    },
    Failed {
        message: &'a str,
        part_path: Option<&'a Path>,
    },
}

pub struct DetailViewState<'a> {
    pub wallpaper_id: &'a WallpaperId,
    pub return_to: GalleryKind,
    pub metadata_loading: bool,
    pub metadata_error: Option<&'a ProviderError>,
    pub wallpaper: Option<&'a Wallpaper>,
    pub has_full_metadata: bool,
    pub is_guest: bool,
    pub preview: DetailPreviewView<'a>,
    pub download: DetailDownloadView<'a>,
    pub default_filename: Option<String>,
    pub copy_feedback: Option<&'a str>,
}

/// Formats a byte count cleanly for wallpaper metadata and download status.
pub(crate) fn format_file_size(bytes: u64) -> String {
    if bytes >= BYTES_PER_GB {
        format!("{:.2} GB", bytes as f64 / BYTES_PER_GB as f64)
    } else if bytes >= BYTES_PER_MB {
        format!("{:.1} MB", bytes as f64 / BYTES_PER_MB as f64)
    } else if bytes >= BYTES_PER_KB {
        format!("{:.1} KB", bytes as f64 / BYTES_PER_KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Parses a `#RRGGBB` or `RRGGBB` hex string into an Iced `Color`.
pub(crate) fn parse_hex_color(raw: &str) -> Option<Color> {
    let hex = raw.trim().strip_prefix('#').unwrap_or(raw.trim());
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::from_rgb8(r, g, b))
}

fn purity_label(purity: Purity) -> (&'static str, Color) {
    match purity {
        Purity::Sfw => ("SFW", palette::SUCCESS),
        Purity::Sketchy => ("Sketchy", palette::WARNING),
        Purity::Nsfw => ("NSFW", palette::DANGER),
    }
}

fn category_label(category: Category) -> &'static str {
    match category {
        Category::General => "General",
        Category::Anime => "Anime",
        Category::People => "People",
    }
}

pub fn view<'a>(state: DetailViewState<'a>) -> Element<'a, Message> {
    let back_label = match state.return_to {
        GalleryKind::Home => "Back to Home",
        GalleryKind::Search => "Back to Search",
    };

    let mut refresh_btn = button(
        text(if state.metadata_loading {
            "Refreshing..."
        } else {
            "Refresh"
        })
        .size(13),
    )
    .padding([7, 14]);
    if !state.metadata_loading {
        refresh_btn = refresh_btn.on_press(Message::DetailRetryMetadata);
    }

    let top_bar = row![
        button(text(back_label).size(13))
            .padding([7, 14])
            .on_press(Message::DetailBack)
            .style(pill_button_style(false)),
        text(format!("Wallpaper {}", state.wallpaper_id.as_str()))
            .size(22)
            .color(palette::TEXT_PRIMARY),
        refresh_btn,
    ]
    .spacing(14)
    .align_y(Alignment::Center);

    let mut root = column![top_bar].spacing(14);

    if let Some(err) = state.metadata_error {
        let mut actions = row![
            button(text("Retry").size(13))
                .padding([6, 14])
                .on_press(Message::DetailRetryMetadata),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        if state.is_guest && *err == ProviderError::Unauthorized {
            actions = actions.push(
                button(text("Open Account").size(13))
                    .padding([6, 14])
                    .on_press(Message::Navigate(Route::Account)),
            );
        }

        let err_card = container(
            column![
                text("Could not load full wallpaper details")
                    .size(15)
                    .color(palette::DANGER),
                text(err.user_message())
                    .size(13)
                    .color(palette::TEXT_SECONDARY),
                actions,
            ]
            .spacing(8),
        )
        .padding(14)
        .width(Length::Fill)
        .style(card_style(palette::DANGER, 1.0));

        if state.wallpaper.is_none() {
            root = root.push(err_card);
            return root.width(Length::Fill).height(Length::Fill).into();
        }
        root = root.push(err_card);
    }

    let left_pane = build_preview_pane(&state);
    let right_pane = build_sidebar_pane(&state);

    let body = row![
        container(left_pane)
            .width(Length::FillPortion(3))
            .height(Length::Fill),
        container(right_pane)
            .width(Length::FillPortion(2))
            .height(Length::Fill),
    ]
    .spacing(18)
    .width(Length::Fill)
    .height(Length::Fill);

    root.push(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn build_preview_pane<'a>(state: &DetailViewState<'a>) -> Element<'a, Message> {
    let frame: Element<'a, Message> = match state.preview {
        DetailPreviewView::Loading { fallback_used } => container(
            text(if state.metadata_loading && !state.has_full_metadata {
                "Loading wallpaper details..."
            } else if fallback_used {
                "Loading thumbnail fallback (original exceeds preview budget)..."
            } else {
                "Loading preview..."
            })
            .size(14)
            .color(palette::TEXT_SECONDARY),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
        DetailPreviewView::Loaded {
            handle,
            stale,
            fallback_used,
        } => {
            let img = iced_image(handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Contain);
            if stale || fallback_used {
                let mut badges = column![].spacing(6);
                if stale {
                    badges = badges.push(badge_chip("Offline (stale)", palette::WARNING));
                }
                if fallback_used {
                    badges = badges.push(badge_chip(
                        "Showing thumbnail fallback (original exceeds preview budget)",
                        palette::ACCENT,
                    ));
                }
                stack![img, container(badges).padding(10)]
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            } else {
                img.into()
            }
        }
        DetailPreviewView::Failed { message } => container(
            column![
                text("Preview could not be loaded")
                    .size(15)
                    .color(palette::DANGER),
                text(message).size(13).color(palette::TEXT_SECONDARY),
                button(text("Retry preview").size(13))
                    .padding([7, 14])
                    .on_press(Message::DetailRetryPreview),
            ]
            .spacing(10)
            .align_x(Alignment::Center),
        )
        .padding(20)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
    };

    let preview_box = container(frame)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(8)
        .style(|_| container::Style {
            background: Some(Background::Color(palette::CANVAS)),
            border: Border {
                radius: 8.0.into(),
                width: 1.0,
                color: palette::MUTED,
            },
            ..Default::default()
        });

    let mut pane = column![preview_box].spacing(10).height(Length::Fill);

    if let Some(wallpaper) = state.wallpaper
        && !wallpaper.colors.is_empty()
    {
        let mut swatches = row![text("Colors:").size(12).color(palette::TEXT_SECONDARY)]
            .spacing(8)
            .align_y(Alignment::Center);
        for hex in &wallpaper.colors {
            let swatch_color = parse_hex_color(hex).unwrap_or(palette::MUTED);
            let box_el =
                container(Space::new().width(16).height(16)).style(move |_| container::Style {
                    background: Some(Background::Color(swatch_color)),
                    border: Border {
                        radius: 4.0.into(),
                        width: 1.0,
                        color: palette::MUTED,
                    },
                    ..Default::default()
                });
            swatches = swatches.push(
                row![
                    box_el,
                    text(hex.as_str()).size(12).color(palette::TEXT_SECONDARY)
                ]
                .spacing(4)
                .align_y(Alignment::Center),
            );
        }
        pane = pane.push(
            container(swatches.wrap().vertical_spacing(6))
                .padding([8, 12])
                .width(Length::Fill)
                .style(card_style(palette::MUTED, 1.0)),
        );
    }

    pane.into()
}

fn badge_chip<'a>(label: &'a str, border_color: Color) -> Element<'a, Message> {
    container(text(label).size(11).color(border_color))
        .padding([4, 8])
        .style(move |_| container::Style {
            background: Some(Background::Color(Color {
                a: 0.88,
                ..palette::CANVAS
            })),
            border: Border {
                radius: 4.0.into(),
                width: 1.0,
                color: border_color,
            },
            ..Default::default()
        })
        .into()
}

fn build_sidebar_pane<'a>(state: &DetailViewState<'a>) -> Element<'a, Message> {
    let mut cards = column![].spacing(12).width(Length::Fill);

    if let Some(wallpaper) = state.wallpaper {
        cards = cards.push(build_actions_card(state, wallpaper));
        cards = cards.push(build_metadata_card(wallpaper));
        cards = cards.push(build_tags_card(state, wallpaper));
    } else if state.metadata_loading {
        cards = cards.push(
            container(
                text("Loading metadata...")
                    .size(14)
                    .color(palette::TEXT_SECONDARY),
            )
            .padding(16)
            .width(Length::Fill)
            .style(card_style(palette::MUTED, 1.0)),
        );
    }

    cards = cards.push(build_handoffs_card());

    scrollable(cards).height(Length::Fill).into()
}

fn build_actions_card<'a>(
    state: &DetailViewState<'a>,
    wallpaper: &'a Wallpaper,
) -> Element<'a, Message> {
    let busy_downloading = matches!(
        state.download,
        DetailDownloadView::PickingLocation | DetailDownloadView::Downloading { .. }
    );
    let can_download = state.has_full_metadata && !busy_downloading;

    let mut download_btn = button(
        text(match state.download {
            DetailDownloadView::PickingLocation => "Choosing location...",
            DetailDownloadView::Downloading { .. } => "Downloading...",
            _ => "Download original",
        })
        .size(13),
    )
    .padding([8, 14])
    .style(pill_button_style(can_download));
    if can_download {
        download_btn = download_btn.on_press(Message::DetailDownloadOriginal);
    }

    let more_like_btn = button(text("More like this (like:)").size(13))
        .padding([8, 14])
        .on_press(Message::DetailMoreLikeThis(wallpaper.id.clone()))
        .style(pill_button_style(false));

    let primary_actions = row![download_btn, more_like_btn]
        .spacing(8)
        .wrap()
        .vertical_spacing(8);

    let mut col = column![
        text("Actions").size(16).color(palette::TEXT_PRIMARY),
        primary_actions,
    ]
    .spacing(10);

    if let Some(default_name) = &state.default_filename {
        col = col.push(
            text(format!("Default filename: {default_name}"))
                .size(12)
                .color(palette::MUTED),
        );
    }

    match state.download {
        DetailDownloadView::Idle => {}
        DetailDownloadView::PickingLocation => {
            col = col.push(
                text("Waiting for native save dialog...")
                    .size(12)
                    .color(palette::TEXT_SECONDARY),
            );
        }
        DetailDownloadView::Downloading {
            target,
            bytes_written,
            total_bytes,
        } => {
            let progress_text = match total_bytes {
                Some(total) if total > 0 => format!(
                    "Downloading to {}: {} / {}",
                    target.display(),
                    format_file_size(bytes_written),
                    format_file_size(total)
                ),
                _ => format!(
                    "Downloading to {}: {}",
                    target.display(),
                    format_file_size(bytes_written)
                ),
            };
            col = col.push(
                row![
                    text(progress_text).size(12).color(palette::TEXT_SECONDARY),
                    button(text("Cancel").size(12))
                        .padding([4, 10])
                        .on_press(Message::DetailCancelDownload),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        DetailDownloadView::Saved {
            path,
            bytes_written,
            collision_renamed,
        } => {
            let msg = if collision_renamed {
                format!(
                    "Saved {} to {} (renamed to avoid overwriting an existing file)",
                    format_file_size(bytes_written),
                    path.display()
                )
            } else {
                format!(
                    "Saved {} to {}",
                    format_file_size(bytes_written),
                    path.display()
                )
            };
            col = col.push(text(msg).size(12).color(palette::SUCCESS));
        }
        DetailDownloadView::Failed { message, part_path } => {
            col = col.push(text(message).size(12).color(palette::DANGER));
            if let Some(part) = part_path {
                col = col.push(
                    text(format!("Partial file kept at: {}", part.display()))
                        .size(11)
                        .color(palette::TEXT_SECONDARY),
                );
            }
        }
    }

    let mut copy_row = row![
        button(text("Copy Page URL").size(12))
            .padding([6, 12])
            .on_press(Message::DetailCopyLink {
                target: DetailCopyTarget::Page,
                url: wallpaper.url.clone(),
            })
            .style(pill_button_style(false)),
        button(text("Copy Original URL").size(12))
            .padding([6, 12])
            .on_press(Message::DetailCopyLink {
                target: DetailCopyTarget::Original,
                url: wallpaper.path.clone(),
            })
            .style(pill_button_style(false)),
    ]
    .spacing(8);

    if let Some(short) = wallpaper.short_url.as_deref().filter(|s| !s.is_empty()) {
        copy_row = copy_row.push(
            button(text("Copy Short URL").size(12))
                .padding([6, 12])
                .on_press(Message::DetailCopyLink {
                    target: DetailCopyTarget::Short,
                    url: short.to_string(),
                })
                .style(pill_button_style(false)),
        );
    }

    col = col.push(copy_row.wrap().vertical_spacing(8));
    if let Some(feedback) = state.copy_feedback {
        col = col.push(text(feedback).size(12).color(palette::SUCCESS));
    }

    container(col)
        .padding(14)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0))
        .into()
}

fn build_metadata_card<'a>(wallpaper: &'a Wallpaper) -> Element<'a, Message> {
    let (purity_text, purity_color) = purity_label(wallpaper.purity);
    let uploader_line = match &wallpaper.uploader {
        Some(uploader) if !uploader.group.is_empty() => {
            format!("{} ({})", uploader.username, uploader.group)
        }
        Some(uploader) => uploader.username.clone(),
        None => "Unknown".to_string(),
    };
    let source_line = if wallpaper.source.trim().is_empty() {
        "None".to_string()
    } else {
        wallpaper.source.clone()
    };

    let dims_line = if wallpaper.ratio.is_empty() {
        format!("{}x{}", wallpaper.dimension_x, wallpaper.dimension_y)
    } else {
        format!(
            "{}x{} (ratio {})",
            wallpaper.dimension_x, wallpaper.dimension_y, wallpaper.ratio
        )
    };

    container(
        column![
            text("Details").size(16).color(palette::TEXT_PRIMARY),
            row![
                text("Purity:").size(13).color(palette::TEXT_SECONDARY),
                text(purity_text).size(13).color(purity_color),
                text(format!(
                    "•  Category: {}",
                    category_label(wallpaper.category)
                ))
                .size(13)
                .color(palette::TEXT_SECONDARY),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(format!("Dimensions: {dims_line}"))
                .size(13)
                .color(palette::TEXT_SECONDARY),
            text(format!(
                "File: {} ({})",
                format_file_size(wallpaper.file_size),
                wallpaper.file_type
            ))
            .size(13)
            .color(palette::TEXT_SECONDARY),
            text(format!(
                "Stats: {} views  •  {} favorites",
                wallpaper.views, wallpaper.favorites
            ))
            .size(13)
            .color(palette::TEXT_SECONDARY),
            text(format!("Created: {}", wallpaper.created_at))
                .size(13)
                .color(palette::TEXT_SECONDARY),
            text(format!("Uploader: {uploader_line}"))
                .size(13)
                .color(palette::TEXT_SECONDARY),
            text(format!("Source: {source_line}"))
                .size(13)
                .color(palette::TEXT_SECONDARY),
        ]
        .spacing(6),
    )
    .padding(14)
    .width(Length::Fill)
    .style(card_style(palette::MUTED, 1.0))
    .into()
}

fn build_tags_card<'a>(
    state: &DetailViewState<'a>,
    wallpaper: &'a Wallpaper,
) -> Element<'a, Message> {
    let mut col = column![text("Tags").size(16).color(palette::TEXT_PRIMARY)].spacing(8);

    if wallpaper.tags.is_empty() {
        let msg = if state.metadata_loading && !state.has_full_metadata {
            "Loading tags..."
        } else {
            "No tags listed."
        };
        col = col.push(text(msg).size(13).color(palette::MUTED));
    } else {
        let tags_row = wallpaper
            .tags
            .iter()
            .fold(row![].spacing(6), |r, tag| {
                r.push(
                    button(text(tag.name.as_str()).size(12))
                        .padding([4, 10])
                        .on_press(Message::DetailSelectTag {
                            id: tag.id,
                            name: tag.name.clone(),
                        })
                        .style(pill_button_style(false)),
                )
            })
            .wrap()
            .vertical_spacing(6);
        col = col.push(tags_row);
    }

    container(col)
        .padding(14)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0))
        .into()
}

fn build_handoffs_card<'a>() -> Element<'a, Message> {
    let mut col = column![text("Website tools").size(16).color(palette::TEXT_PRIMARY)].spacing(10);

    for action in DetailUnsupportedAction::ALL {
        col = col.push(
            column![
                text(action.title()).size(13).color(palette::TEXT_PRIMARY),
                text(action.explanation())
                    .size(12)
                    .color(palette::TEXT_SECONDARY),
                button(text(action.label()).size(12))
                    .padding([5, 12])
                    .on_press(Message::DetailOpenHandOff(action))
                    .style(pill_button_style(false)),
            ]
            .spacing(4),
        );
    }

    container(col)
        .padding(14)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_size_formatting_covers_b_kb_mb_and_gb() {
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(524_288), "524.3 KB");
        assert_eq!(format_file_size(2_500_000), "2.5 MB");
        assert_eq!(format_file_size(1_500_000_000), "1.50 GB");
    }

    #[test]
    fn hex_color_parser_handles_valid_and_invalid_values() {
        assert_eq!(
            parse_hex_color("#f37316"),
            Some(Color::from_rgb8(0xf3, 0x73, 0x16))
        );
        assert_eq!(
            parse_hex_color("1e1c1a"),
            Some(Color::from_rgb8(0x1e, 0x1c, 0x1a))
        );
        assert_eq!(parse_hex_color("#xyz"), None);
        assert_eq!(parse_hex_color("12345"), None);
    }
}
