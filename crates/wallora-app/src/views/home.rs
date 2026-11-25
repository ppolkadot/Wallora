use iced::widget::{Id, button, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, widget::column};
use wallora_core::domain::{TopRange, Wallpaper};
use wallora_core::services::{DiscoveryController, DiscoveryView, UnsupportedFeature};

use crate::message::{GalleryKind, Message};
use crate::theme::{palette, pill_button_style, surface_color};
use crate::widgets::gallery_grid::{self, GalleryGridView, GridMetrics};
use crate::widgets::thumbnail::ThumbnailViewState;

pub const ALL_TOP_RANGES: [TopRange; 7] = [
    TopRange::OneDay,
    TopRange::ThreeDays,
    TopRange::OneWeek,
    TopRange::OneMonth,
    TopRange::ThreeMonths,
    TopRange::SixMonths,
    TopRange::OneYear,
];

pub struct HomeViewState<'a, F>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    pub controller: &'a DiscoveryController,
    pub scrollable_id: &'a Id,
    pub metrics: &'a GridMetrics,
    pub selected: Option<usize>,
    pub is_guest: bool,
    pub thumbnail_state: F,
}

pub fn view<'a, F>(state: HomeViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let active_view = state.controller.view();
    let is_latest = matches!(active_view, DiscoveryView::Latest);
    let is_toplist = matches!(active_view, DiscoveryView::Toplist { .. });
    let is_random = matches!(active_view, DiscoveryView::Random);

    let feed_tabs = row![
        button(text("Latest").size(13))
            .padding([7, 14])
            .on_press(Message::HomeSelectLatest)
            .style(pill_button_style(is_latest)),
        button(text("Toplist").size(13))
            .padding([7, 14])
            .on_press(Message::HomeSelectToplist)
            .style(pill_button_style(is_toplist)),
        button(text("Random").size(13))
            .padding([7, 14])
            .on_press(Message::HomeSelectRandom)
            .style(pill_button_style(is_random)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let refresh_label = if is_random { "New Random" } else { "Refresh" };
    let refresh_btn = button(text(refresh_label).size(13))
        .padding([7, 14])
        .on_press(Message::GalleryRefresh(GalleryKind::Home));

    let mut title_row = row![
        text("Home").size(24).color(palette::TEXT_PRIMARY),
        feed_tabs,
        refresh_btn,
    ]
    .spacing(16)
    .align_y(Alignment::Center);

    if let Some(total) = state.controller.total() {
        title_row = title_row.push(
            text(format!("{total} results"))
                .size(13)
                .color(palette::TEXT_SECONDARY),
        );
    }
    if is_random && let Some(seed) = state.controller.seed() {
        title_row = title_row.push(text(format!("Seed: {seed}")).size(12).color(palette::MUTED));
    }

    let mut header = column![title_row].spacing(10);

    if let DiscoveryView::Toplist { range } = active_view {
        let range_row = ALL_TOP_RANGES
            .into_iter()
            .fold(
                row![text("Range:").size(13).color(palette::TEXT_SECONDARY)].spacing(6),
                |r, option| {
                    let active = option == range;
                    r.push(
                        button(text(option.as_param()).size(12))
                            .padding([5, 10])
                            .on_press(Message::HomeSelectTopRange(option))
                            .style(pill_button_style(active)),
                    )
                },
            )
            .align_y(Alignment::Center);
        header = header.push(range_row);
    }

    let hot_handoff = UnsupportedFeature::HotSorting.hand_off();
    let hot_notice = container(
        row![
            text(UnsupportedFeature::HotSorting.explanation())
                .size(12)
                .color(palette::TEXT_SECONDARY),
            button(text(hot_handoff.label).size(12))
                .padding([5, 12])
                .on_press(Message::OpenUnsupportedFeature(
                    UnsupportedFeature::HotSorting
                )),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([8, 12])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(surface_color())),
        border: Border {
            radius: 6.0.into(),
            width: 1.0,
            color: palette::MUTED,
        },
        ..Default::default()
    });
    header = header.push(hot_notice);

    let grid = gallery_grid::view(GalleryGridView {
        kind: GalleryKind::Home,
        scrollable_id: state.scrollable_id,
        metrics: state.metrics,
        items: state.controller.items(),
        status: state.controller.status(),
        is_loading: state.controller.is_loading(),
        total: state.controller.total(),
        selected: state.selected,
        empty_message: "No wallpapers returned for this feed.",
        show_account_shortcut_on_unauthorized: state.is_guest,
        thumbnail_state: state.thumbnail_state,
    });

    column![header, grid]
        .spacing(14)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
