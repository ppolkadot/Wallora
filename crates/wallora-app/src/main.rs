#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cache_host;
mod message;
mod theme;
mod views;
mod widgets;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use wallora_core::storage::AppPaths;

use crate::app::WalloraApp;
use crate::cache_host::CacheHost;

fn init_tracing(logs_dir: &Path) -> WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(logs_dir, "wallora.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("wallora_app=info,wallora_core=info,warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .init();

    guard
}

fn install_panic_hook(logs_dir: std::path::PathBuf) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        };

        let summary = format!("Wallora encountered a fatal error at {location}:\n{payload}");
        tracing::error!(%location, %payload, "unhandled panic");

        let panic_log_path = logs_dir.join("panic.log");
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log_path)
        {
            let _ = writeln!(file, "{summary}");
        }

        if std::env::var_os("WALLORA_NO_PANIC_DIALOG").is_none() {
            let dialog_msg = format!(
                "{summary}\n\nDiagnostic logs were written to:\n{}",
                logs_dir.display()
            );
            wallora_windows::show_error_dialog("Wallora — Unexpected Error", &dialog_msg);
        }

        default_hook(info);
    }));
}

fn main() -> iced::Result {
    let paths = match AppPaths::resolve().and_then(|p| {
        p.ensure_dirs()?;
        Ok(p)
    }) {
        Ok(paths) => paths,
        Err(err) => {
            let msg = format!("Failed to initialize Wallora application directories: {err}");
            wallora_windows::show_error_dialog("Wallora — Startup Error", &msg);
            std::process::exit(1);
        }
    };

    let _log_guard = init_tracing(&paths.logs_dir);
    install_panic_hook(paths.logs_dir.clone());
    tracing::info!(
        config_dir = %paths.config_dir.display(),
        cache_dir = %paths.cache_dir.display(),
        logs_dir = %paths.logs_dir.display(),
        "starting Wallora shell"
    );

    // The image cache actor lives on its own runtime so it outlives individual Iced tasks. A
    // failure to start it is not fatal: the app runs uncached with a warning notice.
    let cache_host = match CacheHost::new() {
        Ok(host) => Some(host),
        Err(err) => {
            tracing::error!(reason = %err, "could not start the image cache runtime");
            None
        }
    };

    let boot_host = cache_host.clone();
    let result = iced::application(
        move || WalloraApp::boot_interactive(paths.clone(), boot_host.clone()),
        WalloraApp::update,
        WalloraApp::view,
    )
    .title(WalloraApp::title)
    .theme(WalloraApp::theme)
    .subscription(WalloraApp::subscription)
    .window_size((1280.0, 800.0))
    .run();

    // Orderly cache shutdown: stop admissions, join writers, then release the root lock.
    if let Some(host) = cache_host {
        host.shutdown();
    }
    result
}
