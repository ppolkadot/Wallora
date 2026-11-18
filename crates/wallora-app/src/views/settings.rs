use iced::widget::{column, text};
use iced::{Element, Length};
use wallora_core::storage::AppPaths;

use crate::message::Message;
use crate::theme::palette;

pub fn view<'a>(paths: &AppPaths) -> Element<'a, Message> {
    let config_path = paths.config_dir.display().to_string();
    let cache_path = paths.cache_dir.display().to_string();
    let logs_path = paths.logs_dir.display().to_string();

    column![
        text("Settings").size(28).color(palette::TEXT_PRIMARY),
        text("Application directories & diagnostics")
            .size(15)
            .color(palette::TEXT_SECONDARY),
        text(format!("Config: {config_path}"))
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
    .spacing(10)
    .width(Length::Fill)
    .into()
}
