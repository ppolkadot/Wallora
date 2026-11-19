# Wallhaven Desktop Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a faithful native Windows client for Wallhaven before introducing desktop-exclusive customization.

**Architecture:** A Rust GUI consumes a provider abstraction backed by the official Wallhaven API and explicitly supported web endpoints. Separate modules handle session credentials, disk cache, downloads, and Windows integration; no independent wallpaper mirror or hosted backend is required.

**Tech Stack:** Rust, Iced (proposed), Tokio, Reqwest, Serde, SQLite/rusqlite, image, windows-rs, tracing, GitHub Actions, WiX or NSIS.

**Spec:** The requirements baseline below consolidates the user's decisions from the project discussion. This file is a proposed plan for review, not an assertion that every upstream capability is available.

## Requirements baseline and global constraints

- Windows 10/11, native Rust UI, no JavaScript, TypeScript, WebView, HTML/CSS frontend, or Electron.
- v0.x and v1.0 prioritize faithful behavior and comprehensive website feature coverage. Gates precede desktop-only additions.
- Use official API first; only documented or otherwise explicitly supported, authorized web endpoints for gaps. Do not reverse-engineer private sessions, bypass authentication, evade rate limits, or mirror the catalog.
- Match provider account permissions and purity/category controls; no separate app age checker or app-imposed wallpaper filtering. Never expose content the upstream account cannot access. Keep guest/account sessions separate.
- Preserve original full-resolution downloads; optional automatic filenames and default destination in the later desktop phase.
- Treat online favorites and collections as upstream data; unsupported writes must never pretend to sync. A parity gap blocks declaring full parity.
- Native Windows wallpaper control only; use IDesktopWallpaper, including monitor-specific wallpapers. Keep applied wallpapers available locally without requiring a manual download.
- Later customization: static default theme, Ambient and Adaptive opt-in, dynamic color extraction/blur, automatic theme following selected/current wallpaper, custom fonts, reduced-motion off by default, GPU enabled by default, startup/tray settings, local/stock images, history/undo tab, download manager, simple rotation.
- Thumbnail cache hard cap 10 GB (10,000,000,000 bytes); user can select a lower limit. Do not evict active wallpaper or permanent downloads as thumbnail cache entries.
- Default colors: #1E1C1A, #34302D, #5E5A57, #B0ACA9, #F5F4F2, #F37316, #F7B120, #E84D23. Prefer Salted Flutters Polaris or Google Sans only after checking redistribution rights; fallback to system font.
- Deferred: AI recommendations, new visual similarity engine, smart rotation, desktop preview. Preserve provider's existing similar-wallpaper search if available; it is parity, not a new AI feature.

## Approach decision

Official API only is simpler but may omit required actions. API + supported web endpoints is the selected approach, allowing greater coverage without relying on undocumented behavior. Partnership is not a launch dependency. When all supported routes fail, document the gap, provide an explicitly labeled external browser handoff if useful, and do not call that feature native parity. A release may be labeled beta or partial client, but 1.0 full-parity claims require closure of the matrix.

## Proposed repository structure

```text
Cargo.toml
src/main.rs
src/app.rs
src/domain/{mod.rs,wallpaper.rs,search.rs,account.rs}
src/provider/{mod.rs,api.rs,web.rs,capabilities.rs}
src/services/{mod.rs,session.rs,cache.rs,download.rs,library.rs}
src/platform/{mod.rs,windows.rs}
src/ui/{mod.rs,home.rs,search.rs,detail.rs,account.rs,collections.rs,community.rs,settings.rs,history.rs,downloads.rs,theme.rs}
src/storage/{mod.rs,migrations.rs}
tests/{provider_contract.rs,parity.rs,cache.rs,downloads.rs,windows.rs,appearance.rs}
docs/{parity-matrix.md,api-capabilities.md,architecture.md,privacy.md}
.github/workflows/ci.yml
```

## Review focus

- Expired credentials or logout must invalidate account-specific queries and cached views (Task 3).
- HTTP 429, 403, 404 and transport errors must show actionable errors and avoid repeated requests (Task 2).
- Rapid filter changes must not show stale responses (Task 4).
- Interrupted downloads and malicious filename metadata must not overwrite unrelated files (Task 8).
- Monitor disconnection and deleted prior wallpaper must not crash or silently report successful undo (Task 9).

## Phase A — v0.x through v1.0: faithful website port

### Task 1: Baseline audit and native foundation (v0.1)

Files: Cargo.toml, src/main.rs, src/app.rs, src/domain/mod.rs, docs/parity-matrix.md, docs/api-capabilities.md, docs/architecture.md, .github/workflows/ci.yml.

- [ ] Audit the live website as guest and authenticated test account (with permission). Inventory homepage sections, wallpaper search syntax, every filter and sort, wallpaper details, tags, related search, crop/scale, downloads, collections, profile, settings, upload, moderation/tag actions, forums/comments and other discovered features. Record all observed actions, role restrictions, expected behavior, source URLs and API support status. Do not assume every example exists today.
- [ ] For each row classify official API, supported web endpoint, unsupported, or unresolved. Verify authorization, response schema, and limits; redact credentials. Track native implementation, tests, and equivalence status separately.
- [ ] Spike Iced gallery and fonts in a clean Rust project; verify Windows native build, keyboard navigation, high-DPI rendering, resize, image decoding, GPU/software backend feasibility. If Iced fails critical constraints, evaluate Slint without introducing a browser.
- [ ] Set Rust stable toolchain and compatible pinned crate versions, CI for fmt/clippy/test on Windows, and a minimal shell with real navigation. Test application launch and clean exit.
- [ ] Gate: audit matrix exists, GUI compiles, CI passes. Commit `chore: establish native client foundation`.

### Task 2: Provider contract and error handling (v0.1)

Files: src/domain/{wallpaper.rs,search.rs,account.rs}, src/provider/{mod.rs,api.rs,web.rs,capabilities.rs}, tests/provider_contract.rs.

Interfaces: WallpaperId, Wallpaper, SearchQuery, SearchPage, Session, ProviderError, Capability. Provider methods: search(query: SearchQuery, session: &Session) -> Result<SearchPage, ProviderError>; wallpaper(id: WallpaperId, session: &Session) -> Result<Wallpaper, ProviderError>; capabilities() -> Vec<Capability>. Use async methods for network calls.

- [ ] Write mocked contract tests for metadata, pagination, missing optional fields, malformed data, 401/403/404/429 and timeouts.
- [ ] Run `cargo test --test provider_contract`; confirm tests fail before implementation.
- [ ] Implement /api/v1/search and /api/v1/w/{id} against verified response schemas, use TLS, request timeout, bounded concurrency, Retry-After when provided, limited retry/backoff, and redacted logging. Only add verified supported web adapters.
- [ ] Run contract tests; ensure query serialization and provider restrictions are preserved. Commit `feat: add provider adapter`.

### Task 3: Account authentication and preferences (v0.2)

Files: src/services/session.rs, src/ui/account.rs, tests/provider_contract.rs, docs/privacy.md.

Interface: SessionStore::load() -> Result<Session, SessionError>; save(session: &Session) -> Result<(), SessionError>; clear() -> Result<(), SessionError>.

- [ ] Test valid/invalid credentials, logout, account switch, expired credentials, and guest purity restrictions; ensure no extra access restriction is added to supported account controls.
- [ ] Implement API-key entry and validation using supported endpoints; distinguish API authentication from actual website login if upstream does not expose OAuth/login. Store secrets using Windows Credential Manager or equivalent OS-backed secret storage; never log them or put them in SQLite/plaintext.
- [ ] Implement preferences/profile read and write only where supported. Clear account-scoped view state and cache indexes on logout, preserve user-owned downloads, and label unsupported mutations.
- [ ] Run session/contract tests and manual guest/auth comparison. Commit `feat: authenticate Wallhaven accounts`.

### Task 4: Homepage, discovery and filters (v0.3)

Files: src/ui/{home.rs,search.rs}, src/app.rs, src/services/cache.rs, tests/parity.rs.

- [ ] Test each audited homepage section, sorting, search syntax, categories/purity, resolutions, aspect ratios, colors, AI filters if present, pagination and saved provider settings. Compare serialized requests against the audited behavior, not guessed parameters.
- [ ] Implement Homepage, Latest, Toplist, Hot and Random wherever audit confirms them; preserve default behavior, section order, filters, seeded random pagination where supported, and current navigation semantics.
- [ ] Implement virtualized gallery, asynchronous thumbnail loading, cancellation/request generation IDs so rapid filter changes cannot display old results, retry button, empty/loading state, and keyboard focus.
- [ ] Test stale-response cancellation, duplicate page requests and guest/account behavior. Run `cargo test --test parity`. Commit `feat: reproduce discovery and search`.

### Task 5: Wallpaper pages, downloads and cache (v0.4)

Files: src/ui/detail.rs, src/services/{cache.rs,download.rs}, tests/{cache.rs,downloads.rs,parity.rs}.

- [ ] Test metadata, tags, uploader, source, purity, dimensions, favorites, related search, original bytes and crop/scale parity where supported.
- [ ] Implement audited wallpaper detail actions, full original download and copied links. Use verified original image URL; do not silently recompress originals. Add minimal native save dialog and error reporting for website-equivalent downloads.
- [ ] Implement CacheStore::put(key: &str, bytes: &[u8]) -> Result<(), CacheError> and CacheStore::get(key: &str) -> Result<Option<Vec<u8>>, CacheError>. Use disk LRU, atomic writes, bounded decoded textures, cap default proposal 2 GB and hard max 10,000,000,000 bytes. Separate temporary previews, actual downloads and wallpaper storage.
- [ ] Test eviction arithmetic, corrupt entries, concurrent writes, zero disk space and exact original bytes. Run `cargo test --test cache --test downloads --test parity`. Commit `feat: reproduce wallpaper details and original saves`.

### Task 6: Collections, profile and remaining site functions (v0.5–v0.9)

Files: src/ui/{collections.rs,community.rs,account.rs}, src/provider/web.rs, docs/parity-matrix.md, tests/parity.rs.

- [ ] Recheck every audited collection operation, including reading, creating, renaming, deleting, adding/removing favorites and privacy, plus profile/preferences. Add native controls only with verified endpoint permissions.
- [ ] Implement each supported remaining website workflow in its own small test-first commit, including uploads, tags, comments and forums if present and supported. Test denied roles, validation errors, refresh-after-write and mutation failure.
- [ ] Mark unsupported operations in the matrix; do not use undocumented session scraping, private endpoints, or falsely synchronized local state. If necessary offer a clearly labeled browser handoff until supported routes exist.
- [ ] Gate: matrix has a disposition and test for every row. Run `cargo test --test parity`. Commit `docs: resolve website parity inventory`.

### Task 7: v1.0 parity release gate

Files: docs/parity-matrix.md, README.md, LICENSE, .github/workflows/ci.yml, installer configuration.

- [ ] Run fmt, clippy with warnings denied, full tests and clean Windows release build. Manually compare the website and application on guest and authorized test sessions, supported workflow by workflow.
- [ ] Validate fonts/assets redistribution licenses and attribution; use system fallback when not cleared. Package a signed installer if signing is available; disclose unsigned status otherwise.
- [ ] Verify high DPI, keyboard, account isolation, connectivity loss, rate limit recovery and Windows 10/11 smoke tests. Document compatible OS and known limitations.
- [ ] Only call v1.0 a complete port if every required parity row is implemented natively and passes. If upstream gaps remain, keep beta designation or obtain explicit scope revision; do not substitute desktop customization for parity.
- [ ] Commit release docs and tag only after review/authorization.

## Phase B — after v1.0: desktop-exclusive features

### Task 8: Download manager and local library (v1.1)

Files: src/services/{download.rs,library.rs}, src/storage/migrations.rs, src/ui/{downloads.rs,settings.rs}, tests/downloads.rs.

- [ ] Test original bytes, progress, cancellation, retry, duplicate filename, partial failure, reserved Windows names, path traversal and destination permission errors.
- [ ] Implement DownloadJob and DownloadManager::enqueue(id: WallpaperId, destination: PathBuf) -> Result<JobId, DownloadError>. Persist queue/history, sanitized filename templates (default wallhaven-{id}.{ext}), configurable default folder and atomic completion via partial files. Add pause/resume only when server supports ranges.
- [ ] Add custom collections/favorites locally only as clearly distinct from synchronized upstream collections; support local user imports and optional properly licensed stock images.
- [ ] Run download tests and manual recovery test; commit `feat: add desktop download manager and local library`.

### Task 9: Native wallpapers and history (v1.2)

Files: src/platform/windows.rs, src/ui/history.rs, src/services/library.rs, tests/windows.rs.

Interfaces: WallpaperPlatform::monitors() -> Result<Vec<Monitor>, PlatformError>; current(monitor: &MonitorId) -> Result<Option<PathBuf>, PlatformError>; apply(monitor: Option<&MonitorId>, image: &Path) -> Result<(), PlatformError>.

- [ ] Test fake platform adapter for every monitor, all monitors, disconnect, missing file, Windows failure, history persistence and undo. Record prior per-monitor state and do not commit history until Windows confirms success.
- [ ] Use windows-rs IDesktopWallpaper to enumerate monitors, query current paths, apply full local image paths and set supported positioning. Fetch remote original to app-managed persistent directory when user selects Set Background, avoiding manual save. Restrict deletion of files referenced by active wallpapers/history.
- [ ] Implement History tab with reapply, undo and timestamps; safely handle missing files and multi-monitor partial failures. Import any Windows-compatible local image and provide all-monitor/per-monitor selection.
- [ ] Run adapter tests and Windows integration smoke test on an actual multi-monitor machine. Commit `feat: integrate native Windows wallpaper controls`.

### Task 10: Adaptive appearance (v1.3)

Files: src/ui/{theme.rs,settings.rs}, tests/appearance.rs.

- [ ] Test default static colors, opt-in dynamic modes, selected wallpaper change, OS wallpaper change, no image, account logout, reduced motion and unavailable custom font.
- [ ] Implement AppearanceMode::{Static, Ambient, Adaptive}; default Static. Ambient uses downscaled wallpaper blur and translucent readable surfaces; Adaptive extracts palette with contrast guard. Both follow selected wallpaper while browsing and current applied wallpaper otherwise, with configurable source preference. Never write changes into the original image.
- [ ] Add blur intensity, opacity, font selection and reduced-motion toggle; GPU defaults on, software fallback only if the selected backend supports it. Cache palette/blur by image ID and use asynchronous computation, rejecting stale results.
- [ ] Run appearance tests, manually validate 100/150/200% DPI and smooth transitions. Commit `feat: add opt-in dynamic appearance`.

### Task 11: Rotation, tray and startup (v1.4)

Files: src/platform/windows.rs, src/ui/settings.rs, src/services/library.rs, tests/windows.rs.

- [ ] Test rotation interval, no eligible images, network failure, tray exit and disabled startup.
- [ ] Use native Windows slideshow APIs for local image sets where appropriate; for online home/gallery sources, fetch eligible images in a bounded queue and apply using IDesktopWallpaper. Use current homepage source and filters; keep smart context-dependent automation deferred.
- [ ] Implement opt-in startup with Windows registration, tray menu Open/Next/Exit, and minimize-to-tray setting. Make background behavior explicit and ensure Exit stops worker tasks.
- [ ] Run integration tests and background CPU/memory checks. Commit `feat: add native rotation and tray integration`.

## Cross-cutting release quality

- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, `cargo build --release` before each milestone.
- Use synthetic fixtures for CI, no real credentials or copyrighted image catalog in Git. Live smoke tests are opt-in and rate-limited.
- Measure rather than promise 120 Hz: target 8.33 ms frame budget on supported displays; test gallery scroll, selection, resizing, memory and cache. No constant repaint on idle; low background CPU.
- Document architecture, setup, configuration, screenshots, tradeoffs, benchmark methodology and known API gaps in README for portfolio value.

## Naming shortlist (unverified availability)

1. **Wall Ember** — warm palette, clear purpose, independent identity.
2. **Vesperwall** — atmospheric and distinctive.
3. **Wallery** — wallpaper + gallery.
4. **Wallora** — wallpaper + aura, aligns with adaptive themes.
5. **Haven Desktop** — clear association, but check branding permission carefully.
6. **Chromawall** — color-driven desktop customization.
7. **Wallveil** — subtle glass and ambient visual identity.
8. **Emberframe** — warm visual style and image framing.

Recommendation: **Wallora** or **Wallveil**; use `An unofficial native Wallhaven client for Windows` as subtitle. Check GitHub, package registries, trademark and domain availability before locking the final name. Avoid claiming official affiliation.

## References for discovery

- https://wallhaven.cc/help/api (verify against live official documentation; access may be blocked by site protection)
- https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-idesktopwallpaper
- https://iced.rs/
- https://slint.rs/
- https://github.com/ppolkadot/Salted-Flutters-Polaris (audit license before bundling)
