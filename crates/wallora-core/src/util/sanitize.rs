use url::Url;

use crate::domain::Wallpaper;

/// Fallback filename when sanitization reduces an input to an empty string.
pub const DEFAULT_FALLBACK_NAME: &str = "wallpaper";

/// Maximum sanitized filename length in UTF-16 code units, leaving headroom under Windows's
/// 255-unit component limit for collision suffixes (` (N)`) and `.{name}.{uuid}.part` files.
pub const MAX_FILENAME_UTF16_LEN: usize = 180;

/// Returns `true` if `name` (or its stem before the first `.` after trimming trailing whitespace)
/// matches a Windows reserved DOS device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`..`COM9`,
/// `LPT1`..`LPT9`, case-insensitive).
pub fn is_windows_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let trimmed = stem.trim_end_matches(|c: char| c.is_whitespace());
    if trimmed.len() < 3 || trimmed.len() > 4 || !trimmed.is_ascii() {
        return false;
    }
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn is_forbidden_windows_char(c: char) -> bool {
    c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

fn collapse_dot_runs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut dot_run = 0usize;
    for ch in input.chars() {
        if ch == '.' {
            dot_run += 1;
        } else {
            if dot_run == 1 {
                out.push('.');
            } else if dot_run > 1 {
                out.push('_');
            }
            dot_run = 0;
            out.push(ch);
        }
    }
    if dot_run == 1 {
        out.push('.');
    } else if dot_run > 1 {
        out.push('_');
    }
    out
}

fn truncate_to_utf16(s: &str, max_units: usize) -> &str {
    let mut units = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let ch_units = ch.len_utf16();
        if units + ch_units > max_units {
            break;
        }
        units += ch_units;
        end = idx + ch.len_utf8();
    }
    &s[..end]
}

fn neutralize_reserved(mut name: String) -> String {
    if is_windows_reserved_device_name(&name) {
        name.insert(0, '_');
    }
    name
}

fn finalize_with_max_utf16(raw: &str, max_utf16: usize) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| if is_forbidden_windows_char(c) { '_' } else { c })
        .collect();
    let trimmed_outer = mapped.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let collapsed = collapse_dot_runs(trimmed_outer);
    let mut clean = collapsed
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string();

    if clean.is_empty() {
        clean = DEFAULT_FALLBACK_NAME.to_string();
    }
    clean = neutralize_reserved(clean);

    if clean.encode_utf16().count() > max_utf16 {
        let split = clean.rsplit_once('.').and_then(|(stem, ext)| {
            let ext_units = ext.encode_utf16().count();
            (!stem.is_empty()
                && (1..=16).contains(&ext_units)
                && !ext.chars().any(char::is_whitespace))
            .then_some((stem, ext, ext_units))
        });

        clean = if let Some((stem, ext, ext_units)) = split {
            // Reserve 1 UTF-16 unit for `neutralize_reserved` plus `.ext`.
            let stem_budget = max_utf16.saturating_sub(ext_units + 2);
            if stem_budget >= 1 {
                let mut short_stem = truncate_to_utf16(stem, stem_budget)
                    .trim_matches(|c: char| c == '.' || c.is_whitespace())
                    .to_string();
                if short_stem.is_empty() {
                    short_stem = truncate_to_utf16(DEFAULT_FALLBACK_NAME, stem_budget).to_string();
                    if short_stem.is_empty() {
                        short_stem.push('w');
                    }
                }
                short_stem = neutralize_reserved(short_stem);
                format!("{short_stem}.{ext}")
            } else {
                let mut short = truncate_to_utf16(&clean, max_utf16.saturating_sub(1))
                    .trim_matches(|c: char| c == '.' || c.is_whitespace())
                    .to_string();
                if short.is_empty() {
                    short = DEFAULT_FALLBACK_NAME.to_string();
                }
                neutralize_reserved(short)
            }
        } else {
            let mut short = truncate_to_utf16(&clean, max_utf16.saturating_sub(1))
                .trim_matches(|c: char| c == '.' || c.is_whitespace())
                .to_string();
            if short.is_empty() {
                short = DEFAULT_FALLBACK_NAME.to_string();
            }
            neutralize_reserved(short)
        };
    }

    clean
}

/// Sanitizes any server-derived or user-suggested single filename for safe use on Windows.
///
/// Neutralizes Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`..`COM9`,
/// `LPT1`..`LPT9`, with or without extensions), trailing dots/spaces, path separators, traversal
/// (`..`, `.`), drive/ADS colons, wildcard/forbidden characters, and control characters.
pub fn sanitize_filename(raw: &str) -> String {
    finalize_with_max_utf16(raw, MAX_FILENAME_UTF16_LEN)
}

/// Splits a sanitized filename into `(stem, Some(ext))` or `(sanitized, None)`.
pub fn split_sanitized_stem_ext(filename: &str) -> (String, Option<String>) {
    let clean = sanitize_filename(filename);
    if let Some((stem, ext)) = clean.rsplit_once('.') {
        let trimmed_stem = stem.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
        if !trimmed_stem.is_empty() && !ext.is_empty() {
            return (trimmed_stem.to_string(), Some(ext.to_string()));
        }
    }
    (clean, None)
}

/// Formats a Windows-safe collision-numbered filename such as `<stem> (1).<ext>`.
pub fn numbered_collision_name(stem: &str, ext: Option<&str>, n: u32) -> String {
    let clean_ext = ext
        .map(|raw| {
            let cleaned: String = raw
                .chars()
                .filter(|c| !is_forbidden_windows_char(*c) && *c != '.' && !c.is_whitespace())
                .collect();
            cleaned
        })
        .filter(|e| !e.is_empty());

    let suffix = match clean_ext {
        Some(ext) => format!(" ({n}).{ext}"),
        None => format!(" ({n})"),
    };
    let suffix_units = suffix.encode_utf16().count();
    let stem_budget = MAX_FILENAME_UTF16_LEN.saturating_sub(suffix_units).max(1);
    let mut clean_stem = finalize_with_max_utf16(stem, stem_budget);
    if clean_stem.encode_utf16().count() > stem_budget {
        clean_stem = truncate_to_utf16(&clean_stem, stem_budget)
            .trim_matches(|c: char| c == '.' || c.is_whitespace())
            .to_string();
        if clean_stem.is_empty() {
            clean_stem = "w".to_string();
        }
        clean_stem = neutralize_reserved(clean_stem);
    }
    sanitize_filename(&format!("{clean_stem}{suffix}"))
}

fn extension_from_path_str(raw_path: &str) -> Option<String> {
    let path_only = if let Ok(parsed) = Url::parse(raw_path) {
        parsed.path().to_string()
    } else {
        raw_path
            .split(['?', '#'])
            .next()
            .unwrap_or(raw_path)
            .to_string()
    };

    let last_segment = path_only.rsplit(['/', '\\']).next()?;
    let (stem, ext) = last_segment.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 10 {
        return None;
    }
    if ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Some(ext.to_ascii_lowercase())
    } else {
        None
    }
}

/// Safely extracts an image extension from `wallpaper.path`, falling back to `wallpaper.file_type`
/// (`image/png` -> `png`, `image/jpeg` -> `jpg`, etc.) or `"bin"`.
pub fn extract_safe_extension(wallpaper: &Wallpaper) -> String {
    if let Some(ext) = extension_from_path_str(&wallpaper.path) {
        return ext;
    }

    match wallpaper.file_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg".to_string(),
        "image/png" => "png".to_string(),
        "image/webp" => "webp".to_string(),
        "image/gif" => "gif".to_string(),
        "image/bmp" => "bmp".to_string(),
        _ => "bin".to_string(),
    }
}

/// Produces the default download filename `wallhaven-{id}.{ext}` sanitized for Windows.
pub fn default_download_filename(wallpaper: &Wallpaper) -> String {
    let ext = extract_safe_extension(wallpaper);
    let clean_id = sanitize_filename(wallpaper.id.as_str()).replace('.', "_");
    sanitize_filename(&format!("wallhaven-{clean_id}.{ext}"))
}
