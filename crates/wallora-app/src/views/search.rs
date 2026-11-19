use iced::widget::{column, text};
use iced::{Element, Length};
use wallora_core::domain::{BrowseDefaults, Sorting};

use crate::message::Message;
use crate::theme::palette;

pub fn view<'a>(defaults: &BrowseDefaults) -> Element<'a, Message> {
    let toplist_defaults = defaults.to_search_query(Sorting::Toplist);
    column![
        text("Search").size(28).color(palette::TEXT_PRIMARY),
        text("Search query builder, filters, and virtualized results will appear in Task 5.")
            .size(14)
            .color(palette::TEXT_SECONDARY),
        text(format!(
            "Active defaults — Categories: {} | Purity: {} | Page size: {} | Toplist: {}",
            defaults.categories.to_param(),
            defaults.purity.to_param(),
            defaults.per_page,
            toplist_defaults
                .top_range
                .map_or("", |range| range.as_param())
        ))
        .size(13)
        .color(palette::MUTED),
    ]
    .spacing(12)
    .width(Length::Fill)
    .into()
}
