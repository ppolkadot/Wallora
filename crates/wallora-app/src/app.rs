use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, Task, Theme};
use wallora_core::storage::AppPaths;

use crate::message::{Message, Route};
use crate::theme::{palette, surface_color, wallora_theme};
use crate::views;

pub struct WalloraApp {
    route: Route,
    paths: AppPaths,
}

impl WalloraApp {
    pub fn boot(paths: AppPaths) -> (Self, Task<Message>) {
        (
            Self {
                route: Route::Home,
                paths,
            },
            Task::none(),
        )
    }

    pub fn title(&self) -> String {
        format!(
            "Wallora — {} (Unofficial Wallhaven Client)",
            self.route.label()
        )
    }

    pub fn theme(&self) -> Theme {
        wallora_theme()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Navigate(route) => {
                if self.route != route {
                    tracing::info!(?route, "navigated view");
                    self.route = route;
                }
                Task::none()
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let nav_buttons = [Route::Home, Route::Search, Route::Settings]
            .into_iter()
            .fold(row![].spacing(8), |r, route| {
                let active = self.route == route;
                let btn = button(text(route.label()).size(14))
                    .padding([8, 16])
                    .on_press(Message::Navigate(route))
                    .style(move |theme, status| {
                        let base = if active {
                            button::primary(theme, status)
                        } else {
                            button::secondary(theme, status)
                        };
                        button::Style {
                            background: Some(Background::Color(if active {
                                palette::ACCENT
                            } else {
                                surface_color()
                            })),
                            text_color: if active {
                                palette::CANVAS
                            } else {
                                palette::TEXT_PRIMARY
                            },
                            border: Border {
                                radius: 6.0.into(),
                                width: 1.0,
                                color: if active {
                                    palette::ACCENT
                                } else {
                                    palette::MUTED
                                },
                            },
                            ..base
                        }
                    });
                r.push(btn)
            });

        let header = container(
            row![text("Wallora").size(20).color(palette::ACCENT), nav_buttons,]
                .spacing(24)
                .align_y(Alignment::Center),
        )
        .padding([12, 20])
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(surface_color())),
            ..Default::default()
        });

        let body = match self.route {
            Route::Home => views::home::view(),
            Route::Search => views::search::view(),
            Route::Settings => views::settings::view(&self.paths),
        };

        let content = container(body)
            .padding(24)
            .width(Length::Fill)
            .height(Length::Fill);

        column![header, content]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_paths() -> AppPaths {
        AppPaths::from_roots(
            PathBuf::from("C:/tmp/wallora-test/config"),
            PathBuf::from("C:/tmp/wallora-test/local"),
        )
    }

    #[test]
    fn navigation_switches_routes() {
        let (mut app, _) = WalloraApp::boot(test_paths());
        assert_eq!(app.route, Route::Home);

        let _ = app.update(Message::Navigate(Route::Search));
        assert_eq!(app.route, Route::Search);

        let _ = app.update(Message::Navigate(Route::Settings));
        assert_eq!(app.route, Route::Settings);

        let _ = app.update(Message::Navigate(Route::Home));
        assert_eq!(app.route, Route::Home);
    }
}
