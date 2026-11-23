use std::io::Cursor;
use std::time::Duration;

use image::{ImageFormat, Rgba, RgbaImage};
use proptest::prelude::*;
use wallora_core::provider::ImageClass;
use wallora_core::provider::client::{PREVIEW_BYTE_LIMIT, THUMBNAIL_BYTE_LIMIT};
use wallora_core::services::{
    ImageGuard, ImageGuardConfig, ImageGuardError, PREVIEW_MAX_PIXELS, PREVIEW_MAX_SIDE,
    THUMBNAIL_MAX_SIDE,
};

fn encode_rgba_png(width: u32, height: u32, fill: [u8; 4]) -> Vec<u8> {
    let img = RgbaImage::from_pixel(width, height, Rgba(fill));
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode synthetic PNG");
    out
}

/// Builds a minimal valid PNG header + IHDR + minimal IDAT + IEND with arbitrary `(width, height)`
/// without allocating a pixel buffer.
fn craft_png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_png_chunk(&mut out, *b"IHDR", &ihdr);
    // Minimal zlib stream (empty deflate block) inside IDAT so header parsers that peek at IDAT
    // see a valid chunk sequence.
    write_png_chunk(
        &mut out,
        *b"IDAT",
        &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    write_png_chunk(&mut out, *b"IEND", &[]);
    out
}

fn write_png_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32_ieee(&kind, data);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32_ieee(kind: &[u8; 4], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Builds a 54-byte BMP header with arbitrary `(width, height)` without allocating pixels.
fn craft_bmp_with_dimensions(width: u32, height: u32) -> Vec<u8> {
    let mut bmp = vec![0u8; 54];
    bmp[0] = b'B';
    bmp[1] = b'M';
    // file size = 54
    bmp[2..6].copy_from_slice(&54u32.to_le_bytes());
    // pixel data offset = 54
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    // DIB header size = 40 (BITMAPINFOHEADER)
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bmp[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    // planes = 1
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    // bits per pixel = 32
    bmp[28..30].copy_from_slice(&32u16.to_le_bytes());
    bmp
}

/// Builds a GIF whose Logical Screen Descriptor claims `8x8` (passes header check) while its
/// Image Descriptor requests `frame_width x frame_height` (caught by `image::Limits` layer).
fn craft_gif_with_oversized_frame(frame_width: u16, frame_height: u16) -> Vec<u8> {
    let mut gif = Vec::new();
    gif.extend_from_slice(b"GIF89a");
    // Logical screen descriptor: 8x8, global color table (2 entries), bg=0, aspect=0
    gif.extend_from_slice(&8u16.to_le_bytes());
    gif.extend_from_slice(&8u16.to_le_bytes());
    gif.push(0x80); // GCT flag=1, color res=0, sort=0, size=0 (2 colors)
    gif.push(0);
    gif.push(0);
    // Global color table: 2 RGB entries
    gif.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
    // Image descriptor: left=0, top=0, width=frame_width, height=frame_height, packed=0
    gif.push(0x2C);
    gif.extend_from_slice(&0u16.to_le_bytes());
    gif.extend_from_slice(&0u16.to_le_bytes());
    gif.extend_from_slice(&frame_width.to_le_bytes());
    gif.extend_from_slice(&frame_height.to_le_bytes());
    gif.push(0);
    // LZW minimum code size + empty image data + trailer
    gif.push(2);
    gif.push(2);
    gif.extend_from_slice(&[0x4C, 0x01]);
    gif.push(0);
    gif.push(0x3B);
    gif
}

#[test]
fn header_check_enforces_exact_thumbnail_and_preview_boundaries() {
    // Thumbnail boundary: 1024x1024 allowed, 1025 rejected.
    let thumb_ok = craft_png_with_dimensions(THUMBNAIL_MAX_SIDE, THUMBNAIL_MAX_SIDE);
    let header = ImageGuard::inspect_bytes(&thumb_ok, ImageClass::Thumbnail)
        .expect("1024x1024 thumbnail header should pass");
    assert_eq!(header.width, THUMBNAIL_MAX_SIDE);
    assert_eq!(header.height, THUMBNAIL_MAX_SIDE);
    assert_eq!(header.decoded_rgba_bytes, 1024 * 1024 * 4);

    for (w, h) in [(THUMBNAIL_MAX_SIDE + 1, 1), (1, THUMBNAIL_MAX_SIDE + 1)] {
        let oversized = craft_png_with_dimensions(w, h);
        let err = ImageGuard::inspect_bytes(&oversized, ImageClass::Thumbnail)
            .expect_err("thumbnail side > 1024 must be rejected");
        assert!(
            matches!(
                err,
                ImageGuardError::DimensionsExceeded { .. } | ImageGuardError::DecoderLimit(_)
            ),
            "unexpected error for {w}x{h}: {err:?}"
        );
    }

    // Preview boundary: 16_384 x 9_155 = 149_995_520 px <= 150 MP passes; +1 row exceeds 150 MP.
    let preview_ok = craft_png_with_dimensions(PREVIEW_MAX_SIDE, 9_155);
    let header = ImageGuard::inspect_bytes(&preview_ok, ImageClass::Preview)
        .expect("149.99 MP preview within 16,384px side should pass header check");
    assert_eq!(header.decoded_rgba_bytes, 16_384 * 9_155 * 4);

    let preview_over_mp = craft_png_with_dimensions(PREVIEW_MAX_SIDE, 9_156);
    assert!(matches!(
        ImageGuard::inspect_bytes(&preview_over_mp, ImageClass::Preview),
        Err(ImageGuardError::MegapixelsExceeded {
            max_pixels: PREVIEW_MAX_PIXELS,
            ..
        })
    ));

    let preview_over_side = craft_png_with_dimensions(PREVIEW_MAX_SIDE + 1, 1);
    assert!(matches!(
        ImageGuard::inspect_bytes(&preview_over_side, ImageClass::Preview),
        Err(ImageGuardError::DimensionsExceeded { .. } | ImageGuardError::DecoderLimit(_))
    ));
}

#[tokio::test]
async fn rejects_empty_garbage_zero_dimension_and_oversized_encoded_payloads() {
    let guard = ImageGuard::new();

    assert_eq!(
        guard
            .decode_bytes(Vec::new(), ImageClass::Thumbnail)
            .await
            .unwrap_err(),
        ImageGuardError::EmptyPayload
    );

    assert_eq!(
        guard
            .decode_bytes(b"not an image at all".to_vec(), ImageClass::Thumbnail)
            .await
            .unwrap_err(),
        ImageGuardError::UnsupportedFormat
    );

    let zero_dim_bmp = craft_bmp_with_dimensions(0, 10);
    let zero_bmp_res = guard
        .decode_bytes(zero_dim_bmp, ImageClass::Thumbnail)
        .await;
    assert!(
        matches!(
            zero_bmp_res,
            Err(ImageGuardError::InvalidDimensions { .. }
                | ImageGuardError::DecodeFailed(_)
                | ImageGuardError::UnsupportedFormat)
        ),
        "unexpected zero-dim bmp result: {zero_bmp_res:?}"
    );

    let zero_dim_png = craft_png_with_dimensions(0, 10);
    let zero_png_res = guard
        .decode_bytes(zero_dim_png, ImageClass::Thumbnail)
        .await;
    assert!(
        matches!(
            zero_png_res,
            Err(ImageGuardError::InvalidDimensions { .. }
                | ImageGuardError::DecodeFailed(_)
                | ImageGuardError::DecoderLimit(_))
        ),
        "unexpected zero-dim png result: {zero_png_res:?}"
    );

    let too_big_thumb = vec![0u8; THUMBNAIL_BYTE_LIMIT + 1];
    assert_eq!(
        guard
            .decode_bytes(too_big_thumb, ImageClass::Thumbnail)
            .await
            .unwrap_err(),
        ImageGuardError::EncodedTooLarge {
            actual: (THUMBNAIL_BYTE_LIMIT + 1) as u64,
            limit: THUMBNAIL_BYTE_LIMIT,
        }
    );

    let too_big_preview = vec![0u8; PREVIEW_BYTE_LIMIT + 1];
    assert_eq!(
        guard
            .decode_bytes(too_big_preview, ImageClass::Preview)
            .await
            .unwrap_err(),
        ImageGuardError::EncodedTooLarge {
            actual: (PREVIEW_BYTE_LIMIT + 1) as u64,
            limit: PREVIEW_BYTE_LIMIT,
        }
    );
}

#[tokio::test]
async fn second_layer_image_limits_rejects_oversized_gif_frame() {
    let guard = ImageGuard::new();
    // Header claims 8x8, but inner frame requests 2048x2048 (> 1024px thumbnail limit).
    let sneaky_gif = craft_gif_with_oversized_frame(2048, 2048);
    let header = ImageGuard::inspect_bytes(&sneaky_gif, ImageClass::Thumbnail)
        .expect("outer 8x8 header passes first layer");
    assert_eq!((header.width, header.height), (8, 8));

    let err = guard
        .decode_bytes(sneaky_gif, ImageClass::Thumbnail)
        .await
        .expect_err("second-layer image::Limits must reject oversized inner frame");
    assert!(
        matches!(
            err,
            ImageGuardError::DecoderLimit(_) | ImageGuardError::DecodeFailed(_)
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        guard.available_decoded_bytes(),
        guard.decoded_bytes_budget()
    );
    assert_eq!(guard.available_decode_permits(), guard.decode_concurrency());
}

#[tokio::test]
async fn decode_bytes_tracks_in_flight_texture_budget_and_waits_for_release() {
    // 10x10 RGBA8 = 400 bytes per image. Budget = 700 bytes -> only 1 fits at a time.
    let guard = ImageGuard::with_config(ImageGuardConfig {
        decode_concurrency: 2,
        decoded_bytes_budget: 700,
    });
    let png = encode_rgba_png(10, 10, [10, 20, 30, 255]);

    let first = guard
        .decode_bytes(png.clone(), ImageClass::Thumbnail)
        .await
        .expect("first image fits in 700-byte budget");
    assert_eq!(first.dimensions(), (10, 10));
    assert_eq!(first.byte_len(), 400);
    assert_eq!(guard.available_decoded_bytes(), 300);

    // A single image requiring more than the total budget is rejected immediately without waiting.
    let too_large_for_budget = encode_rgba_png(20, 20, [1, 2, 3, 255]); // 1600 bytes > 700
    assert_eq!(
        guard
            .decode_bytes(too_large_for_budget, ImageClass::Thumbnail)
            .await
            .unwrap_err(),
        ImageGuardError::DecodedBudgetExceeded {
            required: 1600,
            budget: 700,
        }
    );

    // Second 400-byte image blocks until `first` is dropped.
    let guard_clone = guard.clone();
    let png_clone = png.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let _ = started_tx.send(());
        guard_clone
            .decode_bytes(png_clone, ImageClass::Thumbnail)
            .await
    });

    started_rx.await.expect("spawned task started");
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "second decode must wait while budget is held"
    );

    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("second decode finishes once first drops")
        .expect("join decode task")
        .expect("second decode succeeds");
    assert_eq!(second.byte_len(), 400);
    assert_eq!(guard.available_decoded_bytes(), 300);

    drop(second);
    assert_eq!(guard.available_decoded_bytes(), 700);
}

#[tokio::test]
async fn decode_file_decodes_original_under_preview_budget() {
    let dir = std::env::temp_dir().join(format!("wallora-guard-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("original.png");

    // 1025x10 exceeds Thumbnail max side (1024) but succeeds under Preview budget.
    let png = encode_rgba_png(1025, 10, [40, 80, 120, 255]);
    std::fs::write(&path, &png).expect("write test file");

    let guard = ImageGuard::new();
    assert!(
        guard
            .decode_file(&path, ImageClass::Thumbnail)
            .await
            .is_err()
    );

    let decoded = guard
        .decode_file(&path, ImageClass::Preview)
        .await
        .expect("original file decodes under Preview budget");
    assert_eq!(decoded.dimensions(), (1025, 10));
    assert_eq!(decoded.byte_len(), 1025 * 10 * 4);

    drop(decoded);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn default_decode_concurrency_is_half_cores_minimum_one() {
    let expected = std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .max(1);
    let guard = ImageGuard::new();
    assert_eq!(guard.decode_concurrency(), expected);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_png_and_bmp_oversized_dimensions_rejected_before_allocation(
        wide in (THUMBNAIL_MAX_SIDE + 1)..=u32::MAX,
        tall in 1u32..=u32::MAX,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let guard = ImageGuard::new();

        let png = craft_png_with_dimensions(wide, tall);
        prop_assert!(ImageGuard::inspect_bytes(&png, ImageClass::Thumbnail).is_err());
        prop_assert!(rt.block_on(guard.decode_bytes(png, ImageClass::Thumbnail)).is_err());

        let bmp = craft_bmp_with_dimensions(tall, wide);
        prop_assert!(ImageGuard::inspect_bytes(&bmp, ImageClass::Thumbnail).is_err());
        prop_assert!(rt.block_on(guard.decode_bytes(bmp, ImageClass::Thumbnail)).is_err());
    }

    #[test]
    fn proptest_preview_megapixel_bombs_rejected_before_allocation(
        width in 12_248u32..=PREVIEW_MAX_SIDE,
        height in 12_248u32..=PREVIEW_MAX_SIDE,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let guard = ImageGuard::new();

        // 12_248 * 12_248 = 150_013_504 > 150_000_000 (150 MP), even though each side <= 16_384.
        let png = craft_png_with_dimensions(width, height);
        let res = ImageGuard::inspect_bytes(&png, ImageClass::Preview);
        let is_mp = matches!(res, Err(ImageGuardError::MegapixelsExceeded { .. }));
        prop_assert!(is_mp, "unexpected inspect result: {:?}", res);

        let decode_res = rt.block_on(guard.decode_bytes(png, ImageClass::Preview));
        let is_decode_mp = matches!(decode_res, Err(ImageGuardError::MegapixelsExceeded { .. }));
        prop_assert!(is_decode_mp, "unexpected decode result: {:?}", decode_res);
    }

    #[test]
    fn proptest_valid_small_pngs_decode_exact_rgba(
        width in 1u32..=32,
        height in 1u32..=32,
        r in any::<u8>(),
        g in any::<u8>(),
        b in any::<u8>(),
        a in any::<u8>(),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let guard = ImageGuard::new();
        let png = encode_rgba_png(width, height, [r, g, b, a]);
        let decoded = rt
            .block_on(guard.decode_bytes(png, ImageClass::Thumbnail))
            .expect("valid small PNG should decode");
        prop_assert_eq!(decoded.dimensions(), (width, height));
        prop_assert_eq!(decoded.byte_len(), (width as usize) * (height as usize) * 4);
    }
}
