use std::borrow::Cow;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, key::Named};
use iced::widget::operation::{self, AbsoluteOffset};
use iced::widget::{Id, Space, column, container, image as iced_image, row, scrollable, text};
use iced::{
    Alignment, Background, Border, Color, Element, Font, Length, Subscription, Task, Theme, window,
};
use image::codecs::jpeg::JpegEncoder;
use image::{ImageBuffer, Rgb};
use tokio::sync::Semaphore;

const TILE_COUNT: usize = 2_000;
const POOL_SIZE: usize = 32;
const THUMB_W: u32 = 200;
const THUMB_H: u32 = 140;
const TILE_W: f32 = 280.0;
const TILE_IMAGE_H: f32 = 160.0;
const TILE_CARD_H: f32 = 196.0;
const TILE_GAP: f32 = 12.0;
const GRID_PADDING: f32 = 16.0;

static VIEW_CALLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone)]
enum Message {
    FontLoaded(Result<(), iced::font::Error>),
    TileDecoded {
        index: usize,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Scrolled {
        offset_y: f32,
        viewport_height: f32,
    },
    WindowResized(iced::Size),
    KeyPressed(keyboard::Key),
    FrameTick(Instant),
    IdleProbeComplete {
        idle_views: usize,
        idle_wall_ms: u128,
        idle_cpu_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchPhase {
    Interactive,
    DecodingAll,
    Scrolling,
    IdleWait,
}

struct SpikeApp {
    scrollable_id: Id,
    encoded_pool: Arc<Vec<Vec<u8>>>,
    decoded: Vec<Option<iced_image::Handle>>,
    in_flight: Vec<bool>,
    decode_sem: Arc<Semaphore>,
    decoded_count: usize,
    selected: usize,
    scroll_y: f32,
    viewport_height: f32,
    window_width: f32,
    custom_font_loaded: Option<bool>,
    custom_font: Font,

    // Benchmark state
    phase: BenchPhase,
    boot_at: Instant,
    decode_done_at: Option<Instant>,
    rss_after_decode_mb: Option<f64>,
    scroll_frame_count: usize,
    last_frame_at: Option<Instant>,
    frame_times_ms: Vec<f64>,
}

fn generate_encoded_pool() -> Vec<Vec<u8>> {
    (0..POOL_SIZE)
        .map(|variant| {
            let r_base = ((variant * 37) % 200 + 30) as u8;
            let g_base = ((variant * 71) % 200 + 30) as u8;
            let b_base = ((variant * 113) % 200 + 30) as u8;
            let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
                ImageBuffer::from_fn(THUMB_W, THUMB_H, |x, y| {
                    let dx = ((x * 255) / THUMB_W) as u8;
                    let dy = ((y * 255) / THUMB_H) as u8;
                    Rgb([
                        r_base.wrapping_add(dx / 3),
                        g_base.wrapping_add(dy / 3),
                        b_base.wrapping_add((dx ^ dy) / 4),
                    ])
                });
            let mut out = Vec::with_capacity(4096);
            let mut encoder = JpegEncoder::new_with_quality(&mut out, 80);
            encoder
                .encode_image(&img)
                .expect("synthetic thumbnail JPEG encode should succeed");
            out
        })
        .collect()
}

fn load_system_ttf() -> Option<Vec<u8>> {
    let candidates = [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\consola.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
    }
    None
}

async fn decode_thumbnail(index: usize, encoded: Vec<u8>, sem: Arc<Semaphore>) -> Message {
    let _permit = sem.acquire_owned().await.ok();
    let res = tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&encoded).expect("synthetic JPEG decode should succeed");
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        (width, height, rgba.into_raw())
    })
    .await
    .expect("spawn_blocking decode should not panic");

    Message::TileDecoded {
        index,
        width: res.0,
        height: res.1,
        rgba: res.2,
    }
}

impl SpikeApp {
    fn boot(benchmark: bool) -> (Self, Task<Message>) {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let permits = (cores / 2).max(1);

        let pool = Arc::new(generate_encoded_pool());
        let sem = Arc::new(Semaphore::new(permits));
        let mut decoded = Vec::with_capacity(TILE_COUNT);
        decoded.resize(TILE_COUNT, None);
        let mut in_flight = Vec::with_capacity(TILE_COUNT);
        in_flight.resize(TILE_COUNT, false);

        let mut tasks = Vec::new();
        if let Some(font_bytes) = load_system_ttf() {
            tasks.push(iced::font::load(Cow::Owned(font_bytes)).map(Message::FontLoaded));
        }

        let phase = if benchmark {
            BenchPhase::DecodingAll
        } else {
            BenchPhase::Interactive
        };

        let initial_decode_count = if benchmark { TILE_COUNT } else { 40 };
        for i in 0..initial_decode_count {
            in_flight[i] = true;
            let bytes = pool[i % POOL_SIZE].clone();
            let sem_clone = Arc::clone(&sem);
            tasks.push(Task::perform(decode_thumbnail(i, bytes, sem_clone), |m| m));
        }

        let app = Self {
            scrollable_id: Id::unique(),
            encoded_pool: pool,
            decoded,
            in_flight,
            decode_sem: sem,
            decoded_count: 0,
            selected: 0,
            scroll_y: 0.0,
            viewport_height: 920.0,
            window_width: 1600.0,
            custom_font_loaded: None,
            custom_font: Font::with_name("Segoe UI"),
            phase,
            boot_at: Instant::now(),
            decode_done_at: None,
            rss_after_decode_mb: None,
            scroll_frame_count: 0,
            last_frame_at: None,
            frame_times_ms: Vec::with_capacity(140),
        };

        (app, Task::batch(tasks))
    }

    fn columns(&self) -> usize {
        let usable = (self.window_width - GRID_PADDING * 2.0).max(TILE_W);
        ((usable + TILE_GAP) / (TILE_W + TILE_GAP)).floor().max(1.0) as usize
    }

    fn row_height(&self) -> f32 {
        TILE_CARD_H + TILE_GAP
    }

    fn total_rows(&self) -> usize {
        let cols = self.columns();
        TILE_COUNT.div_ceil(cols)
    }

    fn max_scroll_y(&self) -> f32 {
        let content_h = self.total_rows() as f32 * self.row_height();
        (content_h - self.viewport_height).max(0.0)
    }

    fn visible_row_range(&self) -> (usize, usize) {
        let total = self.total_rows();
        if total == 0 {
            return (0, 0);
        }
        let rh = self.row_height();
        let first = ((self.scroll_y / rh).floor().max(0.0) as usize).saturating_sub(1);
        let last = (((self.scroll_y + self.viewport_height) / rh)
            .ceil()
            .max(0.0) as usize
            + 1)
        .min(total.saturating_sub(1));
        (first.min(last), last)
    }

    fn queue_visible_decodes(&mut self) -> Task<Message> {
        let cols = self.columns();
        let (first_row, last_row) = self.visible_row_range();
        let start = first_row * cols;
        let end = ((last_row + 1) * cols).min(TILE_COUNT);
        let mut tasks = Vec::new();
        for idx in start..end {
            if self.decoded[idx].is_none() && !self.in_flight[idx] {
                self.in_flight[idx] = true;
                let bytes = self.encoded_pool[idx % POOL_SIZE].clone();
                let sem = Arc::clone(&self.decode_sem);
                tasks.push(Task::perform(decode_thumbnail(idx, bytes, sem), |m| m));
            }
        }
        Task::batch(tasks)
    }

    fn ensure_selected_visible(&self) -> Task<Message> {
        let cols = self.columns();
        let row_idx = self.selected / cols;
        let rh = self.row_height();
        let top = row_idx as f32 * rh;
        let bottom = top + TILE_CARD_H;
        let target_y = if top < self.scroll_y {
            Some(top)
        } else if bottom > self.scroll_y + self.viewport_height {
            Some((bottom - self.viewport_height).max(0.0))
        } else {
            None
        };
        if let Some(y) = target_y {
            operation::scroll_to(
                self.scrollable_id.clone(),
                AbsoluteOffset {
                    x: Some(0.0),
                    y: Some(y),
                },
            )
        } else {
            Task::none()
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::FontLoaded(res) => {
                self.custom_font_loaded = Some(res.is_ok());
                Task::none()
            }
            Message::TileDecoded {
                index,
                width,
                height,
                rgba,
            } => {
                if self.decoded[index].is_none() {
                    self.decoded[index] = Some(iced_image::Handle::from_rgba(width, height, rgba));
                    self.decoded_count += 1;
                }
                self.in_flight[index] = false;

                if self.phase == BenchPhase::DecodingAll && self.decoded_count >= TILE_COUNT {
                    self.decode_done_at = Some(Instant::now());
                    self.rss_after_decode_mb = wallora_windows::current_process_rss_bytes()
                        .map(|b| b as f64 / (1024.0 * 1024.0));
                    self.phase = BenchPhase::Scrolling;
                    self.last_frame_at = Some(Instant::now());
                }
                Task::none()
            }
            Message::Scrolled {
                offset_y,
                viewport_height,
            } => {
                self.scroll_y = offset_y;
                self.viewport_height = viewport_height;
                if self.phase == BenchPhase::Interactive {
                    self.queue_visible_decodes()
                } else {
                    Task::none()
                }
            }
            Message::WindowResized(size) => {
                self.window_width = size.width;
                if self.phase == BenchPhase::Interactive {
                    self.queue_visible_decodes()
                } else {
                    Task::none()
                }
            }
            Message::KeyPressed(key) => {
                let cols = self.columns();
                let prev = self.selected;
                match key {
                    Key::Named(Named::ArrowRight) => {
                        self.selected = (self.selected + 1).min(TILE_COUNT - 1);
                    }
                    Key::Named(Named::ArrowLeft) => {
                        self.selected = self.selected.saturating_sub(1);
                    }
                    Key::Named(Named::ArrowDown) => {
                        self.selected = (self.selected + cols).min(TILE_COUNT - 1);
                    }
                    Key::Named(Named::ArrowUp) => {
                        self.selected = self.selected.saturating_sub(cols);
                    }
                    Key::Named(Named::PageDown) => {
                        self.selected = (self.selected + cols * 4).min(TILE_COUNT - 1);
                    }
                    Key::Named(Named::PageUp) => {
                        self.selected = self.selected.saturating_sub(cols * 4);
                    }
                    Key::Named(Named::Home) => {
                        self.selected = 0;
                    }
                    Key::Named(Named::End) => {
                        self.selected = TILE_COUNT - 1;
                    }
                    _ => {}
                }
                if self.selected != prev {
                    self.ensure_selected_visible()
                } else {
                    Task::none()
                }
            }
            Message::FrameTick(now) => {
                if self.phase != BenchPhase::Scrolling {
                    return Task::none();
                }
                if let Some(prev) = self.last_frame_at {
                    let dt_ms = now.duration_since(prev).as_secs_f64() * 1000.0;
                    self.frame_times_ms.push(dt_ms);
                }
                self.last_frame_at = Some(now);
                self.scroll_frame_count += 1;

                const TOTAL_SCROLL_FRAMES: usize = 120;
                if self.scroll_frame_count <= TOTAL_SCROLL_FRAMES {
                    let progress = self.scroll_frame_count as f32 / TOTAL_SCROLL_FRAMES as f32;
                    let target_y = progress * self.max_scroll_y();
                    self.scroll_y = target_y;
                    operation::scroll_to(
                        self.scrollable_id.clone(),
                        AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(target_y),
                        },
                    )
                } else {
                    self.phase = BenchPhase::IdleWait;
                    Task::perform(
                        async {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            let views_start = VIEW_CALLS.load(Ordering::Relaxed);
                            let cpu_start =
                                wallora_windows::current_process_cpu_time_ms().unwrap_or(0);
                            let started = Instant::now();
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            let idle_views = VIEW_CALLS
                                .load(Ordering::Relaxed)
                                .saturating_sub(views_start);
                            let idle_wall_ms = started.elapsed().as_millis().max(1);
                            let idle_cpu_ms = wallora_windows::current_process_cpu_time_ms()
                                .unwrap_or(0)
                                .saturating_sub(cpu_start);
                            Message::IdleProbeComplete {
                                idle_views,
                                idle_wall_ms,
                                idle_cpu_ms,
                            }
                        },
                        |m| m,
                    )
                }
            }
            Message::IdleProbeComplete {
                idle_views,
                idle_wall_ms,
                idle_cpu_ms,
            } => {
                self.print_benchmark_report(idle_views, idle_wall_ms, idle_cpu_ms);
                iced::exit()
            }
        }
    }

    fn print_benchmark_report(&self, idle_views: usize, idle_wall_ms: u128, idle_cpu_ms: u64) {
        let mut sorted = self.frame_times_ms.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f64| -> f64 {
            if sorted.is_empty() {
                return 0.0;
            }
            let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
            sorted[idx]
        };
        let p50 = pct(0.50);
        let p95 = pct(0.95);
        let p99 = pct(0.99);
        let max_dt = sorted.last().copied().unwrap_or(0.0);
        let decode_ms = self
            .decode_done_at
            .map(|t| t.duration_since(self.boot_at).as_millis())
            .unwrap_or(0);
        let rss_mb = self.rss_after_decode_mb.unwrap_or(0.0);
        let idle_cpu_pct = (idle_cpu_ms as f64 / idle_wall_ms as f64) * 100.0;
        let backend = std::env::var("ICED_BACKEND").unwrap_or_else(|_| "wgpu(default)".to_string());

        println!("=== WALLORA SPIKE BENCHMARK ===");
        println!("backend={backend}");
        println!("tiles={TILE_COUNT} decoded={}", self.decoded_count);
        println!("custom_font_loaded={:?}", self.custom_font_loaded);
        println!("decode_2000_wall_ms={decode_ms}");
        println!("rss_after_2000_decode_mb={rss_mb:.2}");
        println!(
            "scroll_frames={} p50_ms={p50:.2} p95_ms={p95:.2} p99_ms={p99:.2} max_ms={max_dt:.2}",
            sorted.len()
        );
        println!(
            "idle_wall_ms={idle_wall_ms} idle_view_redraws={idle_views} idle_cpu_ms={idle_cpu_ms} idle_cpu_pct={idle_cpu_pct:.2}"
        );
        println!("===============================");
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed { key, .. } => Some(Message::KeyPressed(key)),
                _ => None,
            }),
            window::resize_events().map(|(_id, size)| Message::WindowResized(size)),
        ];
        if self.phase == BenchPhase::Scrolling {
            subs.push(window::frames().map(Message::FrameTick));
        }
        Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Message> {
        VIEW_CALLS.fetch_add(1, Ordering::Relaxed);
        let cols = self.columns();
        let total_rows = self.total_rows();
        let (first_row, last_row) = self.visible_row_range();
        let rh = self.row_height();

        let font_status = match self.custom_font_loaded {
            Some(true) => "custom font: ok",
            Some(false) => "custom font: failed",
            None => "custom font: loading",
        };
        let rss_text = wallora_windows::current_process_rss_bytes()
            .map(|b| format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)))
            .unwrap_or_else(|| "n/a".to_string());

        let status_bar = container(
            row![
                text(format!(
                    "Wallora Iced Spike | tiles: {}/{} | selected: #{} | rows: {}..={} | RSS: {} | {}",
                    self.decoded_count,
                    TILE_COUNT,
                    self.selected + 1,
                    first_row,
                    last_row,
                    rss_text,
                    font_status
                ))
                .font(self.custom_font)
                .size(14)
                .color(Color::from_rgb8(0xF5, 0xF4, 0xF2)),
            ]
            .align_y(Alignment::Center),
        )
        .padding([10, 16])
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(0x34, 0x30, 0x2D))),
            ..Default::default()
        });

        let mut grid_rows = column![].spacing(TILE_GAP);
        if first_row > 0 {
            grid_rows = grid_rows.push(Space::new().height(first_row as f32 * rh));
        }

        for r in first_row..=last_row {
            let mut r_row = row![].spacing(TILE_GAP);
            for c in 0..cols {
                let idx = r * cols + c;
                if idx >= TILE_COUNT {
                    break;
                }
                let is_selected = idx == self.selected;
                let thumb: Element<'_, Message> = if let Some(handle) = &self.decoded[idx] {
                    iced_image(handle.clone())
                        .width(Length::Fill)
                        .height(TILE_IMAGE_H)
                        .into()
                } else {
                    container(text("Decoding...").size(12))
                        .width(Length::Fill)
                        .height(TILE_IMAGE_H)
                        .center_x(Length::Fill)
                        .center_y(TILE_IMAGE_H)
                        .into()
                };
                let card = container(
                    column![
                        thumb,
                        text(format!("Tile #{:04}", idx + 1))
                            .font(self.custom_font)
                            .size(12)
                            .color(Color::from_rgb8(0xB0, 0xAC, 0xA9)),
                    ]
                    .spacing(4),
                )
                .width(TILE_W)
                .height(TILE_CARD_H)
                .padding(6)
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(Color::from_rgb8(0x34, 0x30, 0x2D))),
                    border: Border {
                        radius: 6.0.into(),
                        width: if is_selected { 2.0 } else { 1.0 },
                        color: if is_selected {
                            Color::from_rgb8(0xF3, 0x73, 0x16)
                        } else {
                            Color::from_rgb8(0x5E, 0x5A, 0x57)
                        },
                    },
                    ..Default::default()
                });
                r_row = r_row.push(card);
            }
            grid_rows = grid_rows.push(r_row);
        }

        let remaining_rows = total_rows.saturating_sub(last_row + 1);
        if remaining_rows > 0 {
            grid_rows = grid_rows.push(Space::new().height(remaining_rows as f32 * rh));
        }

        let scroll = scrollable(container(grid_rows).padding(GRID_PADDING))
            .id(self.scrollable_id.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .on_scroll(|vp| Message::Scrolled {
                offset_y: vp.absolute_offset().y,
                viewport_height: vp.bounds().height,
            });

        column![status_bar, scroll]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn main() -> iced::Result {
    let benchmark = std::env::args().any(|a| a == "--benchmark");
    // Touch Cursor to ensure jpeg decoder path works in release too.
    let _ = Cursor::new(&[] as &[u8]);
    iced::application(
        move || SpikeApp::boot(benchmark),
        SpikeApp::update,
        SpikeApp::view,
    )
    .title(|_state: &SpikeApp| "Wallora — Iced Gallery Spike (2,000 Tiles)".to_string())
    .theme(SpikeApp::theme)
    .subscription(SpikeApp::subscription)
    .window_size((1600.0, 1000.0))
    .run()
}
