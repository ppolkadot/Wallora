use std::collections::BTreeSet;
use std::io::{Cursor, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use image::{ImageFormat, Rgba, RgbaImage};
use proptest::prelude::*;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use url::Url;
use wallora_core::domain::{
    AccountId, Category, CredentialStore, InMemoryCredentialStore, Purity, SecretString, Thumbs,
    Wallpaper, WallpaperId,
};
use wallora_core::provider::{
    IMAGE_IN_FLIGHT_CAP, ProviderError, WallhavenClient, WallhavenClientConfig,
};
use wallora_core::services::cache::CancelToken;
use wallora_core::services::{
    DownloadError, DownloadFs, DownloadProgress, DownloadRequest, FilePublisher,
    HardLinkFilePublisher, ImageGuard, OriginalDownloader, PartWriter, PublishError,
    RealDownloadFs,
};
use wallora_core::util::{
    DEFAULT_FALLBACK_NAME, MAX_FILENAME_UTF16_LEN, default_download_filename,
    extract_safe_extension, is_windows_reserved_device_name, numbered_collision_name,
    sanitize_filename, split_sanitized_stem_ext,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn encode_png(width: u32, height: u32, fill: [u8; 4]) -> Vec<u8> {
    let img = RgbaImage::from_pixel(width, height, Rgba(fill));
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode png");
    out
}

fn encode_textured_png(width: u32, height: u32) -> Vec<u8> {
    let img = RgbaImage::from_fn(width, height, |x, y| {
        Rgba([
            ((x.wrapping_mul(37) ^ y.wrapping_mul(13)) & 0xFF) as u8,
            ((x.wrapping_mul(17) + y.wrapping_mul(71)) & 0xFF) as u8,
            ((x ^ y.wrapping_mul(29)) & 0xFF) as u8,
            255,
        ])
    });
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode textured png");
    out
}

fn sample_wallpaper(id: &str, url: &str, file_size: u64, width: u32, height: u32) -> Wallpaper {
    Wallpaper {
        id: WallpaperId::new(id),
        url: format!("https://wallhaven.cc/w/{id}"),
        short_url: Some(format!("https://whvn.cc/{id}")),
        uploader: None,
        views: 42,
        favorites: 7,
        source: String::new(),
        purity: Purity::Sfw,
        category: Category::General,
        dimension_x: width,
        dimension_y: height,
        resolution: format!("{width}x{height}"),
        ratio: "1.50".to_string(),
        file_size,
        file_type: "image/png".to_string(),
        created_at: "2026-01-15 12:00:00".to_string(),
        colors: vec!["#1e1c1a".to_string()],
        path: url.to_string(),
        thumbs: Thumbs {
            large: "https://th.wallhaven.cc/lg/ab/test.jpg".to_string(),
            original: "https://th.wallhaven.cc/orig/ab/test.jpg".to_string(),
            small: "https://th.wallhaven.cc/small/ab/test.jpg".to_string(),
        },
        tags: Vec::new(),
    }
}

fn test_client(image_origin: &str) -> WallhavenClient {
    let creds = Arc::new(InMemoryCredentialStore::new());
    let account = AccountId::generate();
    creds
        .save(&account, &SecretString::new("wh_synthetic_download_secret"))
        .unwrap();
    let api_base = Url::parse("http://127.0.0.1:1/api/v1").unwrap();
    let config = WallhavenClientConfig::for_test(api_base)
        .unwrap()
        .with_test_image_origin(Url::parse(image_origin).unwrap())
        .unwrap();
    WallhavenClient::with_config(creds as Arc<dyn CredentialStore>, config).unwrap()
}

fn list_entries(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn assert_safe_windows_filename(name: &str) {
    assert!(!name.is_empty(), "sanitized name must not be empty");
    assert_ne!(name, ".");
    assert_ne!(name, "..");
    assert!(!name.contains(".."), "must not contain traversal: {name:?}");
    assert!(
        !name.starts_with('.') && !name.ends_with('.'),
        "must not start or end with dot: {name:?}"
    );
    assert!(
        !name.starts_with(char::is_whitespace) && !name.ends_with(char::is_whitespace),
        "must not start or end with whitespace: {name:?}"
    );
    assert!(
        !name
            .chars()
            .any(|c| c.is_control()
                || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')),
        "must not contain forbidden/control chars: {name:?}"
    );
    assert!(
        !is_windows_reserved_device_name(name),
        "must not be a Windows reserved device name: {name:?}"
    );
    assert!(
        name.encode_utf16().count() <= MAX_FILENAME_UTF16_LEN,
        "exceeded max UTF-16 length: {name:?}"
    );
    let mut comps = Path::new(name).components();
    assert!(
        matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none(),
        "must be a single normal path component: {name:?}"
    );
}

#[test]
fn filename_sanitizer_neutralizes_reserved_names_dots_spaces_separators_and_controls() {
    let mut reserved = vec!["CON", "PRN", "AUX", "NUL", "con", "PrN", "aUx", "nUl"];
    let mut com_lpt = Vec::new();
    for i in 1..=9 {
        com_lpt.push(format!("COM{i}"));
        com_lpt.push(format!("com{i}"));
        com_lpt.push(format!("LPT{i}"));
        com_lpt.push(format!("lpt{i}"));
    }
    for name in reserved.drain(..).chain(com_lpt.iter().map(String::as_str)) {
        for variant in [
            name.to_string(),
            format!("{name}.png"),
            format!("{name}.tar.gz"),
            format!("{name} .txt"),
            format!("  .{name}.  "),
        ] {
            let safe = sanitize_filename(&variant);
            assert_safe_windows_filename(&safe);
            assert!(safe.starts_with('_'), "{variant:?} -> {safe:?}");
        }
    }

    // Non-reserved names like COM0, LPT0, CONSOLE stay untouched.
    for ok in [
        "COM0.png",
        "LPT0.jpg",
        "CONSOLE.png",
        "wallhaven-abc123.jpg",
    ] {
        assert_eq!(sanitize_filename(ok), ok);
    }

    // Traversal, separators, colons, trailing dots/spaces, controls, empties.
    for tricky in [
        "",
        "   ",
        ".",
        "..",
        "...",
        "foo. ",
        "bar...",
        "baz . .  ",
        "../secret.png",
        "..\\..\\Windows\\System32\\CON.png",
        "C:\\Users\\alice:stream.png",
        "a/b\\c:d*e?f\"g<h>i|j\0k\x1fl\x7f.png",
    ] {
        let safe = sanitize_filename(tricky);
        assert_safe_windows_filename(&safe);
    }
    assert_eq!(sanitize_filename(""), DEFAULT_FALLBACK_NAME);
    assert_eq!(sanitize_filename("..."), DEFAULT_FALLBACK_NAME);
    assert_eq!(sanitize_filename("foo.  "), "foo");
    assert_eq!(
        numbered_collision_name("wallhaven-abc123", Some("png"), 1),
        "wallhaven-abc123 (1).png"
    );
    assert_eq!(
        numbered_collision_name("wallhaven-abc123", Some("png"), 2),
        "wallhaven-abc123 (2).png"
    );
    assert_eq!(numbered_collision_name("archive", None, 3), "archive (3)");

    // Very long stem preserves collision suffix within UTF-16 budget.
    let long_collision = numbered_collision_name(&"a".repeat(250), Some("png"), 42);
    assert_safe_windows_filename(&long_collision);
    assert!(long_collision.ends_with(" (42).png"), "{long_collision}");
}

#[test]
fn default_download_filename_extracts_extension_and_falls_back_cleanly() {
    let jpg = sample_wallpaper(
        "abc123",
        "https://w.wallhaven.cc/full/ab/wallhaven-abc123.jpg",
        10,
        1,
        1,
    );
    assert_eq!(default_download_filename(&jpg), "wallhaven-abc123.jpg");

    let with_query = sample_wallpaper(
        "xyz999",
        "https://w.wallhaven.cc/full/xy/wallhaven-xyz999.PNG?token=1#frag",
        10,
        1,
        1,
    );
    assert_eq!(
        default_download_filename(&with_query),
        "wallhaven-xyz999.png"
    );

    let mut no_ext_png = sample_wallpaper(
        "p1",
        "https://w.wallhaven.cc/full/p1/image_without_ext",
        10,
        1,
        1,
    );
    no_ext_png.file_type = "image/png".to_string();
    assert_eq!(default_download_filename(&no_ext_png), "wallhaven-p1.png");

    no_ext_png.file_type = "image/jpeg".to_string();
    assert_eq!(extract_safe_extension(&no_ext_png), "jpg");
    no_ext_png.file_type = "application/octet-stream".to_string();
    assert_eq!(extract_safe_extension(&no_ext_png), "bin");

    let evil_id = sample_wallpaper(
        "../../CON:x",
        "https://w.wallhaven.cc/full/ab/wallhaven-evil.jpg",
        10,
        1,
        1,
    );
    let safe_evil = default_download_filename(&evil_id);
    assert_safe_windows_filename(&safe_evil);
    assert!(safe_evil.ends_with(".jpg"));
}

#[test]
fn sanitized_names_can_be_created_as_regular_files_on_disk() {
    let dir = TempDir::new("wallora-sanitize-fs");
    for raw in [
        "CON",
        "NUL.png",
        "COM1.jpg",
        "LPT9 .png",
        "../a/b.png",
        "foo..bar.png",
        "trailing. ",
    ] {
        let safe = sanitize_filename(raw);
        let path = dir.0.join(&safe);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap_or_else(|e| {
                panic!("failed to create regular file for {raw:?} ({safe:?}): {e}")
            });
        assert!(path.is_file());
    }
}

#[tokio::test]
async fn byte_exact_original_download_verifies_size_decode_headers_and_releases_semaphore() {
    let server = MockServer::start().await;
    let png = encode_png(48, 32, [30, 144, 255, 255]);
    Mock::given(method("GET"))
        .and(path("/full/ab/wallhaven-abc123.png"))
        .and(|req: &wiremock::Request| {
            !req.headers.contains_key("x-api-key") && !req.headers.contains_key("referer")
        })
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let client = Arc::new(test_client(&server.uri()));
    let downloader = OriginalDownloader::new(
        Arc::clone(&client),
        ImageGuard::new(),
        Arc::new(HardLinkFilePublisher),
    );
    let dir = TempDir::new("wallora-dl-exact");
    let wallpaper = sample_wallpaper(
        "abc123",
        &format!("{}/full/ab/wallhaven-abc123.png", server.uri()),
        png.len() as u64,
        48,
        32,
    );
    let dest = dir.0.join(default_download_filename(&wallpaper));
    let req = DownloadRequest::for_wallpaper(&wallpaper, dest.clone()).unwrap();

    let outcome = downloader
        .download(req, CancelToken::new())
        .await
        .expect("byte-exact download succeeds");

    assert_eq!(outcome.path, dest);
    assert_eq!(outcome.bytes_written, png.len() as u64);
    assert_eq!(outcome.dimensions, (48, 32));
    assert!(!outcome.collision_renamed);
    assert_eq!(std::fs::read(&dest).unwrap(), png);
    assert_eq!(
        list_entries(&dir.0),
        BTreeSet::from(["wallhaven-abc123.png".to_string()])
    );
    assert_eq!(client.available_image_slots_for_test(), IMAGE_IN_FLIGHT_CAP);
}

#[tokio::test]
async fn original_stream_is_not_capped_at_thumbnail_or_preview_limits_and_allows_same_origin_redirect()
 {
    let server = MockServer::start().await;
    let evil = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&evil)
        .await;

    // 2.5 MB textured PNG exceeds THUMBNAIL_BYTE_LIMIT (2 MB) and decodes cleanly under Preview.
    let big_png = encode_textured_png(1200, 900);
    assert!(
        big_png.len() > 2_000_000,
        "textured PNG should exceed 2 MB thumbnail cap (got {})",
        big_png.len()
    );

    Mock::given(method("GET"))
        .and(path("/full/redirect.png"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", &format!("{}/full/final.png", server.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/full/final.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(big_png.clone()))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/full/cross.png"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", &format!("{}/evil.png", evil.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = Arc::new(test_client(&server.uri()));
    let downloader = OriginalDownloader::new(
        Arc::clone(&client),
        ImageGuard::new(),
        Arc::new(HardLinkFilePublisher),
    );
    let dir = TempDir::new("wallora-dl-redirect");

    let ok_wp = sample_wallpaper(
        "big1",
        &format!("{}/full/redirect.png", server.uri()),
        big_png.len() as u64,
        1200,
        900,
    );
    let out = downloader
        .download(
            DownloadRequest::for_wallpaper(&ok_wp, dir.0.join("big1.png")).unwrap(),
            CancelToken::new(),
        )
        .await
        .expect("same-origin redirect and >2MB original download succeed");
    assert_eq!(std::fs::read(&out.path).unwrap(), big_png);

    let cross_wp = sample_wallpaper(
        "cross1",
        &format!("{}/full/cross.png", server.uri()),
        10,
        1,
        1,
    );
    let err = downloader
        .download(
            DownloadRequest::for_wallpaper(&cross_wp, dir.0.join("cross.png")).unwrap(),
            CancelToken::new(),
        )
        .await
        .expect_err("cross-origin redirect must fail");
    assert!(matches!(
        err,
        DownloadError::Provider {
            source: ProviderError::OriginRejected,
            part_path: None
        }
    ));
    assert!(
        !dir.0.join("cross.png").exists(),
        "cross-origin target must not exist"
    );
}

#[tokio::test]
async fn original_stream_chunked_exceeds_64mb_preview_limit_without_capping() {
    // 65 MB streamed in chunks directly via open_original_stream proves the original stream path
    // is NOT capped at the 2 MB thumbnail or 64 MB preview byte ceilings.
    const TOTAL_BYTES: usize = 65 * 1024 * 1024;
    const CHUNK_SIZE: usize = 1024 * 1024;

    let (listener, addr) = support::bind().await;
    let _server = support::serve(listener, |mut stream| async move {
        let _ = support::read_request_head(&mut stream).await;
        let _ = support::write_head(
            &mut stream,
            &[
                ("Content-Type", "application/octet-stream"),
                ("Transfer-Encoding", "chunked"),
                ("Connection", "close"),
            ],
        )
        .await;
        let chunk = vec![0x5Au8; CHUNK_SIZE];
        for _ in 0..(TOTAL_BYTES / CHUNK_SIZE) {
            if support::write_chunk(&mut stream, &chunk).await.is_err() {
                return;
            }
        }
        let _ = support::finish_chunks(&mut stream).await;
        let _ = stream.shutdown().await;
    });

    let client = test_client(&format!("http://{addr}"));
    let url = Url::parse(&format!("http://{addr}/full/huge.bin")).unwrap();
    let mut stream = client
        .open_original_stream(&url)
        .await
        .expect("stream opens cleanly");

    let mut total = 0usize;
    while let Some(chunk) = stream.next_chunk().await.expect("chunk succeeds") {
        total += chunk.len();
    }
    drop(stream);

    assert_eq!(total, TOTAL_BYTES);
    assert_eq!(client.available_image_slots_for_test(), IMAGE_IN_FLIGHT_CAP);
}

#[tokio::test]
async fn abrupt_server_disconnect_mid_stream_leaves_only_partial_file() {
    let png = encode_png(24, 24, [70, 80, 90, 255]);
    let split = png.len() / 2;
    let first_half = png[..split].to_vec();
    let total_len = png.len();

    let (listener, addr) = support::bind().await;
    let first_tx = first_half.clone();
    let _server = support::serve(listener, move |mut stream| {
        let first_tx = first_tx.clone();
        async move {
            let _ = support::read_request_head(&mut stream).await;
            let len_str = total_len.to_string();
            let _ = support::write_head(
                &mut stream,
                &[
                    ("Content-Type", "image/png"),
                    ("Content-Length", &len_str),
                    ("Connection", "close"),
                ],
            )
            .await;
            let _ = stream.write_all(&first_tx).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    });

    let client = Arc::new(test_client(&format!("http://{addr}")));
    let downloader =
        OriginalDownloader::new(client, ImageGuard::new(), Arc::new(HardLinkFilePublisher));
    let dir = TempDir::new("wallora-dl-disconnect");
    let target = dir.0.join("truncated.png");
    let req = DownloadRequest {
        url: Url::parse(&format!("http://{addr}/full/truncated.png")).unwrap(),
        destination: target.clone(),
        expected_file_size: Some(png.len() as u64),
        expected_dimensions: Some((24, 24)),
    };

    let err = downloader
        .download(req, CancelToken::new())
        .await
        .expect_err("truncated stream must fail");

    let part_path = match &err {
        DownloadError::Provider {
            part_path: Some(p), ..
        } => p.clone(),
        other => panic!("expected Provider interruption with part_path, got {other:?}"),
    };
    assert!(!target.exists());
    assert!(part_path.exists());
    assert_eq!(std::fs::read(&part_path).unwrap(), first_half);
}

#[tokio::test]
async fn rejects_size_mismatch_dimension_mismatch_and_invalid_image_without_publishing() {
    let server = MockServer::start().await;
    let png = encode_png(16, 16, [10, 20, 30, 255]);

    Mock::given(method("GET"))
        .and(path("/full/valid.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/full/corrupt.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not-a-valid-png".as_slice()))
        .mount(&server)
        .await;

    let client = Arc::new(test_client(&server.uri()));
    let downloader =
        OriginalDownloader::new(client, ImageGuard::new(), Arc::new(HardLinkFilePublisher));
    let dir = TempDir::new("wallora-dl-mismatch");

    // 1. Expected size larger than actual -> SizeMismatch
    let short_req = DownloadRequest {
        url: Url::parse(&format!("{}/full/valid.png", server.uri())).unwrap(),
        destination: dir.0.join("short.png"),
        expected_file_size: Some(png.len() as u64 + 10),
        expected_dimensions: Some((16, 16)),
    };
    let err = downloader
        .download(short_req, CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        DownloadError::SizeMismatch {
            expected: png.len() as u64 + 10,
            actual: png.len() as u64,
        }
    );

    // 2. Expected size smaller than actual -> SizeMismatch
    let long_req = DownloadRequest {
        url: Url::parse(&format!("{}/full/valid.png", server.uri())).unwrap(),
        destination: dir.0.join("long.png"),
        expected_file_size: Some(png.len() as u64 - 1),
        expected_dimensions: Some((16, 16)),
    };
    let err = downloader
        .download(long_req, CancelToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::SizeMismatch { .. }));

    // 3. Dimension mismatch -> DimensionMismatch
    let dim_req = DownloadRequest {
        url: Url::parse(&format!("{}/full/valid.png", server.uri())).unwrap(),
        destination: dir.0.join("dims.png"),
        expected_file_size: Some(png.len() as u64),
        expected_dimensions: Some((32, 32)),
    };
    let err = downloader
        .download(dim_req, CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        DownloadError::DimensionMismatch {
            expected: (32, 32),
            actual: (16, 16),
        }
    );

    // 4. Corrupt payload -> InvalidImage
    let corrupt_req = DownloadRequest {
        url: Url::parse(&format!("{}/full/corrupt.png", server.uri())).unwrap(),
        destination: dir.0.join("corrupt.png"),
        expected_file_size: Some(15),
        expected_dimensions: None,
    };
    let err = downloader
        .download(corrupt_req, CancelToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::InvalidImage(_)));

    assert!(
        list_entries(&dir.0).is_empty(),
        "failed verification must not publish target or leave stale files"
    );
}

struct FailOnNthWriteFs {
    inner: RealDownloadFs,
    writes_remaining: Arc<AtomicUsize>,
}

struct FailOnNthPartWriter {
    inner: Box<dyn PartWriter>,
    writes_remaining: Arc<AtomicUsize>,
}

impl PartWriter for FailOnNthPartWriter {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let prev = self.writes_remaining.load(Ordering::SeqCst);
        if prev == 0 {
            return Err(std::io::Error::new(
                ErrorKind::StorageFull,
                "simulated zero disk space",
            ));
        }
        self.writes_remaining.fetch_sub(1, Ordering::SeqCst);
        self.inner.write_all(bytes)
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        self.inner.finish()
    }
}

impl DownloadFs for FailOnNthWriteFs {
    fn create_part(&self, path: &Path) -> std::io::Result<Box<dyn PartWriter>> {
        let inner = self.inner.create_part(path)?;
        Ok(Box::new(FailOnNthPartWriter {
            inner,
            writes_remaining: Arc::clone(&self.writes_remaining),
        }))
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
}

#[tokio::test]
async fn zero_disk_space_mid_stream_leaves_only_partial_and_never_publishes_target() {
    let png = encode_png(32, 32, [200, 100, 50, 255]);
    let split = png.len() / 2;
    let first_half = png[..split].to_vec();
    let second_half = png[split..].to_vec();

    let (listener, addr) = support::bind().await;
    let first_tx = first_half.clone();
    let second_tx = second_half.clone();
    let _server = support::serve(listener, move |mut stream| {
        let first_tx = first_tx.clone();
        let second_tx = second_tx.clone();
        async move {
            let _ = support::read_request_head(&mut stream).await;
            let len_str = (first_tx.len() + second_tx.len()).to_string();
            let _ = support::write_head(
                &mut stream,
                &[
                    ("Content-Type", "image/png"),
                    ("Content-Length", &len_str),
                    ("Connection", "close"),
                ],
            )
            .await;
            let _ = stream.write_all(&first_tx).await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = stream.write_all(&second_tx).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    });

    let client = Arc::new(test_client(&format!("http://{addr}")));
    let fs = Arc::new(FailOnNthWriteFs {
        inner: RealDownloadFs,
        writes_remaining: Arc::new(AtomicUsize::new(1)),
    });
    let downloader = OriginalDownloader::with_fs(
        client,
        ImageGuard::new(),
        Arc::new(HardLinkFilePublisher),
        fs,
    );
    let dir = TempDir::new("wallora-dl-diskfull");
    let target = dir.0.join("diskfull.png");
    let req = DownloadRequest {
        url: Url::parse(&format!("http://{addr}/full/diskfull.png")).unwrap(),
        destination: target.clone(),
        expected_file_size: Some(png.len() as u64),
        expected_dimensions: Some((32, 32)),
    };

    let err = downloader
        .download(req, CancelToken::new())
        .await
        .expect_err("zero disk space must fail");

    let part_path = match &err {
        DownloadError::DiskFull {
            part_path: Some(p), ..
        } => p.clone(),
        other => panic!("expected DiskFull with part_path, got {other:?}"),
    };
    assert!(!target.exists(), "target file must not be published");
    assert!(part_path.exists(), "partial file should remain on disk");
    let file_name = part_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(file_name.starts_with(".diskfull.png."));
    assert!(file_name.ends_with(".part"));
    assert_eq!(list_entries(&dir.0), BTreeSet::from([file_name]));
    assert_eq!(std::fs::read(&part_path).unwrap(), first_half);
}

#[tokio::test]
async fn cancellation_mid_stream_leaves_only_partial_and_never_publishes_target() {
    let png = encode_png(24, 24, [12, 34, 56, 255]);
    let split = png.len() / 2;
    let first_half = png[..split].to_vec();

    let (listener, addr) = support::bind().await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let server_gate = Arc::clone(&gate);
    let first_tx = first_half.clone();
    let total_len = png.len();
    let _server = support::serve(listener, move |mut stream| {
        let gate = Arc::clone(&server_gate);
        let first_tx = first_tx.clone();
        async move {
            let _ = support::read_request_head(&mut stream).await;
            let len_str = total_len.to_string();
            let _ = support::write_head(
                &mut stream,
                &[
                    ("Content-Type", "image/png"),
                    ("Content-Length", &len_str),
                    ("Connection", "close"),
                ],
            )
            .await;
            let _ = stream.write_all(&first_tx).await;
            let _ = stream.flush().await;
            gate.notified().await;
        }
    });

    let client = Arc::new(test_client(&format!("http://{addr}")));
    let downloader =
        OriginalDownloader::new(client, ImageGuard::new(), Arc::new(HardLinkFilePublisher));
    let dir = TempDir::new("wallora-dl-cancel");
    let target = dir.0.join("interrupted.png");
    let req = DownloadRequest {
        url: Url::parse(&format!("http://{addr}/full/interrupted.png")).unwrap(),
        destination: target.clone(),
        expected_file_size: Some(png.len() as u64),
        expected_dimensions: Some((24, 24)),
    };

    let (prog_tx, mut prog_rx) = mpsc::unbounded_channel::<DownloadProgress>();
    let cancel = CancelToken::new();
    let cancel_for_dl = cancel.clone();
    let handle = tokio::spawn(async move {
        downloader
            .download_with_progress(req, cancel_for_dl, Some(prog_tx))
            .await
    });

    let progress = tokio::time::timeout(Duration::from_secs(5), prog_rx.recv())
        .await
        .expect("progress arrives after first chunk")
        .expect("progress item");
    assert_eq!(progress.bytes_written, first_half.len() as u64);

    cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("download stops promptly on cancel")
        .expect("join task")
        .expect_err("cancelled download returns error");
    gate.notify_waiters();

    let part_path = match &err {
        DownloadError::Cancelled { part_path: Some(p) } => p.clone(),
        other => panic!("expected Cancelled with part_path, got {other:?}"),
    };
    assert!(!target.exists());
    assert!(part_path.exists());
    let file_name = part_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(file_name.starts_with(".interrupted.png."));
    assert!(file_name.ends_with(".part"));
    assert_eq!(list_entries(&dir.0), BTreeSet::from([file_name]));
    assert_eq!(std::fs::read(&part_path).unwrap(), first_half);
}

#[tokio::test]
async fn collision_1_target_exists_before_download_starts() {
    let server = MockServer::start().await;
    let png = encode_png(20, 20, [90, 180, 45, 255]);
    Mock::given(method("GET"))
        .and(path("/full/col1.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new("wallora-dl-col1");
    let target = dir.0.join("wallhaven-col1.png");
    let existing_bytes = b"existing-user-file-before-start";
    std::fs::write(&target, existing_bytes).unwrap();

    let client = Arc::new(test_client(&server.uri()));
    let downloader =
        OriginalDownloader::new(client, ImageGuard::new(), Arc::new(HardLinkFilePublisher));
    let wp = sample_wallpaper(
        "col1",
        &format!("{}/full/col1.png", server.uri()),
        png.len() as u64,
        20,
        20,
    );
    let req = DownloadRequest::for_wallpaper(&wp, target.clone()).unwrap();

    let outcome = downloader
        .download(req, CancelToken::new())
        .await
        .expect("collision publishes numbered file");

    let expected_renamed = dir.0.join("wallhaven-col1 (1).png");
    assert_eq!(outcome.path, expected_renamed);
    assert!(outcome.collision_renamed);
    assert_eq!(std::fs::read(&target).unwrap(), existing_bytes);
    assert_eq!(std::fs::read(&expected_renamed).unwrap(), png);
    assert_eq!(
        list_entries(&dir.0),
        BTreeSet::from([
            "wallhaven-col1.png".to_string(),
            "wallhaven-col1 (1).png".to_string()
        ])
    );
}

#[tokio::test]
async fn collision_2_target_created_during_download_before_publish() {
    let png = encode_png(22, 22, [15, 75, 195, 255]);
    let split = png.len() / 2;
    let first_half = png[..split].to_vec();
    let second_half = png[split..].to_vec();

    let (listener, addr) = support::bind().await;
    let release = Arc::new(tokio::sync::Notify::new());
    let server_release = Arc::clone(&release);
    let first_tx = first_half.clone();
    let second_tx = second_half.clone();
    let total_len = png.len();

    let _server = support::serve(listener, move |mut stream| {
        let release = Arc::clone(&server_release);
        let first_tx = first_tx.clone();
        let second_tx = second_tx.clone();
        async move {
            let _ = support::read_request_head(&mut stream).await;
            let len_str = total_len.to_string();
            let _ = support::write_head(
                &mut stream,
                &[
                    ("Content-Type", "image/png"),
                    ("Content-Length", &len_str),
                    ("Connection", "close"),
                ],
            )
            .await;
            let _ = stream.write_all(&first_tx).await;
            let _ = stream.flush().await;
            release.notified().await;
            let _ = stream.write_all(&second_tx).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    });

    let dir = TempDir::new("wallora-dl-col2");
    let target = dir.0.join("wallhaven-col2.png");
    let client = Arc::new(test_client(&format!("http://{addr}")));
    let downloader =
        OriginalDownloader::new(client, ImageGuard::new(), Arc::new(HardLinkFilePublisher));
    let req = DownloadRequest {
        url: Url::parse(&format!("http://{addr}/full/col2.png")).unwrap(),
        destination: target.clone(),
        expected_file_size: Some(png.len() as u64),
        expected_dimensions: Some((22, 22)),
    };

    let (prog_tx, mut prog_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        downloader
            .download_with_progress(req, CancelToken::new(), Some(prog_tx))
            .await
    });

    // Wait until chunk 1 has landed in the .part file, then create the target file on disk.
    let progress = tokio::time::timeout(Duration::from_secs(5), prog_rx.recv())
        .await
        .expect("chunk 1 progress")
        .expect("progress");
    assert_eq!(progress.bytes_written, first_half.len() as u64);
    assert!(!target.exists());

    let racing_bytes = b"created-mid-download-must-not-be-clobbered";
    std::fs::write(&target, racing_bytes).unwrap();
    release.notify_one();

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("download completes")
        .expect("join")
        .expect("publish resolves collision without clobbering");

    let expected_renamed = dir.0.join("wallhaven-col2 (1).png");
    assert_eq!(outcome.path, expected_renamed);
    assert!(outcome.collision_renamed);
    assert_eq!(std::fs::read(&target).unwrap(), racing_bytes);
    assert_eq!(std::fs::read(&expected_renamed).unwrap(), png);
    assert_eq!(
        list_entries(&dir.0),
        BTreeSet::from([
            "wallhaven-col2.png".to_string(),
            "wallhaven-col2 (1).png".to_string()
        ])
    );
}

struct BarrierFirstPublishPublisher {
    barrier: Arc<Barrier>,
    first_attempts: AtomicUsize,
}

impl FilePublisher for BarrierFirstPublishPublisher {
    fn publish_no_clobber(&self, src: &Path, dst: &Path) -> Result<(), PublishError> {
        let is_first_for_caller = self.first_attempts.fetch_add(1, Ordering::SeqCst) < 2;
        if is_first_for_caller {
            self.barrier.wait();
        }
        HardLinkFilePublisher.publish_no_clobber(src, dst)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collision_3_two_concurrent_downloads_same_id_produce_distinct_byte_exact_files() {
    let server = MockServer::start().await;
    let png = encode_png(28, 18, [255, 115, 22, 255]);
    Mock::given(method("GET"))
        .and(path("/full/race.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .expect(2)
        .mount(&server)
        .await;

    let dir = TempDir::new("wallora-dl-col3");
    let target = dir.0.join("wallhaven-race.png");
    let client = Arc::new(test_client(&server.uri()));
    let publisher = Arc::new(BarrierFirstPublishPublisher {
        barrier: Arc::new(Barrier::new(2)),
        first_attempts: AtomicUsize::new(0),
    });
    let downloader = OriginalDownloader::new(client, ImageGuard::new(), publisher);
    let wp = sample_wallpaper(
        "race",
        &format!("{}/full/race.png", server.uri()),
        png.len() as u64,
        28,
        18,
    );

    let req1 = DownloadRequest::for_wallpaper(&wp, target.clone()).unwrap();
    let req2 = DownloadRequest::for_wallpaper(&wp, target.clone()).unwrap();
    let dl1 = downloader.clone();
    let dl2 = downloader.clone();

    let (res1, res2) = tokio::join!(
        tokio::spawn(async move { dl1.download(req1, CancelToken::new()).await }),
        tokio::spawn(async move { dl2.download(req2, CancelToken::new()).await }),
    );
    let out1 = res1.unwrap().expect("download 1 succeeds");
    let out2 = res2.unwrap().expect("download 2 succeeds");

    assert_ne!(out1.path, out2.path);
    let expected_first = dir.0.join("wallhaven-race.png");
    let expected_second = dir.0.join("wallhaven-race (1).png");
    let paths: BTreeSet<PathBuf> = BTreeSet::from([out1.path.clone(), out2.path.clone()]);
    assert_eq!(
        paths,
        BTreeSet::from([expected_first.clone(), expected_second.clone()])
    );
    assert!(out1.collision_renamed ^ out2.collision_renamed);
    assert_eq!(std::fs::read(&expected_first).unwrap(), png);
    assert_eq!(std::fs::read(&expected_second).unwrap(), png);
    assert_eq!(
        list_entries(&dir.0),
        BTreeSet::from([
            "wallhaven-race.png".to_string(),
            "wallhaven-race (1).png".to_string()
        ])
    );
}

#[tokio::test]
async fn reserved_destination_filename_is_sanitized_before_creating_files() {
    let server = MockServer::start().await;
    let png = encode_png(12, 12, [100, 200, 150, 255]);
    Mock::given(method("GET"))
        .and(path("/full/resv.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .mount(&server)
        .await;

    let dir = TempDir::new("wallora-dl-reserved");
    let client = Arc::new(test_client(&server.uri()));
    let downloader =
        OriginalDownloader::new(client, ImageGuard::new(), Arc::new(HardLinkFilePublisher));
    let req = DownloadRequest {
        url: Url::parse(&format!("{}/full/resv.png", server.uri())).unwrap(),
        destination: dir.0.join("CON.png"),
        expected_file_size: Some(png.len() as u64),
        expected_dimensions: Some((12, 12)),
    };

    let outcome = downloader.download(req, CancelToken::new()).await.unwrap();
    assert_eq!(outcome.path, dir.0.join("_CON.png"));
    assert_eq!(std::fs::read(&outcome.path).unwrap(), png);
}

fn adversarial_filename_strategy() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("CON".to_string()),
        Just("nul".to_string()),
        Just("COM1".to_string()),
        Just("LPT9".to_string()),
        Just("..".to_string()),
        Just(".".to_string()),
        Just("/".to_string()),
        Just("\\".to_string()),
        Just(":".to_string()),
        Just(" ".to_string()),
        Just("<>\"|?*".to_string()),
        Just("\0\n\x1f".to_string()),
        "[a-zA-Z0-9._ -]{1,20}",
    ];
    prop::collection::vec(piece, 0..10).prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn proptest_sanitize_filename_always_produces_windows_safe_single_segment(raw in any::<String>()) {
        let out = sanitize_filename(&raw);
        assert_safe_windows_filename(&out);
    }

    #[test]
    fn proptest_sanitize_filename_neutralizes_adversarial_combinations(raw in adversarial_filename_strategy()) {
        let out = sanitize_filename(&raw);
        assert_safe_windows_filename(&out);

        let (stem, ext) = split_sanitized_stem_ext(&out);
        let numbered = numbered_collision_name(&stem, ext.as_deref(), 1);
        assert_safe_windows_filename(&numbered);
    }
}
