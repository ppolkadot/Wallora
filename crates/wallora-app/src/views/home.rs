use iced::widget::{Id, button, container, row, text};
use iced::{Alignment, Element, Length, widget::column};
use wallora_core::domain::{TopRange, Wallpaper};
use wallora_core::services::{
    DiscoveryController, DiscoveryView, SiteGapAction, UnsupportedFeature,
};

use crate::message::{GalleryKind, Message};
use crate::theme::{card_style, palette, pill_button_style};
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
    pub site_gaps_expanded: bool,
    pub thumbnail_state: F,
}

fn site_gap_card<'a>(
    title: &'a str,
    explanation: &'a str,
    button_label: &'a str,
    on_press: Message,
) -> Element<'a, Message> {
    container(
        column![
            text(title).size(13).color(palette::TEXT_PRIMARY),
            text(explanation).size(11).color(palette::TEXT_SECONDARY),
            button(text(button_label).size(12))
                .padding([5, 10])
                .on_press(on_press)
                .style(pill_button_style(false)),
        ]
        .spacing(6),
    )
    .padding(10)
    .width(Length::Fixed(290.0))
    .style(card_style(palette::MUTED, 1.0))
    .into()
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

    let gaps_summary = if state.site_gaps_expanded {
        "Website-only features with no public API route; open them in your browser."
    } else {
        "Hot · Homepage · Tag Directory · Few Tags / Untagged · Upload · Forums"
    };
    let toggle_label = if state.site_gaps_expanded {
        "Hide links"
    } else {
        "Show links (6)"
    };
    let mut gaps_col = column![
        row![
            text("More on wallhaven.cc (API gaps)")
                .size(12)
                .color(palette::TEXT_PRIMARY),
            text(gaps_summary)
                .size(12)
                .color(palette::TEXT_SECONDARY)
                .width(Length::Fill),
            button(text(toggle_label).size(12))
                .padding([4, 10])
                .on_press(Message::HomeToggleSiteGaps)
                .style(pill_button_style(state.site_gaps_expanded)),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    ]
    .spacing(10);

    if state.site_gaps_expanded {
        let hot_handoff = UnsupportedFeature::HotSorting.hand_off();
        let mut cards = row![site_gap_card(
            UnsupportedFeature::HotSorting.title(),
            UnsupportedFeature::HotSorting.explanation(),
            hot_handoff.label,
            Message::OpenUnsupportedFeature(UnsupportedFeature::HotSorting),
        )]
        .spacing(10);

        for action in SiteGapAction::ALL {
            cards = cards.push(site_gap_card(
                action.title(),
                action.explanation(),
                action.label(),
                Message::OpenSiteGap(action),
            ));
        }

        gaps_col = gaps_col.push(cards.wrap().vertical_spacing(8));
    }

    let gaps_card = container(gaps_col)
        .padding([8, 12])
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0));

    column![header, grid, gaps_card]
        .spacing(14)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
