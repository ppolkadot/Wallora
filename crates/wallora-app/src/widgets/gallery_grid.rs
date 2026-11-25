use std::ops::Range;

use iced::keyboard::{Key, key::Named};
use iced::widget::{Id, Space, button, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length};
use wallora_core::domain::Wallpaper;
use wallora_core::provider::ProviderError;
use wallora_core::services::DiscoveryStatus;

use crate::message::{GalleryKind, Message, Route};
use crate::theme::{card_style, palette};
use crate::widgets::thumbnail::{
    self, TILE_CARD_H, TILE_W, ThumbnailCardProps, ThumbnailViewState,
};

pub const TILE_GAP: f32 = 12.0;
pub const GRID_PADDING: f32 = 8.0;
pub const SHELL_HORIZONTAL_PADDING: f32 = 48.0;
pub const SCROLLBAR_RESERVE: f32 = 12.0;
pub const OVERSCAN_ROWS: usize = 1;
pub const INFINITE_SCROLL_ROWS_THRESHOLD: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridKey {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

impl GridKey {
    pub fn from_iced_key(key: &Key) -> Option<Self> {
        match key {
            Key::Named(Named::ArrowLeft) => Some(Self::Left),
            Key::Named(Named::ArrowRight) => Some(Self::Right),
            Key::Named(Named::ArrowUp) => Some(Self::Up),
            Key::Named(Named::ArrowDown) => Some(Self::Down),
            Key::Named(Named::PageUp) => Some(Self::PageUp),
            Key::Named(Named::PageDown) => Some(Self::PageDown),
            Key::Named(Named::Home) => Some(Self::Home),
            Key::Named(Named::End) => Some(Self::End),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GridMetrics {
    pub window_width: f32,
    pub viewport_width: Option<f32>,
    pub viewport_height: f32,
    pub scroll_y: f32,
}

impl Default for GridMetrics {
    fn default() -> Self {
        Self {
            window_width: 1280.0,
            viewport_width: None,
            viewport_height: 580.0,
            scroll_y: 0.0,
        }
    }
}

impl GridMetrics {
    pub fn set_window_size(&mut self, width: f32, height: f32) {
        self.window_width = width;
        self.viewport_width = None;
        self.viewport_height = (height - 220.0).max(240.0);
    }

    pub fn update_scroll(&mut self, offset_y: f32, viewport_width: f32, viewport_height: f32) {
        self.scroll_y = offset_y.max(0.0);
        if viewport_width > 0.0 {
            self.viewport_width = Some(viewport_width);
        }
        if viewport_height > 0.0 {
            self.viewport_height = viewport_height;
        }
    }

    pub fn columns(&self) -> usize {
        let avail = self
            .viewport_width
            .unwrap_or_else(|| (self.window_width - SHELL_HORIZONTAL_PADDING).max(TILE_W));
        let usable = (avail - GRID_PADDING * 2.0 - SCROLLBAR_RESERVE).max(TILE_W);
        ((usable + TILE_GAP) / (TILE_W + TILE_GAP)).floor().max(1.0) as usize
    }

    pub fn row_height(&self) -> f32 {
        TILE_CARD_H + TILE_GAP
    }

    pub fn total_rows(&self, item_count: usize) -> usize {
        if item_count == 0 {
            return 0;
        }
        item_count.div_ceil(self.columns())
    }

    pub fn total_content_height(&self, item_count: usize) -> f32 {
        let rows = self.total_rows(item_count);
        if rows == 0 {
            return 0.0;
        }
        GRID_PADDING * 2.0 + rows as f32 * self.row_height() - TILE_GAP
    }

    /// Inclusive visible row range with [`OVERSCAN_ROWS`] padding above and below.
    pub fn visible_row_range(&self, item_count: usize) -> (usize, usize) {
        let total = self.total_rows(item_count);
        if total == 0 {
            return (0, 0);
        }
        let rh = self.row_height();
        let grid_scroll = (self.scroll_y - GRID_PADDING).max(0.0);
        let first = ((grid_scroll / rh).floor().max(0.0) as usize).saturating_sub(OVERSCAN_ROWS);
        let last_visible =
            (((grid_scroll + self.viewport_height - 1.0).max(0.0) / rh).floor()) as usize;
        let last = (last_visible + OVERSCAN_ROWS).min(total.saturating_sub(1));
        (first.min(last), last)
    }

    pub fn visible_item_range(&self, item_count: usize) -> Range<usize> {
        if item_count == 0 {
            return 0..0;
        }
        let cols = self.columns();
        let (first_row, last_row) = self.visible_row_range(item_count);
        let start = first_row.saturating_mul(cols).min(item_count);
        let end = (last_row + 1).saturating_mul(cols).min(item_count);
        start..end
    }

    pub fn is_near_bottom(&self, item_count: usize, content_height: Option<f32>) -> bool {
        if item_count == 0 {
            return false;
        }
        let total_height = content_height.unwrap_or_else(|| self.total_content_height(item_count));
        if total_height <= 0.0 {
            return false;
        }
        let remaining = total_height - (self.scroll_y + self.viewport_height);
        remaining <= self.row_height() * INFINITE_SCROLL_ROWS_THRESHOLD
    }

    pub fn page_jump_rows(&self) -> usize {
        ((self.viewport_height / self.row_height()).floor() as usize).max(1)
    }

    pub fn move_selection(
        &self,
        current: Option<usize>,
        item_count: usize,
        key: GridKey,
    ) -> Option<usize> {
        if item_count == 0 {
            return None;
        }
        let last = item_count - 1;
        let Some(curr) = current else {
            return Some(match key {
                GridKey::End => last,
                _ => 0,
            });
        };
        let curr = curr.min(last);
        let cols = self.columns();
        let page_step = cols.saturating_mul(self.page_jump_rows());
        let next = match key {
            GridKey::Left => curr.saturating_sub(1),
            GridKey::Right => (curr + 1).min(last),
            GridKey::Up => curr.saturating_sub(cols),
            GridKey::Down => (curr + cols).min(last),
            GridKey::PageUp => curr.saturating_sub(page_step),
            GridKey::PageDown => (curr + page_step).min(last),
            GridKey::Home => 0,
            GridKey::End => last,
        };
        Some(next)
    }

    pub fn scroll_target_for(&self, selected: usize) -> Option<f32> {
        let cols = self.columns();
        let row_idx = selected / cols;
        let rh = self.row_height();
        let top = if row_idx == 0 {
            0.0
        } else {
            GRID_PADDING + row_idx as f32 * rh
        };
        let bottom = GRID_PADDING + row_idx as f32 * rh + TILE_CARD_H;
        if top < self.scroll_y {
            Some(top)
        } else if bottom > self.scroll_y + self.viewport_height {
            Some((bottom - self.viewport_height).max(0.0))
        } else {
            None
        }
    }
}

pub struct GalleryGridView<'a, F>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    pub kind: GalleryKind,
    pub scrollable_id: &'a Id,
    pub metrics: &'a GridMetrics,
    pub items: &'a [Wallpaper],
    pub status: &'a DiscoveryStatus,
    pub is_loading: bool,
    pub total: Option<u64>,
    pub selected: Option<usize>,
    pub empty_message: &'a str,
    pub show_account_shortcut_on_unauthorized: bool,
    pub thumbnail_state: F,
}

pub fn view<'a, F>(state: GalleryGridView<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    if state.items.is_empty() {
        return empty_or_error_state(&state);
    }

    let cols = state.metrics.columns();
    let total_rows = state.metrics.total_rows(state.items.len());
    let (first_row, last_row) = state.metrics.visible_row_range(state.items.len());
    let rh = state.metrics.row_height();

    let mut grid_rows = column![].spacing(TILE_GAP);
    if first_row > 0 {
        let top_height = (first_row as f32 * rh - TILE_GAP).max(0.0);
        grid_rows = grid_rows.push(Space::new().height(top_height));
    }

    for r in first_row..=last_row {
        let mut r_row = row![].spacing(TILE_GAP);
        for c in 0..cols {
            let idx = r * cols + c;
            if idx >= state.items.len() {
                break;
            }
            let wallpaper = &state.items[idx];
            let thumb_state = (state.thumbnail_state)(wallpaper);
            r_row = r_row.push(thumbnail::view(ThumbnailCardProps {
                kind: state.kind,
                index: idx,
                wallpaper,
                state: thumb_state,
                selected: state.selected == Some(idx),
            }));
        }
        grid_rows = grid_rows.push(r_row);
    }

    let remaining_rows = total_rows.saturating_sub(last_row + 1);
    if remaining_rows > 0 {
        let bottom_height = (remaining_rows as f32 * rh - TILE_GAP).max(0.0);
        grid_rows = grid_rows.push(Space::new().height(bottom_height));
    }

    let footer: Element<'a, Message> = match state.status {
        DiscoveryStatus::Failed { error, .. } => container(
            row![
                text(error.user_message()).size(13).color(palette::DANGER),
                button(text("Retry").size(13))
                    .padding([6, 14])
                    .on_press(Message::GalleryRetry(state.kind)),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([12, 0])
        .into(),
        _ if state.is_loading => container(
            text("Loading more wallpapers...")
                .size(13)
                .color(palette::TEXT_SECONDARY),
        )
        .padding([12, 0])
        .into(),
        _ => {
            let summary = match state.total {
                Some(total) => format!("Showing {} of {} wallpapers", state.items.len(), total),
                None => format!("Showing {} wallpapers", state.items.len()),
            };
            container(text(summary).size(12).color(palette::MUTED))
                .padding([12, 0])
                .into()
        }
    };
    grid_rows = grid_rows.push(footer);

    let kind = state.kind;
    scrollable(container(grid_rows).padding(GRID_PADDING))
        .id(state.scrollable_id.clone())
        .width(Length::Fill)
        .height(Length::Fill)
        .on_scroll(move |vp| Message::GalleryScrolled {
            kind,
            offset_y: vp.absolute_offset().y,
            viewport_width: vp.bounds().width,
            viewport_height: vp.bounds().height,
            content_height: vp.content_bounds().height,
        })
        .into()
}

fn empty_or_error_state<'a, F>(state: &GalleryGridView<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let inner: Element<'a, Message> = match state.status {
        DiscoveryStatus::Idle | DiscoveryStatus::Loading => container(
            text("Loading wallpapers...")
                .size(15)
                .color(palette::TEXT_SECONDARY),
        )
        .padding(24)
        .style(card_style(palette::MUTED, 1.0))
        .into(),
        DiscoveryStatus::Empty => container(
            column![
                text(state.empty_message)
                    .size(15)
                    .color(palette::TEXT_PRIMARY),
                button(text("Refresh").size(13))
                    .padding([8, 16])
                    .on_press(Message::GalleryRefresh(state.kind)),
            ]
            .spacing(12)
            .align_x(Alignment::Center),
        )
        .padding(24)
        .style(card_style(palette::MUTED, 1.0))
        .into(),
        DiscoveryStatus::Failed { error, .. } => {
            let mut actions = row![
                button(text("Retry").size(13))
                    .padding([8, 16])
                    .on_press(Message::GalleryRetry(state.kind)),
            ]
            .spacing(10)
            .align_y(Alignment::Center);

            if *error == ProviderError::Unauthorized && state.show_account_shortcut_on_unauthorized
            {
                actions = actions.push(
                    button(text("Open Account").size(13))
                        .padding([8, 16])
                        .on_press(Message::Navigate(Route::Account)),
                );
            }

            container(
                column![
                    text("Could not load wallpapers")
                        .size(16)
                        .color(palette::DANGER),
                    text(error.user_message())
                        .size(13)
                        .color(palette::TEXT_SECONDARY),
                    actions,
                ]
                .spacing(10)
                .align_x(Alignment::Center),
            )
            .padding(24)
            .style(card_style(palette::DANGER, 1.0))
            .into()
        }
        DiscoveryStatus::Ready => container(
            text("No wallpapers loaded.")
                .size(14)
                .color(palette::TEXT_SECONDARY),
        )
        .padding(24)
        .style(card_style(palette::MUTED, 1.0))
        .into(),
    };

    container(inner)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_adapt_to_viewport_and_window_width() {
        let mut m = GridMetrics::default();
        // 1280 - 48 shell - 16 grid - 12 scrollbar = 1204 -> 4 columns of 264 + 12 gap
        assert_eq!(m.columns(), 4);

        m.set_window_size(600.0, 800.0);
        assert_eq!(m.columns(), 1);

        m.update_scroll(0.0, 1450.0, 700.0);
        assert_eq!(m.columns(), 5);
    }

    #[test]
    fn visible_row_and_item_ranges_include_overscan_only() {
        let m = GridMetrics {
            window_width: 1280.0,
            viewport_width: Some(1200.0),
            viewport_height: 450.0,
            scroll_y: 0.0,
        };
        assert_eq!(m.columns(), 4);
        // Row height = 212 + 12 = 224. Viewport 450 covers rows 0..=2 + overscan 1 => 0..=3.
        assert_eq!(m.visible_row_range(40), (0, 3));
        assert_eq!(m.visible_item_range(40), 0..16);

        let scrolled = GridMetrics {
            scroll_y: 1200.0,
            ..m.clone()
        };
        let (first, last) = scrolled.visible_row_range(40);
        assert!(first > 1);
        assert!(last < 9);
        assert_eq!(
            scrolled.visible_item_range(40),
            (first * 4)..((last + 1) * 4)
        );
    }

    #[test]
    fn near_bottom_triggers_within_threshold_rows() {
        let m = GridMetrics {
            window_width: 1280.0,
            viewport_width: Some(1200.0),
            viewport_height: 500.0,
            scroll_y: 0.0,
        };
        // 40 items @ 4 cols = 10 rows => total height ~ 2244px.
        assert!(!m.is_near_bottom(40, None));

        let near = GridMetrics {
            scroll_y: m.total_content_height(40) - m.viewport_height - 100.0,
            ..m.clone()
        };
        assert!(near.is_near_bottom(40, None));
        assert!(!near.is_near_bottom(0, None));
    }

    #[test]
    fn move_selection_handles_all_navigation_keys_and_scroll_targets() {
        let m = GridMetrics {
            window_width: 1280.0,
            viewport_width: Some(1200.0),
            viewport_height: 450.0,
            scroll_y: 0.0,
        };
        assert_eq!(m.columns(), 4);
        assert_eq!(m.move_selection(None, 20, GridKey::Down), Some(0));
        assert_eq!(m.move_selection(None, 20, GridKey::End), Some(19));
        assert_eq!(m.move_selection(None, 0, GridKey::Right), None);

        assert_eq!(m.move_selection(Some(0), 20, GridKey::Right), Some(1));
        assert_eq!(m.move_selection(Some(1), 20, GridKey::Left), Some(0));
        assert_eq!(m.move_selection(Some(0), 20, GridKey::Down), Some(4));
        assert_eq!(m.move_selection(Some(4), 20, GridKey::Up), Some(0));
        assert_eq!(m.move_selection(Some(1), 20, GridKey::PageDown), Some(9));
        assert_eq!(m.move_selection(Some(9), 20, GridKey::PageUp), Some(1));
        assert_eq!(m.move_selection(Some(9), 20, GridKey::End), Some(19));
        assert_eq!(m.move_selection(Some(19), 20, GridKey::Home), Some(0));

        // Tile in row 0 is already visible at scroll_y = 0.
        assert_eq!(m.scroll_target_for(2), None);
        // Tile in row 4 is below viewport_height = 450.
        assert!(m.scroll_target_for(16).is_some());
        // Tile in row 0 when scrolled down returns 0.0.
        let scrolled = GridMetrics {
            scroll_y: 300.0,
            ..m
        };
        assert_eq!(scrolled.scroll_target_for(0), Some(0.0));
    }
}
