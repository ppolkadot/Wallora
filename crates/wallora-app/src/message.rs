/// Top-level navigation destinations in the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Home,
    Search,
    Settings,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Search => "Search",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Navigate(Route),
}
