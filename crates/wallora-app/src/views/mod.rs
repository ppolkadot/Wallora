pub mod account;
pub mod collections;
pub mod detail;
pub mod home;
pub mod search;
pub mod settings;
pub mod tag;
pub mod uploader;

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Element, Length};
use wallora_core::domain::{Purity, PurityMask, SortOrder, Sorting, TopRange};

use crate::message::Message;
use crate::theme::{card_style, palette, pill_button_style};
use crate::views::home::ALL_TOP_RANGES;
use crate::views::search::{ALL_SORTINGS, sorting_label};

pub(crate) struct GalleryFilterBarProps {
    pub purity: PurityMask,
    pub selectable_purity: PurityMask,
    pub sorting: Sorting,
    pub order: Option<SortOrder>,
    pub top_range: TopRange,
    pub on_toggle_purity: fn(Purity) -> Message,
    pub on_select_sorting: fn(Sorting) -> Message,
    pub on_select_order: fn(SortOrder) -> Message,
    pub on_select_top_range: fn(TopRange) -> Message,
}

pub(crate) fn gallery_filter_bar<'a>(props: GalleryFilterBarProps) -> Element<'a, Message> {
    let nsfw_label = if props.selectable_purity.nsfw {
        "NSFW"
    } else {
        "NSFW (key)"
    };
    let mut purity_row = row![
        text("Purity:").size(13).color(palette::TEXT_SECONDARY),
        button(text("SFW").size(12))
            .padding([5, 10])
            .on_press((props.on_toggle_purity)(Purity::Sfw))
            .style(pill_button_style(props.purity.sfw)),
        button(text("Sketchy").size(12))
            .padding([5, 10])
            .on_press((props.on_toggle_purity)(Purity::Sketchy))
            .style(pill_button_style(props.purity.sketchy)),
        button(text(nsfw_label).size(12))
            .padding([5, 10])
            .on_press((props.on_toggle_purity)(Purity::Nsfw))
            .style(pill_button_style(props.purity.nsfw)),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    if props.purity.nsfw && !props.selectable_purity.nsfw {
        purity_row = purity_row.push(
            text("Upstream enforces NSFW access for this session.")
                .size(12)
                .color(palette::WARNING),
        );
    }

    let sorting_row = ALL_SORTINGS
        .into_iter()
        .fold(
            row![text("Sort:").size(13).color(palette::TEXT_SECONDARY)].spacing(6),
            |r, option| {
                r.push(
                    button(text(sorting_label(option)).size(12))
                        .padding([5, 10])
                        .on_press((props.on_select_sorting)(option))
                        .style(pill_button_style(props.sorting == option)),
                )
            },
        )
        .push(text("Order:").size(13).color(palette::TEXT_SECONDARY))
        .push(
            button(text("Desc").size(12))
                .padding([5, 10])
                .on_press((props.on_select_order)(SortOrder::Desc))
                .style(pill_button_style(
                    props.order.is_none_or(|order| order == SortOrder::Desc),
                )),
        )
        .push(
            button(text("Asc").size(12))
                .padding([5, 10])
                .on_press((props.on_select_order)(SortOrder::Asc))
                .style(pill_button_style(props.order == Some(SortOrder::Asc))),
        )
        .align_y(Alignment::Center);

    let mut col = column![
        purity_row.wrap().vertical_spacing(6),
        sorting_row.wrap().vertical_spacing(6),
    ]
    .spacing(8);

    if props.sorting == Sorting::Toplist {
        let range_row = ALL_TOP_RANGES
            .into_iter()
            .fold(
                row![
                    text("Toplist range:")
                        .size(13)
                        .color(palette::TEXT_SECONDARY)
                ]
                .spacing(6),
                |r, option| {
                    r.push(
                        button(text(option.as_param()).size(12))
                            .padding([5, 10])
                            .on_press((props.on_select_top_range)(option))
                            .style(pill_button_style(option == props.top_range)),
                    )
                },
            )
            .align_y(Alignment::Center);
        col = col.push(range_row.wrap().vertical_spacing(6));
    }

    container(col)
        .padding(12)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0))
        .into()
}
