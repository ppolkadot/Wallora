use iced::widget::{column, text};
use iced::{Element, Length};

use crate::message::Message;
use crate::theme::palette;

pub fn view<'a>() -> Element<'a, Message> {
    column![
        text("Home").size(28).color(palette::TEXT_PRIMARY),
        text("Wallora — An unofficial native Wallhaven client for Windows.")
            .size(14)
            .color(palette::TEXT_SECONDARY),
        text(
            "Discovery sections will appear here once provider integration and gallery views land."
        )
        .size(14)
        .color(palette::MUTED),
    ]
    .spacing(12)
    .width(Length::Fill)
    .into()
}
