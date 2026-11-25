# Wallora architecture

Updated 2026-11-25, reflecting `main` through Task 6c. The plan lives in [2026-11-18-wallora-plan-v2.md](2026-11-18-wallora-plan-v2.md). This file describes what is built; planned modules are marked.

## Workspace

```text
crates/wallora-core      GUI-free, Windows-free: domain, provider, services, storage
crates/wallora-windows   Windows-only: Credential Manager, browser hand-off, error dialog, process stats
crates/wallora-app       Iced 0.14 application, views, theme, manifest, MSI definition
spikes/iced-gallery      Frame-time/memory spike, not shipped
```

Dependency direction: `wallora-app` → `wallora-core` and `wallora-windows`; `wallora-windows` → `wallora-core` (for the `CredentialStore` trait). `wallora-core` depends on neither Iced nor Windows APIs, and that boundary is deliberate. `unsafe_code` is denied workspace-wide; the one crate that needs Windows FFI is `wallora-windows`. The toolkit is Iced, pinned at `=0.14.0` (decision §4).

## wallora-core

| Module | Responsibility |
| --- | --- |
| `domain` | Plain types: `SearchQuery`/`SearchTerms` and `SearchPage`, `Wallpaper`, `CollectionSummary`, `Tag`, and account types (`AccountId`, `SessionScope`, `SecretString`, `UsernameVerification`, `LocalPreferences`, `ProxyConfig`, cache-cap validation). `domain::query_builder` parses and renders the documented search syntax (`tag`, `-tag`, `+tag`, `@user`, `id:N`, `type:`, `like:ID`) and passes raw text through unchanged. The `CredentialStore` trait is defined here. |
| `provider` | The `Provider` trait and its `WallhavenClient` implementation: `search`, `wallpaper`, `user_settings`/`user_settings_with_key`, `own_collections`/`own_collections_with_key`, `public_collections`, `collections`, `collection_items`, `tag`, `fetch_image_bytes`, and `open_original_stream` for uncapped originals. `origin` decides where the API key may be sent. `pacer` schedules every API attempt. `error` holds `ProviderError`. `api` parses `Retry-After` and similar details. |
| `services::session` | `SessionManager`: sign-in, identity verification, refresh, logout, account switching, 401 revocation, proxy and cache-cap preferences. The only place that mutates session state. `restore_with_cache(storage, credentials, cache_sink)` takes a `SessionCacheSink` that is notified of every scope transition (restore, sign-in, sign-out, account switch, key revocation) and of account purges (including a 403 during account refresh), so the image cache can hide and delete the outgoing account's entries. The app calls `restore_with_cache` at boot. `apply_cache_cap`/`commit_cache_cap` validate, live-apply and persist the cap. |
| `services::image_guard` | `ImageGuard`: per-class budgets. Thumbnail: encoded ≤ 2 MB, ≤ 1024 px per side. Preview: encoded ≤ 64 MB, ≤ 16,384 px per side, ≤ 150 MP. A header-only dimension check runs before decoding, with `image::Limits` as a second layer. Decode concurrency is `max(1, cores/2)`, and in-flight decoded bytes are budgeted (default 640 MiB). |
| `services::image_pipeline` | `ImagePipeline`: cache-first thumbnail and preview loads through the fetch seam and `ImageGuard`, returning scope-checked decoded images or cache misses. |
| `services::cache` | `CacheStore`: one actor that serializes reads, writes, eviction and scope transitions. Cap defaults to 2,000,000,000 bytes with a hard max of 10,000,000,000; LRU eviction; supervised writers; a 10 s watchdog that only marks cancelled writers overdue. Files live under `%LOCALAPPDATA%\Wallora\cache\`: `cache.lock`, `cache_index.db` (a SQLite index separate from `wallora.db`), `objects/` and `staging/`. An empty `cache.active` marker exists while the cache is open, and one found at startup means the last exit was not orderly, so all account-scoped entries are discarded (guest entries are kept). The app opens it at boot via `CacheHost` and awaits `CacheStore::shutdown()` on window close. |
| `services::discovery` | `DiscoveryController` for Latest, Toplist (+ range), Random and Search: generation ids drop superseded responses, page de-duplication, `last_page` stop, stable random seed, retry, scroll-away cancellation hooks, and validated browser hand-offs (`SiteGapAction`, `UnsupportedFeature`, `DetailUnsupportedAction`). |
| `services::download` | `DownloadService`: Windows-safe filename sanitizer, `.<name>.<uuid>.part` staging in the destination folder, no-clobber publication with ` (n)` retry, byte/size/dimension verification, cancellation, and disk-full cleanup. |
| `services::collections` | `CollectionsController`: own collection list, `verified`-only own-item gate, unresolved per-`AccountId` Favorites mapping, other users' public collections, and paginated items with purity. Write actions stay disabled with validated hand-offs. |
| `services::profiles_tags` | `TagController` and `UploaderController`: tag metadata with `id:N` listing, `@user` uploader listing, filtering/pagination, and profile/tag-directory/moderation hand-offs. |
| `storage` | `AppPaths`, SQLite store and migrations (schema version 3; `local_preferences.cache_cap_bytes`). |
| `util::sanitize` | Server-derived filename sanitization: reserved device names, trailing dots and spaces, path separators, `..`, and control characters. |

*Planned:* `library`, `history` services (Tasks 8, 9).

### Request path

```text
UI message → Task::perform(provider.search(query, scope))
           → WallhavenClient.execute_paced_get
           → ApiPacer: queue by priority, coalesce duplicates, rate budget, cooldown
           → origin::authorize: attach X-API-Key only for https://wallhaven.cc:443/api/v1/*
           → response tagged with SessionScope → SessionManager.handle_provider_result
```

Key properties, each covered by tests:

- **Pacing:** at least 1.5 s between attempt starts, at most 40 starts per rolling 60 s, at most 2 in flight, shared cooldown on any 429 (`Retry-After` seconds or HTTP-date, clamped to one hour), retries at 2 s and 4 s, at most three tries. Queues: `Interactive` 16, `Visible` 64, `Prefetch` 64. Identical queued requests are coalesced and each caller gets an independent subscription.
- **Key isolation:** the key is attached per request, never stored as a default header, never in a URL; cross-origin redirects are rejected and the image client never carries credentials.
- **Scope:** every response carries the `SessionScope` (guest or account, plus a generation counter). Responses whose scope no longer matches the current one are discarded, which is how sign-out, switching and revocation avoid showing stale private data.
- **Sessions:** sign-in validates the key via `/settings`, then compares the keyed own-collection list with the public username lookup to grade the username (`verified`, `unverifiable-private`, `unverifiable-empty`, `inconclusive`, or `mismatch`, which is refused). Account collection *items* require `verified`; the "Favorites" label additionally needs a confirmed per-`AccountId` mapping, which is still unresolved, so upstream collection names are shown unchanged.

### Features

`test-util` (non-default) exposes explicit test constructors and hooks. The integration tests `provider_contract`, `key_isolation`, `search_serialization`, `session`, `cache`, `downloads` and `image_pipeline_loopback` require it (`required-features = ["test-util"]` in `crates/wallora-core/Cargo.toml`); `cache_cap`, `collections`, `discovery`, `image_guard`, `image_pipeline` and `profiles_tags` run in the default lane. CI runs a second lane with `--features wallora-core/test-util`. It never enters the release dependency graph.

## wallora-windows

`WindowsCredentialStore` implements `CredentialStore` over `keyring` with the `windows-native` backend (service name `Wallora`). `open_wallhaven_in_browser` validates that a URL is a Wallhaven HTTPS URL before calling `ShellExecuteW`. `file_publish::WindowsFilePublisher` publishes a staged file with `MoveFileExW` and no `MOVEFILE_REPLACE_EXISTING`, so an existing target is never overwritten. The crate also provides a native error dialog for the panic hook and process RSS/CPU readings. Planned: `IDesktopWallpaper` control, tray, startup (Tasks 9, 11).

## wallora-app

Iced application state in `app.rs`, messages and routes in `message.rs`, views in `views/{home,search,detail,collections,tag,uploader,account,settings}.rs`, and the virtualized gallery in `widgets/{gallery_grid,thumbnail}.rs`; palette in `theme.rs`. `cache_host.rs` owns the cache runtime. Startup order in `main.rs`: resolve and create data directories, start the rolling file log, install the panic hook (log plus native dialog), start the cache runtime and open the cache with the persisted cap, restore sessions through `restore_with_cache`, run the Iced app, and await cache shutdown on window close. `rfd` provides the native Windows Save dialog for original downloads. `build.rs` embeds the manifest (PerMonitorV2, `longPathAware`, Windows 10/11) and the icon. `wix/main.wxs` defines the per-machine MSI.

Home, Search and Detail are live, with collections, tag and uploader routes, keyboard grid navigation, and labelled browser hand-offs for gaps. The manual website equivalence check is part of Task 7.

## Build and verification

CI (`.github/workflows/ci.yml`, `windows-latest`): `fmt`, Clippy and tests in both feature lanes, `cargo deny`, release build, MSI build (pinned `cargo-wix` 0.3.9 and WiX 3.14.1 with a checksum), a read-only MSI check, and artifact upload. Local checks are listed in the README.
