use iced::widget::{column, text};
use iced::{Element, Length};

use crate::message::Message;
use crate::theme::palette;

pub fn view<'a>() -> Element<'a, Message> {
    column![
        text("Search").size(28).color(palette::TEXT_PRIMARY),
        text("Search query builder, filters, and virtualized results will appear here.")
            .size(14)
            .color(palette::TEXT_SECONDARY),
    ]
    .spacing(12)
    .width(Length::Fill)
    .into()
}
