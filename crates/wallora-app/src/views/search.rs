use iced::widget::{Id, button, column, container, row, text, text_input};
use iced::{Alignment, Element, Length};
use wallora_core::domain::{
    AspectRatio, Category, Dimensions, Purity, PurityMask, SortOrder, Sorting, TopRange, Wallpaper,
};
use wallora_core::services::{DiscoveryController, UnsupportedFeature};

use crate::message::{GalleryKind, Message};
use crate::theme::{card_style, palette, pill_button_style};
use crate::views::home::ALL_TOP_RANGES;
use crate::widgets::gallery_grid::{self, GalleryGridView, GridMetrics};
use crate::widgets::thumbnail::ThumbnailViewState;

pub const ALL_SORTINGS: [Sorting; 6] = [
    Sorting::DateAdded,
    Sorting::Relevance,
    Sorting::Random,
    Sorting::Views,
    Sorting::Favorites,
    Sorting::Toplist,
];

#[derive(Debug, Clone)]
pub struct SearchInputIds {
    pub query: Id,
    pub atleast: Id,
    pub resolutions: Id,
    pub ratios: Id,
    pub colors: Id,
}

impl Default for SearchInputIds {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchInputIds {
    pub fn new() -> Self {
        Self {
            query: Id::unique(),
            atleast: Id::unique(),
            resolutions: Id::unique(),
            ratios: Id::unique(),
            colors: Id::unique(),
        }
    }

    pub fn all(&self) -> [Id; 5] {
        [
            self.query.clone(),
            self.atleast.clone(),
            self.resolutions.clone(),
            self.ratios.clone(),
            self.colors.clone(),
        ]
    }
}

pub fn parse_atleast_input(raw: &str) -> Result<Option<Dimensions>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Dimensions::parse_wxh(trimmed)
        .map(Some)
        .ok_or_else(|| "At least resolution must look like 1920x1080.".to_string())
}

pub fn parse_resolutions_input(raw: &str) -> Result<Vec<Dimensions>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::new();
    for segment in trimmed.split(',') {
        let segment = segment.trim();
        let dims = Dimensions::parse_wxh(segment).ok_or_else(|| {
            "Resolutions must be comma-separated WxH values, e.g. 1920x1080,2560x1440.".to_string()
        })?;
        parsed.push(dims);
    }
    Ok(parsed)
}

pub fn parse_ratios_input(raw: &str) -> Result<Vec<AspectRatio>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::new();
    for segment in trimmed.split(',') {
        let segment = segment.trim();
        if segment.eq_ignore_ascii_case("landscape") {
            parsed.push(AspectRatio::new("landscape"));
        } else if segment.eq_ignore_ascii_case("portrait") {
            parsed.push(AspectRatio::new("portrait"));
        } else if let Some(dims) = Dimensions::parse_wxh(segment) {
            parsed.push(AspectRatio::new(dims.to_string()));
        } else {
            return Err(
                "Ratios must be comma-separated WxH ratios (e.g. 16x9,21x9), landscape, or portrait."
                    .to_string(),
            );
        }
    }
    Ok(parsed)
}

pub fn parse_color_input(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(Some(hex.to_ascii_lowercase()))
    } else {
        Err("Color must be a 6-digit hex code like 336699 or #336699.".to_string())
    }
}

pub fn format_resolutions(resolutions: &[Dimensions]) -> String {
    resolutions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn format_ratios(ratios: &[AspectRatio]) -> String {
    ratios
        .iter()
        .map(AspectRatio::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn sorting_label(sorting: Sorting) -> &'static str {
    match sorting {
        Sorting::DateAdded => "Date added",
        Sorting::Relevance => "Relevance",
        Sorting::Random => "Random",
        Sorting::Views => "Views",
        Sorting::Favorites => "Favorites",
        Sorting::Toplist => "Toplist",
    }
}

pub struct SearchViewState<'a, F>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    pub query_input: &'a str,
    pub atleast_input: &'a str,
    pub resolutions_input: &'a str,
    pub ratios_input: &'a str,
    pub colors_input: &'a str,
    pub filter_error: Option<&'a str>,
    pub filters_expanded: bool,
    pub input_ids: &'a SearchInputIds,
    pub selectable_purity: PurityMask,
    pub is_guest: bool,
    pub controller: &'a DiscoveryController,
    pub scrollable_id: &'a Id,
    pub metrics: &'a GridMetrics,
    pub selected: Option<usize>,
    pub thumbnail_state: F,
}

pub fn view<'a, F>(state: SearchViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let filters = state.controller.filters();

    let mut search_bar = row![
        text("Search").size(24).color(palette::TEXT_PRIMARY),
        text_input(
            "Search tags, -exclude, +require, @user, id:N, type:png, like:ID",
            state.query_input
        )
        .id(state.input_ids.query.clone())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchSubmit)
        .padding(8)
        .width(Length::Fill),
        button(text("Search").size(13))
            .padding([8, 16])
            .on_press(Message::SearchSubmit),
        button(text("Clear").size(13))
            .padding([8, 14])
            .on_press(Message::SearchClear),
        button(
            text(if state.filters_expanded {
                "Hide filters"
            } else {
                "Show filters"
            })
            .size(13)
        )
        .padding([8, 14])
        .on_press(Message::SearchToggleFiltersPanel),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    if filters.sorting == Some(Sorting::Random) {
        search_bar = search_bar.push(
            button(text("New Random").size(13))
                .padding([8, 14])
                .on_press(Message::GalleryRefresh(GalleryKind::Search)),
        );
    }

    let mut top = column![search_bar].spacing(10);

    if let Some(total) = state.controller.total() {
        let mut status_row = row![
            text(format!("{total} matching wallpapers"))
                .size(12)
                .color(palette::TEXT_SECONDARY),
        ]
        .spacing(14)
        .align_y(Alignment::Center);

        if filters.sorting == Some(Sorting::Random)
            && let Some(seed) = state.controller.seed()
        {
            status_row =
                status_row.push(text(format!("Seed: {seed}")).size(12).color(palette::MUTED));
        }
        top = top.push(status_row);
    }

    if state.filters_expanded {
        let category_row = row![
            text("Categories:").size(13).color(palette::TEXT_SECONDARY),
            button(text("General").size(12))
                .padding([5, 10])
                .on_press(Message::SearchToggleCategory(Category::General))
                .style(pill_button_style(filters.categories.general)),
            button(text("Anime").size(12))
                .padding([5, 10])
                .on_press(Message::SearchToggleCategory(Category::Anime))
                .style(pill_button_style(filters.categories.anime)),
            button(text("People").size(12))
                .padding([5, 10])
                .on_press(Message::SearchToggleCategory(Category::People))
                .style(pill_button_style(filters.categories.people)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let nsfw_label = if state.selectable_purity.nsfw {
            "NSFW"
        } else {
            "NSFW (key)"
        };
        let mut purity_row = row![
            text("Purity:").size(13).color(palette::TEXT_SECONDARY),
            button(text("SFW").size(12))
                .padding([5, 10])
                .on_press(Message::SearchTogglePurity(Purity::Sfw))
                .style(pill_button_style(filters.purity.sfw)),
            button(text("Sketchy").size(12))
                .padding([5, 10])
                .on_press(Message::SearchTogglePurity(Purity::Sketchy))
                .style(pill_button_style(filters.purity.sketchy)),
            button(text(nsfw_label).size(12))
                .padding([5, 10])
                .on_press(Message::SearchTogglePurity(Purity::Nsfw))
                .style(pill_button_style(filters.purity.nsfw)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        if filters.purity.nsfw && !state.selectable_purity.nsfw {
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
                    let active = filters.sorting == Some(option);
                    r.push(
                        button(text(sorting_label(option)).size(12))
                            .padding([5, 10])
                            .on_press(Message::SearchSelectSorting(option))
                            .style(pill_button_style(active)),
                    )
                },
            )
            .push(text("Order:").size(13).color(palette::TEXT_SECONDARY))
            .push(
                button(text("Desc").size(12))
                    .padding([5, 10])
                    .on_press(Message::SearchSelectOrder(SortOrder::Desc))
                    .style(pill_button_style(
                        filters.order.is_none_or(|order| order == SortOrder::Desc),
                    )),
            )
            .push(
                button(text("Asc").size(12))
                    .padding([5, 10])
                    .on_press(Message::SearchSelectOrder(SortOrder::Asc))
                    .style(pill_button_style(filters.order == Some(SortOrder::Asc))),
            )
            .align_y(Alignment::Center);

        let mut filter_col =
            column![row![category_row, purity_row].spacing(20), sorting_row].spacing(8);

        if filters.sorting == Some(Sorting::Toplist) {
            let active_range = filters.top_range.unwrap_or(TopRange::OneMonth);
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
                                .on_press(Message::SearchSelectTopRange(option))
                                .style(pill_button_style(option == active_range)),
                        )
                    },
                )
                .align_y(Alignment::Center);
            filter_col = filter_col.push(range_row);
        }

        let inputs_row = row![
            text("At least:").size(12).color(palette::TEXT_SECONDARY),
            text_input("1920x1080", state.atleast_input)
                .id(state.input_ids.atleast.clone())
                .on_input(Message::SearchAtleastChanged)
                .on_submit(Message::SearchSubmit)
                .padding(6)
                .width(Length::Fixed(110.0)),
            text("Resolutions:").size(12).color(palette::TEXT_SECONDARY),
            text_input("1920x1080,2560x1440", state.resolutions_input)
                .id(state.input_ids.resolutions.clone())
                .on_input(Message::SearchResolutionsChanged)
                .on_submit(Message::SearchSubmit)
                .padding(6)
                .width(Length::FillPortion(2)),
            text("Ratios:").size(12).color(palette::TEXT_SECONDARY),
            text_input("16x9,21x9", state.ratios_input)
                .id(state.input_ids.ratios.clone())
                .on_input(Message::SearchRatiosChanged)
                .on_submit(Message::SearchSubmit)
                .padding(6)
                .width(Length::FillPortion(1)),
            text("Color:").size(12).color(palette::TEXT_SECONDARY),
            text_input("#336699", state.colors_input)
                .id(state.input_ids.colors.clone())
                .on_input(Message::SearchColorsChanged)
                .on_submit(Message::SearchSubmit)
                .padding(6)
                .width(Length::Fixed(92.0)),
            button(text("Apply").size(12))
                .padding([6, 12])
                .on_press(Message::SearchSubmit),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        filter_col = filter_col.push(inputs_row);

        if let Some(error) = state.filter_error {
            filter_col = filter_col.push(text(error).size(12).color(palette::DANGER));
        }

        let ai_handoff = UnsupportedFeature::AiArtFilter.hand_off();
        filter_col = filter_col.push(
            row![
                text(UnsupportedFeature::AiArtFilter.explanation())
                    .size(12)
                    .color(palette::TEXT_SECONDARY),
                button(text(ai_handoff.label).size(12))
                    .padding([5, 12])
                    .on_press(Message::OpenUnsupportedFeature(
                        UnsupportedFeature::AiArtFilter
                    )),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        );

        top = top.push(
            container(filter_col)
                .padding(12)
                .width(Length::Fill)
                .style(card_style(palette::MUTED, 1.0)),
        );
    }

    let grid = gallery_grid::view(GalleryGridView {
        kind: GalleryKind::Search,
        scrollable_id: state.scrollable_id,
        metrics: state.metrics,
        items: state.controller.items(),
        status: state.controller.status(),
        is_loading: state.controller.is_loading(),
        total: state.controller.total(),
        selected: state.selected,
        empty_message: "No wallpapers matched your query and filters.",
        show_account_shortcut_on_unauthorized: state.is_guest,
        thumbnail_state: state.thumbnail_state,
    });

    column![top, grid]
        .spacing(14)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atleast_parser_accepts_blank_and_wxh_and_rejects_bad_values() {
        assert_eq!(parse_atleast_input(""), Ok(None));
        assert_eq!(parse_atleast_input("   "), Ok(None));
        assert_eq!(
            parse_atleast_input(" 1920x1080 "),
            Ok(Some(Dimensions::new(1920, 1080)))
        );
        assert!(parse_atleast_input("1920").is_err());
        assert!(parse_atleast_input("0x1080").is_err());
    }

    #[test]
    fn resolutions_parser_and_formatter_round_trip() {
        let parsed = parse_resolutions_input("1920x1080, 2560x1440").unwrap();
        assert_eq!(
            parsed,
            vec![Dimensions::new(1920, 1080), Dimensions::new(2560, 1440)]
        );
        assert_eq!(format_resolutions(&parsed), "1920x1080, 2560x1440");
        assert_eq!(
            parse_resolutions_input("").unwrap(),
            Vec::<Dimensions>::new()
        );
        assert!(parse_resolutions_input("1920x1080,").is_err());
    }

    #[test]
    fn ratios_parser_handles_numeric_and_named_ratios() {
        let parsed = parse_ratios_input("16x9, Landscape, portrait").unwrap();
        assert_eq!(
            parsed,
            vec![
                AspectRatio::new("16x9"),
                AspectRatio::new("landscape"),
                AspectRatio::new("portrait"),
            ]
        );
        assert_eq!(format_ratios(&parsed), "16x9, landscape, portrait");
        assert!(parse_ratios_input("square").is_err());
    }

    #[test]
    fn color_parser_normalizes_six_digit_hex_and_rejects_invalid_values() {
        assert_eq!(parse_color_input(""), Ok(None));
        assert_eq!(parse_color_input("#3366AA"), Ok(Some("3366aa".to_string())));
        assert_eq!(
            parse_color_input(" ff00aa "),
            Ok(Some("ff00aa".to_string()))
        );
        assert!(parse_color_input("3366a").is_err());
        assert!(parse_color_input("#zzzzzz").is_err());
    }
}
