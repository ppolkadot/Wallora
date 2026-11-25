use iced::widget::{Id, button, column, container, row, text};
use iced::{Alignment, Color, Element, Length};
use wallora_core::domain::{Purity, PurityMask, Sorting, Wallpaper};
use wallora_core::provider::ProviderError;
use wallora_core::services::{ProfileTagUnsupportedAction, TagMetadataStatus, TagPageController};

use crate::message::{GalleryKind, Message, Route};
use crate::theme::{card_style, palette, pill_button_style};
use crate::views::{GalleryFilterBarProps, gallery_filter_bar};
use crate::widgets::gallery_grid::{self, GalleryGridView, GridMetrics};
use crate::widgets::thumbnail::ThumbnailViewState;

pub struct TagViewState<'a, F>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    pub controller: &'a TagPageController,
    pub back_label: &'a str,
    pub selectable_purity: PurityMask,
    pub is_guest: bool,
    pub scrollable_id: &'a Id,
    pub metrics: &'a GridMetrics,
    pub selected: Option<usize>,
    pub thumbnail_state: F,
}

fn purity_display(purity: Purity) -> (&'static str, Color) {
    match purity {
        Purity::Sfw => ("SFW", palette::SUCCESS),
        Purity::Sketchy => ("Sketchy", palette::WARNING),
        Purity::Nsfw => ("NSFW", palette::DANGER),
    }
}

pub fn view<'a, F>(state: TagViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let title = match state.controller.tag() {
        Some(tag) => format!("#{}", tag.name),
        None => match state.controller.hint_name() {
            Some(name) => format!("#{name}"),
            None => state
                .controller
                .tag_id()
                .map_or_else(|| "Tag".to_string(), |id| format!("Tag #{id}")),
        },
    };

    let mut top_bar = row![
        button(text(state.back_label).size(13))
            .padding([7, 14])
            .on_press(Message::TagBack)
            .style(pill_button_style(false)),
        text(title).size(24).color(palette::TEXT_PRIMARY),
        button(text("Search this tag").size(13))
            .padding([7, 14])
            .on_press(Message::TagSearchThisTag)
            .style(pill_button_style(false)),
        button(text("Refresh").size(13))
            .padding([7, 14])
            .on_press(Message::GalleryRefresh(GalleryKind::Tag))
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

    let mut meta_col = column![].spacing(8);
    match state.controller.metadata_status() {
        TagMetadataStatus::Idle | TagMetadataStatus::Loading => {
            meta_col = meta_col.push(
                text("Loading tag metadata...")
                    .size(13)
                    .color(palette::TEXT_SECONDARY),
            );
        }
        TagMetadataStatus::Loaded(tag) => {
            let (purity_text, purity_color) = purity_display(tag.purity);
            let mut details = row![
                text(format!("Name: {}", tag.name))
                    .size(13)
                    .color(palette::TEXT_PRIMARY),
                text(format!("Category: {}", tag.category))
                    .size(13)
                    .color(palette::TEXT_SECONDARY),
                row![
                    text("Purity:").size(13).color(palette::TEXT_SECONDARY),
                    text(purity_text).size(13).color(purity_color),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
                text(format!("Created: {}", tag.created_at))
                    .size(13)
                    .color(palette::TEXT_SECONDARY),
            ]
            .spacing(16)
            .align_y(Alignment::Center);

            if !tag.alias.trim().is_empty() {
                details = details.push(
                    text(format!("Alias: {}", tag.alias))
                        .size(13)
                        .color(palette::TEXT_SECONDARY),
                );
            }
            meta_col = meta_col.push(details.wrap().vertical_spacing(6));
        }
        TagMetadataStatus::Failed(err) => {
            let mut actions = row![
                button(text("Retry metadata").size(12))
                    .padding([5, 12])
                    .on_press(Message::TagRetryMetadata)
                    .style(pill_button_style(false)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);

            if state.is_guest && *err == ProviderError::Unauthorized {
                actions = actions.push(
                    button(text("Open Account").size(12))
                        .padding([5, 12])
                        .on_press(Message::Navigate(Route::Account))
                        .style(pill_button_style(false)),
                );
            }

            meta_col = meta_col.push(
                column![
                    text(format!(
                        "Could not load tag metadata: {}",
                        err.user_message()
                    ))
                    .size(13)
                    .color(palette::DANGER),
                    actions,
                ]
                .spacing(6),
            );
        }
    }

    meta_col = meta_col.push(
        row![
            text(ProfileTagUnsupportedAction::TagDirectory.explanation())
                .size(12)
                .color(palette::TEXT_SECONDARY),
            button(text(ProfileTagUnsupportedAction::TagDirectory.label()).size(12))
                .padding([5, 12])
                .on_press(Message::TagOpenDirectoryHandOff)
                .style(pill_button_style(false)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .wrap()
        .vertical_spacing(6),
    );

    let meta_card = container(meta_col)
        .padding(12)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0));

    let filter_bar = gallery_filter_bar(GalleryFilterBarProps {
        purity: state.controller.purity(),
        selectable_purity: state.selectable_purity,
        sorting: state.controller.sorting(),
        order: state.controller.order(),
        top_range: state.controller.top_range(),
        on_toggle_purity: Message::TagTogglePurity,
        on_select_sorting: Message::TagSelectSorting,
        on_select_order: Message::TagSelectOrder,
        on_select_top_range: Message::TagSelectTopRange,
    });

    let grid = gallery_grid::view(GalleryGridView {
        kind: GalleryKind::Tag,
        scrollable_id: state.scrollable_id,
        metrics: state.metrics,
        items: state.controller.items(),
        status: state.controller.items_status(),
        is_loading: state.controller.is_loading_items(),
        total: state.controller.total(),
        selected: state.selected,
        empty_message: "No wallpapers found for this tag and purity.",
        show_account_shortcut_on_unauthorized: state.is_guest,
        thumbnail_state: state.thumbnail_state,
    });

    column![
        top_bar.wrap().vertical_spacing(8),
        meta_card,
        filter_bar,
        grid,
    ]
    .spacing(12)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
