use std::fmt;

use iced::widget::{button, column, container, image as iced_image, mouse_area, row, stack, text};
use iced::{Alignment, Background, Border, Color, ContentFit, Element, Length};
use wallora_core::domain::{Purity, Thumbs, Wallpaper};

use crate::message::{GalleryKind, Message};
use crate::theme::{palette, surface_color};

pub const TILE_W: f32 = 264.0;
pub const TILE_IMAGE_H: f32 = 168.0;
pub const TILE_CARD_H: f32 = 212.0;

#[derive(Clone)]
pub struct LoadedThumbnail {
    pub handle: iced_image::Handle,
    pub stale: bool,
}

impl fmt::Debug for LoadedThumbnail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedThumbnail")
            .field("stale", &self.stale)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ThumbnailViewState<'a> {
    Loading,
    Loaded {
        handle: &'a iced_image::Handle,
        stale: bool,
    },
    Failed {
        message: &'a str,
    },
}

pub struct ThumbnailCardProps<'a> {
    pub kind: GalleryKind,
    pub index: usize,
    pub wallpaper: &'a Wallpaper,
    pub state: ThumbnailViewState<'a>,
    pub selected: bool,
}

/// Chooses the API-supplied thumbnail URL matching upstream `thumb_size` preferences without ever
/// constructing thumbnail URLs locally.
pub fn select_thumbnail_url<'a>(thumbs: &'a Thumbs, preferred: &str) -> &'a str {
    let candidate = match preferred.trim().to_ascii_lowercase().as_str() {
        "small" => thumbs.small.as_str(),
        "orig" | "original" => thumbs.original.as_str(),
        _ => thumbs.large.as_str(),
    };
    if !candidate.is_empty() {
        return candidate;
    }
    [
        thumbs.large.as_str(),
        thumbs.original.as_str(),
        thumbs.small.as_str(),
    ]
    .into_iter()
    .find(|url| !url.is_empty())
    .unwrap_or("")
}

fn purity_badge(purity: Purity) -> (&'static str, Color) {
    match purity {
        Purity::Sfw => ("SFW", palette::SUCCESS),
        Purity::Sketchy => ("Sketchy", palette::WARNING),
        Purity::Nsfw => ("NSFW", palette::DANGER),
    }
}

pub fn view<'a>(props: ThumbnailCardProps<'a>) -> Element<'a, Message> {
    let frame: Element<'a, Message> = match props.state {
        ThumbnailViewState::Loaded { handle, stale } => {
            let img = iced_image(handle.clone())
                .width(Length::Fill)
                .height(TILE_IMAGE_H)
                .content_fit(ContentFit::Contain);
            if stale {
                let badge = container(
                    container(text("Offline (stale)").size(11).color(palette::WARNING))
                        .padding([2, 6])
                        .style(|_| container::Style {
                            background: Some(Background::Color(Color {
                                a: 0.85,
                                ..palette::CANVAS
                            })),
                            border: Border {
                                radius: 4.0.into(),
                                width: 1.0,
                                color: palette::WARNING,
                            },
                            ..Default::default()
                        }),
                )
                .padding(6);
                stack![img, badge]
                    .width(Length::Fill)
                    .height(TILE_IMAGE_H)
                    .into()
            } else {
                img.into()
            }
        }
        ThumbnailViewState::Loading => {
            container(text("Loading...").size(12).color(palette::TEXT_SECONDARY))
                .width(Length::Fill)
                .height(TILE_IMAGE_H)
                .center_x(Length::Fill)
                .center_y(TILE_IMAGE_H)
                .into()
        }
        ThumbnailViewState::Failed { message } => container(
            column![
                text("Thumbnail failed").size(12).color(palette::DANGER),
                text(message).size(11).color(palette::TEXT_SECONDARY),
                button(text("Retry").size(11))
                    .padding([4, 10])
                    .on_press(Message::RetryThumbnail {
                        kind: props.kind,
                        id: props.wallpaper.id.clone(),
                    }),
            ]
            .spacing(6)
            .align_x(Alignment::Center),
        )
        .padding(8)
        .width(Length::Fill)
        .height(TILE_IMAGE_H)
        .center_x(Length::Fill)
        .center_y(TILE_IMAGE_H)
        .into(),
    };

    let image_box = container(frame)
        .width(Length::Fill)
        .height(TILE_IMAGE_H)
        .style(|_| container::Style {
            background: Some(Background::Color(palette::CANVAS)),
            border: Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let resolution_label = if props.wallpaper.resolution.is_empty() {
        format!(
            "{}x{}",
            props.wallpaper.dimension_x, props.wallpaper.dimension_y
        )
    } else {
        props.wallpaper.resolution.clone()
    };
    let (purity_label, purity_color) = purity_badge(props.wallpaper.purity);

    let mut left_meta = row![
        text(resolution_label)
            .size(12)
            .color(palette::TEXT_SECONDARY),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    if props.wallpaper.file_type.eq_ignore_ascii_case("image/png") {
        left_meta = left_meta.push(text("PNG").size(11).color(palette::MUTED));
    }

    let metadata_strip = row![
        container(left_meta).width(Length::Fill),
        text(purity_label).size(11).color(purity_color),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let selected = props.selected;
    let card = container(column![image_box, metadata_strip].spacing(6))
        .width(TILE_W)
        .height(TILE_CARD_H)
        .padding(6)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(surface_color())),
            border: Border {
                radius: 6.0.into(),
                width: if selected { 2.0 } else { 1.0 },
                color: if selected {
                    palette::ACCENT
                } else {
                    palette::MUTED
                },
            },
            ..Default::default()
        });

    mouse_area(card)
        .on_press(Message::GallerySelectTile {
            kind: props.kind,
            index: props.index,
        })
        .into()
}
