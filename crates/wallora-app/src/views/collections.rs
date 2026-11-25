use iced::widget::{Id, button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Element, Length};
use wallora_core::domain::{CollectionSummary, Purity, PurityMask, Wallpaper};
use wallora_core::services::{
    CollectionListStatus, CollectionSource, CollectionUnsupportedAction, CollectionsController,
    OwnCollectionsListStatus, OwnItemsBlockedReason, SelectedCollection,
};

use crate::message::{GalleryKind, Message, Route};
use crate::theme::{card_style, disabled_button_style, palette, pill_button_style};
use crate::widgets::gallery_grid::{self, GalleryGridView, GridMetrics};
use crate::widgets::thumbnail::ThumbnailViewState;

pub const COLLECTIONS_SIDEBAR_WIDTH: f32 = 340.0;

pub struct CollectionsViewState<'a, F>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    pub controller: &'a CollectionsController,
    pub username_input: &'a str,
    pub username_input_id: &'a Id,
    pub username_error: Option<&'a str>,
    pub selectable_purity: PurityMask,
    pub is_guest: bool,
    pub refresh_busy: bool,
    pub scrollable_id: &'a Id,
    pub metrics: &'a GridMetrics,
    pub selected_tile: Option<usize>,
    pub thumbnail_state: F,
}

pub fn view<'a, F>(state: CollectionsViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let header = row![
        text("Collections").size(24).color(palette::TEXT_PRIMARY),
        text("Browse your verified collections and other users' public collections (read-only).")
            .size(13)
            .color(palette::TEXT_SECONDARY),
    ]
    .spacing(16)
    .align_y(Alignment::Center);

    let sidebar = build_sidebar(&state);
    let main_pane = build_main_pane(state);

    let body = row![
        container(scrollable(sidebar).height(Length::Fill))
            .width(Length::Fixed(COLLECTIONS_SIDEBAR_WIDTH))
            .height(Length::Fill),
        container(main_pane)
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    .spacing(16)
    .width(Length::Fill)
    .height(Length::Fill);

    column![header, body]
        .spacing(14)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn build_sidebar<'a, F>(state: &CollectionsViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    column![
        build_my_collections_card(state),
        build_browse_user_card(state),
        build_manage_handoff_card(),
    ]
    .spacing(12)
    .width(Length::Fill)
    .into()
}

fn build_my_collections_card<'a, F>(state: &CollectionsViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let mut col = column![].spacing(8);

    if let Some(account) = state.controller.own_account() {
        let mut refresh_btn = button(
            text(if state.refresh_busy {
                "Refreshing..."
            } else {
                "Refresh"
            })
            .size(12),
        )
        .padding([4, 10])
        .style(pill_button_style(false));
        if !state.refresh_busy {
            refresh_btn = refresh_btn.on_press(Message::RefreshAccount { reverify: true });
        }

        col = col.push(
            row![
                text("My Collections")
                    .size(16)
                    .color(palette::TEXT_PRIMARY)
                    .width(Length::Fill),
                refresh_btn,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        let items_allowed = account.verification.can_access_own_collection_items();
        if !items_allowed {
            let mut gate_actions = row![
                button(text("Open Account").size(12))
                    .padding([5, 10])
                    .on_press(Message::Navigate(Route::Account))
                    .style(pill_button_style(false)),
            ]
            .spacing(8);
            let mut verify_btn = button(
                text(if state.refresh_busy {
                    "Verifying..."
                } else {
                    "Retry verification"
                })
                .size(12),
            )
            .padding([5, 10])
            .style(pill_button_style(false));
            if !state.refresh_busy {
                verify_btn = verify_btn.on_press(Message::RefreshAccount { reverify: true });
            }
            gate_actions = gate_actions.push(verify_btn);

            col = col.push(
                container(
                    column![
                        text(format!("Items locked: {}", account.verification.label()))
                            .size(12)
                            .color(palette::WARNING),
                        text(account.verification.explanation())
                            .size(11)
                            .color(palette::TEXT_SECONDARY),
                        gate_actions.wrap().vertical_spacing(6),
                    ]
                    .spacing(6),
                )
                .padding(10)
                .width(Length::Fill)
                .style(card_style(palette::WARNING, 1.0)),
            );
        }

        match state.controller.own_collections_status() {
            OwnCollectionsListStatus::SignedOut => {}
            OwnCollectionsListStatus::NotLoaded => {
                col = col.push(
                    text("Own collection list not loaded yet.")
                        .size(12)
                        .color(palette::TEXT_SECONDARY),
                );
            }
            OwnCollectionsListStatus::Empty => {
                col = col.push(
                    text("No collections returned for this account.")
                        .size(12)
                        .color(palette::TEXT_SECONDARY),
                );
            }
            OwnCollectionsListStatus::Ready => {
                for entry in state.controller.own_collections().unwrap_or_default() {
                    let display_label = state.controller.display_own_collection_label(entry);
                    let selected = matches!(
                        state.controller.selected_collection(),
                        Some(SelectedCollection {
                            source: CollectionSource::Own { .. },
                            collection_id,
                            ..
                        }) if *collection_id == entry.id
                    );
                    col = col.push(collection_list_item(
                        entry,
                        display_label,
                        selected,
                        !items_allowed,
                        Message::CollectionsSelectOwn(entry.id),
                    ));
                }
            }
        }
    } else {
        col = col.push(text("My Collections").size(16).color(palette::TEXT_PRIMARY));
        col = col.push(
            text("Sign in with a verified Wallhaven account to list your collections and view private items.")
                .size(12)
                .color(palette::TEXT_SECONDARY),
        );
        col = col.push(
            button(text("Open Account").size(12))
                .padding([6, 12])
                .on_press(Message::Navigate(Route::Account))
                .style(pill_button_style(false)),
        );
    }

    container(col)
        .padding(12)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0))
        .into()
}

fn build_browse_user_card<'a, F>(state: &CollectionsViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    let load_label = if state.controller.is_loading_user_collections() {
        "Loading..."
    } else {
        "Load"
    };

    let input_row = row![
        text_input("Wallhaven username", state.username_input)
            .id(state.username_input_id.clone())
            .on_input(Message::CollectionsUsernameChanged)
            .on_submit(Message::CollectionsLoadUser)
            .padding(6)
            .width(Length::Fill),
        button(text(load_label).size(12))
            .padding([6, 12])
            .on_press(Message::CollectionsLoadUser)
            .style(pill_button_style(false)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut col = column![
        text("Browse User Collections")
            .size(16)
            .color(palette::TEXT_PRIMARY),
        text("List public collections for any Wallhaven username.")
            .size(12)
            .color(palette::TEXT_SECONDARY),
        input_row,
    ]
    .spacing(8);

    if let Some(err) = state.username_error {
        col = col.push(text(err).size(12).color(palette::DANGER));
    }

    match state.controller.user_collections_status() {
        CollectionListStatus::Idle => {
            col = col.push(
                text("Enter a username and click Load to browse public collections.")
                    .size(12)
                    .color(palette::MUTED),
            );
        }
        CollectionListStatus::Loading => {
            col = col.push(
                text("Loading public collections...")
                    .size(12)
                    .color(palette::TEXT_SECONDARY),
            );
        }
        CollectionListStatus::Empty => {
            let who = state.controller.browsed_username().unwrap_or("This user");
            col = col.push(
                text(format!("{who} has no public collections."))
                    .size(12)
                    .color(palette::TEXT_SECONDARY),
            );
        }
        CollectionListStatus::Failed(err) => {
            col = col.push(
                column![
                    text(err.user_message()).size(12).color(palette::DANGER),
                    button(text("Retry").size(12))
                        .padding([5, 12])
                        .on_press(Message::CollectionsRetryUserList),
                ]
                .spacing(6),
            );
        }
        CollectionListStatus::Ready => {
            if let Some(username) = state.controller.browsed_username() {
                col = col.push(
                    text(format!("Public collections for @{username}"))
                        .size(12)
                        .color(palette::TEXT_SECONDARY),
                );
            }
            for entry in state.controller.user_collections() {
                let selected = matches!(
                    state.controller.selected_collection(),
                    Some(SelectedCollection {
                        source: CollectionSource::PublicUser { username },
                        collection_id,
                        ..
                    }) if *collection_id == entry.id
                        && state
                            .controller
                            .browsed_username()
                            .is_some_and(|u| u.eq_ignore_ascii_case(username))
                );
                col = col.push(collection_list_item(
                    entry,
                    entry.label.as_str(),
                    selected,
                    false,
                    Message::CollectionsSelectUserCollection(entry.id),
                ));
            }
        }
    }

    container(col)
        .padding(12)
        .width(Length::Fill)
        .style(card_style(palette::MUTED, 1.0))
        .into()
}

fn build_manage_handoff_card<'a>() -> Element<'a, Message> {
    let disabled_controls = row![
        button(text("Create collection").size(11))
            .padding([5, 10])
            .style(disabled_button_style()),
        button(text("Rename / Delete").size(11))
            .padding([5, 10])
            .style(disabled_button_style()),
        button(text("Toggle privacy").size(11))
            .padding([5, 10])
            .style(disabled_button_style()),
    ]
    .spacing(6)
    .wrap()
    .vertical_spacing(6);

    container(
        column![
            text(CollectionUnsupportedAction::ManageCollections.title())
                .size(15)
                .color(palette::TEXT_PRIMARY),
            disabled_controls,
            text(CollectionUnsupportedAction::ManageCollections.explanation())
                .size(12)
                .color(palette::TEXT_SECONDARY),
            button(text(CollectionUnsupportedAction::ManageCollections.label()).size(12))
                .padding([6, 12])
                .on_press(Message::CollectionsOpenManageHandOff)
                .style(pill_button_style(false)),
        ]
        .spacing(8),
    )
    .padding(12)
    .width(Length::Fill)
    .style(card_style(palette::MUTED, 1.0))
    .into()
}

fn collection_list_item<'a>(
    summary: &'a CollectionSummary,
    display_label: &'a str,
    selected: bool,
    locked: bool,
    on_press: Message,
) -> Element<'a, Message> {
    let (visibility_text, visibility_color) = if summary.public {
        ("Public", palette::SUCCESS)
    } else {
        ("Private", palette::WARNING)
    };

    let mut top_row = row![
        text(display_label)
            .size(13)
            .color(if selected {
                palette::CANVAS
            } else {
                palette::TEXT_PRIMARY
            })
            .width(Length::Fill),
        text(visibility_text).size(11).color(if selected {
            palette::CANVAS
        } else {
            visibility_color
        }),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    if locked {
        top_row = top_row.push(text("Locked").size(11).color(if selected {
            palette::CANVAS
        } else {
            palette::WARNING
        }));
    }

    let meta_row = text(format!(
        "{} items  •  {} views",
        summary.count, summary.views
    ))
    .size(11)
    .color(if selected {
        palette::CANVAS
    } else {
        palette::TEXT_SECONDARY
    });

    button(column![top_row, meta_row].spacing(3).width(Length::Fill))
        .padding([8, 10])
        .width(Length::Fill)
        .on_press(on_press)
        .style(pill_button_style(selected))
        .into()
}

fn build_main_pane<'a, F>(state: CollectionsViewState<'a, F>) -> Element<'a, Message>
where
    F: Fn(&Wallpaper) -> ThumbnailViewState<'a>,
{
    if let Some(selected) = state.controller.selected_collection() {
        let source_badge = match &selected.source {
            CollectionSource::Own { username, .. } => format!("My collection (@{username})"),
            CollectionSource::PublicUser { username } => format!("@{username}"),
        };

        let mut title_row = row![
            text(&selected.label).size(20).color(palette::TEXT_PRIMARY),
            text(source_badge).size(12).color(palette::TEXT_SECONDARY),
        ]
        .spacing(12)
        .align_y(Alignment::Center);

        if let Some(is_public) = selected.public {
            title_row = title_row.push(
                text(if is_public { "Public" } else { "Private" })
                    .size(12)
                    .color(if is_public {
                        palette::SUCCESS
                    } else {
                        palette::WARNING
                    }),
            );
        }

        if let Some(total) = state.controller.total() {
            title_row = title_row.push(
                text(format!("{total} wallpapers"))
                    .size(12)
                    .color(palette::TEXT_SECONDARY),
            );
        }

        title_row = title_row.push(
            button(text("Refresh").size(12))
                .padding([5, 12])
                .on_press(Message::GalleryRefresh(GalleryKind::Collections))
                .style(pill_button_style(false)),
        );

        let purity = state.controller.purity();
        let nsfw_label = if state.selectable_purity.nsfw {
            "NSFW"
        } else {
            "NSFW (key)"
        };
        let mut purity_row = row![
            text("Purity:").size(13).color(palette::TEXT_SECONDARY),
            button(text("SFW").size(12))
                .padding([5, 10])
                .on_press(Message::CollectionsTogglePurity(Purity::Sfw))
                .style(pill_button_style(purity.sfw)),
            button(text("Sketchy").size(12))
                .padding([5, 10])
                .on_press(Message::CollectionsTogglePurity(Purity::Sketchy))
                .style(pill_button_style(purity.sketchy)),
            button(text(nsfw_label).size(12))
                .padding([5, 10])
                .on_press(Message::CollectionsTogglePurity(Purity::Nsfw))
                .style(pill_button_style(purity.nsfw)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        if purity.nsfw && !state.selectable_purity.nsfw {
            purity_row = purity_row.push(
                text("Upstream enforces NSFW access for this session.")
                    .size(12)
                    .color(palette::WARNING),
            );
        }

        let header =
            container(column![title_row.wrap().vertical_spacing(6), purity_row].spacing(8))
                .padding(12)
                .width(Length::Fill)
                .style(card_style(palette::MUTED, 1.0));

        let grid = gallery_grid::view(GalleryGridView {
            kind: GalleryKind::Collections,
            scrollable_id: state.scrollable_id,
            metrics: state.metrics,
            items: state.controller.items(),
            status: state.controller.items_status(),
            is_loading: state.controller.is_loading_items(),
            total: state.controller.total(),
            selected: state.selected_tile,
            empty_message: "This collection has no wallpapers for the selected purity.",
            show_account_shortcut_on_unauthorized: state.is_guest,
            thumbnail_state: state.thumbnail_state,
        });

        return column![header, grid]
            .spacing(12)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    if let Some(reason) = state.controller.items_blocked() {
        let mut actions = row![
            button(text("Open Account").size(13))
                .padding([8, 16])
                .on_press(Message::Navigate(Route::Account))
                .style(pill_button_style(false)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        if matches!(reason, OwnItemsBlockedReason::Unverified(_)) {
            let mut refresh_btn = button(
                text(if state.refresh_busy {
                    "Verifying..."
                } else {
                    "Refresh & verify"
                })
                .size(13),
            )
            .padding([8, 16])
            .style(pill_button_style(false));
            if !state.refresh_busy {
                refresh_btn = refresh_btn.on_press(Message::RefreshAccount { reverify: true });
            }
            actions = actions.push(refresh_btn);
        }

        let card = container(
            column![
                text("Collection items require a verified username")
                    .size(18)
                    .color(palette::WARNING),
                text(reason.explanation())
                    .size(13)
                    .color(palette::TEXT_SECONDARY),
                actions,
            ]
            .spacing(10)
            .align_x(Alignment::Start),
        )
        .padding(24)
        .width(Length::Fixed(520.0))
        .style(card_style(palette::WARNING, 1.0));

        return container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
    }

    let prompt_text = if let Some(blocked) = state.controller.own_items_blocked_reason()
        && matches!(blocked, OwnItemsBlockedReason::Unverified(_))
    {
        "Your own collection list is visible on the left, while item access stays locked until your username is verified. You can still browse any other user's public collections."
    } else {
        "Select a collection on the left, or enter a Wallhaven username to browse public collections."
    };

    container(
        container(
            column![
                text("Select a collection")
                    .size(18)
                    .color(palette::TEXT_PRIMARY),
                text(prompt_text).size(13).color(palette::TEXT_SECONDARY),
            ]
            .spacing(8),
        )
        .padding(24)
        .width(Length::Fixed(520.0))
        .style(card_style(palette::MUTED, 1.0)),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}
