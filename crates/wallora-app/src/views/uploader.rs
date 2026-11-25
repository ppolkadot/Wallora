use iced::widget::{Id, button, column, container, row, text};
use iced::{Alignment, Element, Length};
use wallora_core::domain::{PurityMask, Sorting, Wallpaper};
use wallora_core::services::{ProfileTagUnsupportedAction, UploaderController};

use crate::message::{GalleryKind, Message};
use crate::theme::{card_style, palette, pill_button_style};
use crate::views::{GalleryFilterBarProps, gallery_filter_bar};
use crate::widgets::gallery_grid::{self, GalleryGridView, GridMetrics};
use crate::widgets::thumbnail::ThumbnailViewState;

pub struct UploaderViewState<'a, F>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    pub controller: &'a UploaderController,
    pub back_label: &'a str,
    pub selectable_purity: PurityMask,
    pub is_guest: bool,
    pub scrollable_id: &'a Id,
    pub metrics: &'a GridMetrics,
    pub selected: Option<usize>,
    pub thumbnail_state: F,
}

pub fn view<'a, F>(state: UploaderViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let username = state.controller.username().unwrap_or("uploader");
    let title = format!("@{username}");
    let search_label = format!("Search @{username}");

    let mut top_bar = row![
        button(text(state.back_label).size(13))
            .padding([7, 14])
            .on_press(Message::UploaderBack)
            .style(pill_button_style(false)),
        text(title).size(24).color(palette::TEXT_PRIMARY),
        button(text("View public collections").size(13))
            .padding([7, 14])
            .on_press(Message::UploaderViewPublicCollections)
            .style(pill_button_style(false)),
        button(text(search_label).size(13))
            .padding([7, 14])
            .on_press(Message::UploaderSearchUploads)
            .style(pill_button_style(false)),
        button(text("Refresh").size(13))
            .padding([7, 14])
            .on_press(Message::GalleryRefresh(GalleryKind::Uploader))
            .style(pill_button_style(false)),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    if let Some(total) = state.controller.total() {
        top_bar = top_bar.push(
            text(format!("{total} wallpapers"))
                .size(13)
                .color(palette::TEXT_SECONDARY),
        );
    }
    if state.controller.sorting() == Sorting::Random
        && let Some(seed) = state.controller.seed()
    {
        top_bar = top_bar.push(text(format!("Seed: {seed}")).size(12).color(palette::MUTED));
    }

    let profile_handoff_card = container(
        row![
            text(ProfileTagUnsupportedAction::UserProfileAndComments.explanation())
                .size(12)
                .color(palette::TEXT_SECONDARY),
            button(text(ProfileTagUnsupportedAction::UserProfileAndComments.label()).size(12))
                .padding([5, 12])
                .on_press(Message::UploaderOpenProfileHandOff)
                .style(pill_button_style(false)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .wrap()
        .vertical_spacing(6),
    )
    .padding(12)
    .width(Length::Fill)
    .style(card_style(palette::MUTED, 1.0));

    let filter_bar = gallery_filter_bar(GalleryFilterBarProps {
        purity: state.controller.purity(),
        selectable_purity: state.selectable_purity,
        sorting: state.controller.sorting(),
        order: state.controller.order(),
        top_range: state.controller.top_range(),
        on_toggle_purity: Message::UploaderTogglePurity,
        on_select_sorting: Message::UploaderSelectSorting,
        on_select_order: Message::UploaderSelectOrder,
        on_select_top_range: Message::UploaderSelectTopRange,
    });

    let grid = gallery_grid::view(GalleryGridView {
        kind: GalleryKind::Uploader,
        scrollable_id: state.scrollable_id,
        metrics: state.metrics,
        items: state.controller.items(),
        status: state.controller.status(),
        is_loading: state.controller.is_loading(),
        total: state.controller.total(),
        selected: state.selected,
        empty_message: "No wallpapers found for this uploader and purity.",
        show_account_shortcut_on_unauthorized: state.is_guest,
        thumbnail_state: state.thumbnail_state,
    });

    column![
        top_bar.wrap().vertical_spacing(8),
        profile_handoff_card,
        filter_bar,
        grid,
    ]
    .spacing(12)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
