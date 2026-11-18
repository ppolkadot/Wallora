# Wallora — Wallhaven Desktop Client Implementation Plan (v2, rev 5)

> Supersedes `docs/2026-11-17-wallhaven-desktop.md` as the execution plan. The v1 file remains the requirements record; where they disagree, v1's "Requirements baseline" wins unless the owner records a change in `docs/decisions.md`.
> Steps use checkbox (`- [ ]`) syntax. Each task ends with a gate; do not start the next task until the gate passes.

**Goal:** Deliver a faithful native Windows client for Wallhaven before adding desktop-only customization.

**Name:** Wallora (repo `ppolkadot/Wallora`, MIT). Subtitle: *An unofficial native Wallhaven client for Windows*. No official affiliation claimed anywhere (README, About dialog, installer, window title).

**Architecture:** Cargo workspace. A GUI-free, Windows-free core crate (domain, provider, services, storage) is consumed by a thin Iced app crate and a Windows platform crate. All API traffic goes through one provider client with a shared pacer. No hosted backend, no catalog mirror.

**Tech stack:** Rust stable (pinned in `rust-toolchain.toml`, `Cargo.lock` committed), Iced (exact version pinned after the spike; Slint is the fallback), Tokio, Reqwest (features set explicitly: `rustls-tls`, `json`, `gzip`, `stream`; no default features), Serde, rusqlite (`bundled`), `image` (with app-level decode budgets), `windows` crate, `keyring` 3 with the `windows-native` feature (never the mock backend in release), `directories`, `tracing` + `tracing-appender`, `wiremock`, `proptest`, `insta`. CI on GitHub Actions `windows-latest`. Installer: `cargo-wix` (MSI) with a pinned cargo-wix/WiX pair, built in CI from Task 1.

## Changes from v1

1. **Known API surface written down up front** ("Upstream facts") so the audit verifies hypotheses with recorded evidence.
2. **Parity scope checkpoint at the end of the audit (Task 1), before any provider code.** The public API exposes no write routes (favorites, collection edits, uploads, comments, tag edits), so v1's 1.0 gate is likely unreachable; the owner decides early instead of at release. Phase B stays blocked until Task 7 passes in full under the gate in force (original, or owner-revised via option A).
3. **Workspace split** (`wallora-core` / `wallora-windows` / `wallora-app`); core depends on neither Iced nor Windows APIs.
4. **Client-side pacing** below the published limit, retries counted against the same budget, shared cooldown on any 429.
5. **API key sent only as `X-API-Key`, only to the approved HTTPS API origin**; cross-origin redirects rejected; tests prove CDN and redirect targets never see it.
6. **Image and cache safeguards land before the first live gallery** (new Task 4), not after.
7. **No-clobber atomic file publication** (note: `std::fs::rename` replaces existing files on Windows — not used for publication).
8. **Undo correctness**: prior wallpaper, global position, and slideshow state are snapshotted before applying.
9. **Cross-cutting items added**: app manifest (PerMonitorV2, longPathAware), settings file, panic/log handling, User-Agent, proxy behavior, accessibility risk, offline rules, cargo-deny, early MSI.
10. **Measurable GUI spike** and **Task 6 split** into gated subtasks. Naming resolved.

## Requirements baseline (from v1 — binding)

- Windows 10/11; native Rust UI; no JS/TS, WebView, HTML/CSS, Electron.
- Phase A (v0.x → v1.0) = faithful port with comprehensive website coverage. Phase B = desktop-only features. Gates precede desktop-only additions.
- Official API first; only documented or explicitly authorized web endpoints for gaps. No private-session scraping, auth bypass, rate-limit evasion, or mirroring. Partnership is optional, not a launch dependency.
- Follow upstream account permissions and purity/category controls exactly; no app age checker or app-imposed filtering; never expose what the session cannot access; guest and account state fully separate.
- Original full-resolution downloads, byte-exact; optional automatic filenames and default destination arrive in Phase B.
- Upstream favorites/collections are upstream data; unsupported writes never pretend to sync. An open required gap blocks a "full parity" / "complete port" claim — such releases are labelled beta or partial client. When no supported route exists: document the gap and, where useful, an explicitly labelled external-browser hand-off (which never counts as native parity).
- Wallpaper control only via `IDesktopWallpaper`, incl. per-monitor. Applied wallpapers kept locally without manual download.
- Thumbnail cache hard cap 10,000,000,000 bytes; user may lower it; active wallpapers and permanent downloads are never thumbnail-cache entries.
- Phase B customization: static default theme; Ambient and Adaptive opt-in; color extraction/blur; theme follows selected/current wallpaper with a configurable source preference; custom fonts; reduced motion off by default; GPU on by default; startup/tray settings; local/stock images; history/undo tab; download manager; simple rotation.
- Palette: `#1E1C1A #34302D #5E5A57 #B0ACA9 #F5F4F2 #F37316 #F7B120 #E84D23`. Font: Salted Flutters Polaris (owner's repo — confirm a license file exists before bundling) or Google Sans (check current license); system font fallback.
- Deferred: AI recommendations, new similarity engine, smart rotation, desktop preview. Upstream `like:` similar search is parity, not AI.

## Upstream facts (hypotheses — Task 1 confirms each with evidence)

| Area | Expected (per public API docs) | Must verify / caveat |
|---|---|---|
| Wallpaper info | `GET /api/v1/w/{id}` | NSFW requires API key; record exact 401 shape |
| Search | `GET /api/v1/search`: `q, categories, purity, sorting (date_added, relevance, random, views, favorites, toplist), order, topRange, atleast, resolutions, ratios, colors, page, seed` | **`hot` is in the site UI but not the documented API sorting values** — verify before promising it. Verify whether an **AI-art filter** exists on site and in API. |
| Search `meta` | `current_page, last_page, per_page, total, query, seed` | Model `query` as `Option<QueryEcho>` where `QueryEcho = Text(String) | Tag { id, tag }`; model `seed` as `Option<String>` |
| Query syntax | `tag`, `-tag`, `+tag`, `@user`, `id:N`, `type:png/jpg`, `like:ID` | |
| Tag | `GET /api/v1/tag/{id}` | |
| User settings | `GET /api/v1/settings` (key required), read-only | Response has **no username** — usable for key validation only |
| Collections | `GET /api/v1/collections` (own, key), `/collections/{username}`, `/collections/{username}/{id}?purity=` | Read-only. Own collection *items* need the username → see Task 3. Verify that the default collection is what the site calls "Favorites". |
| Auth | `X-API-Key` header or `apikey` param | We use the header only. No OAuth; API key ≠ website login. |
| Rate limit | 45 requests/min, HTTP 429 on excess | We pace at ≤ 40/min |
| Images | `w.wallhaven.cc/full/..`, `th.wallhaven.cc/{small,lg,orig}/..` | Use URLs from API responses; never construct them; never send the key there |
| Crop/scale | site-side feature (if present) | Audit whether it is a client-side view or a server route |
| Writes | none in public API | Expected gaps: favorite add/remove, collection CRUD/privacy, upload, comments, tag edits, forums, settings edits |

## Repository structure

```text
Cargo.toml  Cargo.lock  rust-toolchain.toml  deny.toml
crates/wallora-core/src/
  domain/{mod,wallpaper,search,account,collection,tag}.rs
  provider/{mod,client,api,pacer,origin,capabilities,error}.rs
  services/{mod,session,credentials,image_guard,cache,download,library,history}.rs
  storage/{mod,db,migrations,paths}.rs
  util/sanitize.rs
crates/wallora-core/examples/probe.rs
crates/wallora-core/tests/{provider_contract,search_serialization,key_isolation,image_guard,cache,downloads,session}.rs
crates/wallora-core/tests/fixtures/*.json     # synthetic or scrubbed
crates/wallora-windows/src/{lib,credentials,wallpaper,startup,tray,single_instance}.rs
crates/wallora-app/src/{main,app,message,theme}.rs
crates/wallora-app/src/views/{home,search,detail,account,collections,settings,history,downloads}.rs
crates/wallora-app/src/widgets/{gallery_grid,thumbnail}.rs
crates/wallora-app/{wallora.manifest,build.rs,wix/main.wxs}
spikes/iced-gallery/                           # not shipped, excluded from release build
docs/{parity-matrix,api-capabilities,architecture,privacy,decisions}.md
.github/workflows/ci.yml
```

## Core contracts (introduced only when a task uses them)

```rust
// Task 2 ships only these two; later tasks add methods as they need them.
pub trait Provider: Send + Sync + 'static {
    fn search(&self, q: SearchQuery, scope: SessionScope)
        -> impl Future<Output = Result<SearchPage, ProviderError>> + Send + 'static;
    fn wallpaper(&self, id: WallpaperId, scope: SessionScope)
        -> impl Future<Output = Result<Wallpaper, ProviderError>> + Send + 'static;
}
// Added later: user_settings + own_collections (Task 3), collections/collection_items (Task 6a), tag (Task 6b).

pub trait CredentialStore: Send + Sync + 'static {   // implemented in wallora-windows; fake in tests
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError>;
    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError>;
    fn delete(&self, account: &AccountId) -> Result<(), CredentialError>;
}
```

- Inputs are owned so futures are `'static` and can be moved into `iced::Task::perform`. Provider is held in an `Arc` and cloned into tasks. Static dispatch; `FakeProvider` for tests.
- `SessionScope { kind: Guest | Account(AccountId), generation: u64 }`. `AccountId` is an opaque random UUID created locally on sign-in; the username is stored as a separate non-secret field. Generation increments on sign-in, sign-out, account switch, and key revocation. Every response, cache read, and cache write carries the scope; a mismatch with the current scope is discarded.
- `ProviderError`: `Unauthorized, Forbidden, NotFound, RateLimited { retry_after: Option<Duration> }, Busy, Timeout, Network, Decode { context }, Upstream { status }, OriginRejected`. Each maps to one user-facing message and one action (retry / sign in / wait N s).

### Network policy (Task 2)

- One `reqwest::Client` for the API origin (`https://wallhaven.cc`, path prefix `/api/v1/`) with `redirect::Policy::none()`; a 3xx is surfaced as `Upstream` — never followed with the key. A separate client without any auth for image hosts, redirect policy = same-origin-only.
- `X-API-Key` attached per request by `origin::authorize(req, scope)` only when scheme is `https`, host is exactly `wallhaven.cc`, **effective port is 443**, and path starts with `/api/v1/`. Never stored as a default header.
- **Pacer (API)**: every attempt start (first try or retry) must satisfy both (i) ≥ 1.5 s since the previous attempt start and (ii) ≤ 40 starts in any rolling 60 s window (sliding log of start times). No burst allowance. In-flight cap 2. On any 429: shared cooldown for `Retry-After` or 60 s if absent; queued requests wait, prefetch is dropped. At most 2 retries, idempotent GETs only, on 429/5xx/transport errors; never on 401/403/404.
- **Queues**: `Interactive` 16, `Visible` 64, `Prefetch` 64. Overflow: `Prefetch` drops oldest; `Visible` drops oldest (it belongs to content already scrolled away; the grid re-requests if it comes back); `Interactive` rejects the new request immediately with `ProviderError::Busy` → UI shows "Too many requests queued, try again" (no silent drop). Identical in-queue requests are coalesced. Image-host traffic has its own in-flight cap (6) and no API-token cost.
- Timeouts: connect 10 s, request 30 s (API), 120 s idle-read for originals.
- `User-Agent: Wallora/<version> (+https://github.com/ppolkadot/Wallora)`. Proxy: Reqwest's environment/system proxy detection is verified on Windows in Task 1; if it does not read the Windows proxy, document it as a known limitation and add a manual proxy setting in Task 3.

## Phase A — faithful port

### Task 1: Foundation, audit, spike, scope decision (v0.1)

Order matters: 1a → 1b → 1c → 1d → 1e.

**1a. Workspace bootstrap**
- [ ] Workspace `Cargo.toml` (`[workspace.lints]`: `unsafe_code = "deny"` everywhere except `wallora-windows`; clippy `all` warn), `rust-toolchain.toml` (current stable), committed `Cargo.lock`, `deny.toml`, `.gitignore` += `/target`.
- [ ] Provisional Iced pin for the shell: latest stable Iced release at bootstrap time, pinned with `=x.y.z`; the spike (1d) confirms or replaces it.
- [ ] `wallora-app` shell: Iced window with real navigation between empty Home/Search/Settings views; manifest (PerMonitorV2, longPathAware, Windows 10/11 `supportedOS`) + icon embedded via `build.rs`; `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`.
- [ ] Infrastructure: data dirs via `directories` (`%APPDATA%\Wallora` config, `%LOCALAPPDATA%\Wallora` cache/data/logs); `tracing` to rolling file; panic hook logging + native message box.
- [ ] CI (`windows-latest`): `cargo fmt --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace --all-targets`, `cargo deny check`, `cargo build --locked --release`, tool installs via `cargo install --locked`, rust-cache, and a shell MSI build with pinned cargo-wix + WiX.
- [ ] Commit `chore: bootstrap workspace and CI`.

**1b. API probe** (`cargo run -p wallora-core --example probe`; opt-in; key read from `WALLHAVEN_API_KEY` in env/`.env`; never printed, logged, or written)
- [ ] Hard budget: ≤ 25 requests per run, ≥ 2 s spacing, abort on first 429. Rate limiting itself is tested with mocks, never live.
- [ ] Probe matrix (input → expected): `/w/{known SFW id}` guest → 200; `/w/{known NSFW id}` guest → 401; same with key → 200 if account allows NSFW, else record actual; `/search` with each documented `sorting`, `purity=111` guest → record; `/search?q=like:{id}`; `/search?q=id:{tagid}` → `meta.query` object shape; `sorting=random` → `meta.seed`; `/tag/{id}` → 200; `/settings` guest → 401, key → 200; `/collections` key → 200; `/collections/{username}` and `/collections/{username}/{id}` → 200.
- [ ] **`hot` check**: request page 1 with `sorting=hot`, with no `sorting` (default), and with `sorting=not_a_sort`. `hot` is confirmed only if (i) its id order differs from both the default and the invalid-value responses, and (ii) it matches the website's Hot page order captured at the same time (manual). A 200 alone is not confirmation (could be a silent fallback). Otherwise `hot` is `unsupported` in the API and handled per the matrix.
- [ ] **AI-art filter check**: if the site shows such a filter, find the API parameter in upstream documentation or an explicit written permission; confirm it changes results (ids differ between on/off for the same query) and that the change matches the site. No documented or explicitly permitted parameter → `unsupported`; never reverse-engineer one.
- [ ] **Favorites mapping check** (account only): identify which `/collections` entry the site labels Favorites; record its id **for that account only** (mappings are stored per `AccountId` and never assumed for another account), confirm the id is stable across two probe runs and after adding/removing a favorite on the website (incl. when Favorites is empty — it must still be listed). Without account evidence the mapping stays `unresolved`, and Task 6a must not label any collection "Favorites".
- [ ] Save response *shapes* (values scrubbed/synthesized; no usernames, no keys) to `tests/fixtures/`. Each upstream-facts row becomes **confirmed / differs / missing / unresolved** in `docs/api-capabilities.md` with the date, probe command, status codes and scrubbed evidence. If no key or test account is available, account rows are **unresolved**, not assumed.

**1c. Website inventory** (manual, guest + test account, respecting site ToS; no automated crawling)
- [ ] Cover explicitly: homepage sections and order, Latest/Toplist/Hot/Random, search syntax, every filter and sort (incl. any AI-art filter), wallpaper detail (tags, uploader, source, purity, dims, related/similar, crop/scale), downloads, collections/favorites, profile, settings, upload, tag/moderation actions, comments, forums, anything else found.
- [ ] `docs/parity-matrix.md`, one row per user-visible action: *action · roles · expected behavior · source URL · route class (official API / supported web endpoint / unsupported / unresolved) · authorization evidence · behavior evidence · required-for-parity (y/n) · native status · test · equivalence verified*.
- [ ] **Authorization evidence** is only upstream documentation (link + date) or explicit written permission from Wallhaven (stored reference). A probe result never authorizes a route; it goes in **behavior evidence**. A web endpoint without authorization evidence is `unsupported`, however well it works.

**1d. GUI spike** (`spikes/iced-gallery`)
- [ ] Reference machine recorded in `docs/decisions.md` (CPU, GPU, RAM, display Hz, Windows build). Viewport 1600×1000 at 150% DPI, plus 100% and 200%.
- [ ] Pass criteria: (a) 2,000-tile virtualized grid, scripted scroll, p95 frame time ≤ 16.7 ms (record vs 8.33 ms target); (b) idle CPU ≈ 0%, no continuous redraw; (c) thumbnails decoded off the UI thread via a Tokio-backed `Task`; (d) live window resize without stalls; (e) full keyboard navigation of the grid; (f) custom font loads; (g) RSS after loading 2,000 thumbnails ≤ 500 MB; (h) wgpu starts, and tiny-skia software fallback starts.
- [ ] Fallback triggers: fail (a), (c), (d) or (e) → repeat with Slint. Fail (h)-software only → document "GPU required" as a limitation, not a fallback. Accessibility: record Narrator output; incomplete screen-reader support is a documented limitation, not a fallback trigger.
- [ ] Record decision + numbers in `docs/decisions.md`.

**1e. Parity scope checkpoint (blocking, owner decision)**
- [ ] Summarize matrix gaps (required rows that are unsupported/unresolved). Owner chooses and records in `docs/decisions.md`:
  - **(A) Revise the parity gate** — 1.0 means "complete for supported routes", with listed gaps and browser hand-offs. Requires explicit owner sign-off as a scope revision.
  - **(B) Keep the gate** — releases stay beta/partial until gaps close; **Phase B remains blocked** until Task 7 passes in full under the original gate, or under an owner-revised gate if the owner later chooses (A).
  - **(C) Pursue authorization/partnership** for specific gaps, in parallel with (B). Optional, never a dependency.
- [ ] Gate for Task 1: CI green incl. MSI artifact; fixtures + api-capabilities + matrix committed; spike decision recorded; checkpoint decision recorded. Commit `docs: record audit, spike and parity scope`.

### Task 2: Provider client — search and wallpaper (v0.1)

- [ ] Tests first (wiremock): decode fixtures incl. missing optional fields and unknown extra fields; `meta.query` as string, tag object, and null; `seed` present/null; malformed JSON; 401/403/404/5xx/timeouts; 429 with and without `Retry-After` → shared cooldown, retries consume tokens, prefetch dropped; `SearchQuery` → query-string snapshots (insta) for every documented filter.
- [ ] Pacer tests (paused Tokio clock): never two starts < 1.5 s apart; never > 40 starts in any 60 s window under a 200-request burst incl. retries; overflow behavior per queue; `Busy` surfaced for full Interactive queue.
- [ ] `key_isolation` tests: key header present on `/api/v1/*`; absent on image-host requests; absent for `https://wallhaven.cc:8443/api/v1/*` and `http://wallhaven.cc/api/v1/*`; API 3xx to another origin is not followed and the target receives nothing; image-host redirect to another origin is rejected; key never appears in any URL, log line, or error `Display`.
- [ ] Implement `WallhavenClient` per "Network policy"; `SecretString` (zeroize on drop, `Debug`/`Display` = `***`).
- [ ] Gate: `cargo test -p wallora-core --test provider_contract --test search_serialization --test key_isolation` green. Commit `feat(core): provider client for search and wallpaper`.

### Task 3: Sessions and preferences (v0.2)

- [ ] Add `Provider::user_settings` and `Provider::own_collections` (list only; items come in 6a).
- [ ] Sign-in form takes **API key + username**. Two independent results:
  - **Key validity**: `/settings` with the key → 200 = valid; 401 = invalid (sign-in refused); other = retry later.
  - **Username verification** (only for a valid key): compare `/collections` (keyed, own) with `/collections/{username}` (unkeyed, public view):
    - public ids ⊆ own ids and at least one id in common → **verified**;
    - any public id not in own list, or `/collections/{username}` → 404 → **mismatch** (sign-in refused with "username does not match this API key");
    - own list contains only private collections (public view empty) → **unverifiable-private**;
    - own list empty → **unverifiable-empty**;
    - public-list request fails with 429/5xx/timeout/transport error → **inconclusive** (retry later; treated as unverified until resolved).
  - Unverifiable and inconclusive states are allowed and shown in Account. The authenticated own-collection **list** (names, counts) stays available; own collection **items** and the Favorites label require `verified` (or an authorized account-bound route recorded in the matrix).
- [ ] Tests: each identity outcome (verified, mismatch, unverifiable-private, unverifiable-empty, inconclusive) tested separately, each with a valid key; key-invalid tested separately with no identity check attempted.
- [ ] On success create `AccountId` (UUID), save key via `CredentialStore`, store username + verification state + AccountId in SQLite.
- [ ] `wallora-windows::credentials` implements `CredentialStore` with `keyring` (`windows-native`); manual smoke test against real Credential Manager (write, read, delete).
- [ ] Tests: valid/invalid key; key revoked mid-session (401 → scope generation bump, account caches purged, user told); logout; account switch; guest requesting NSFW (upstream refuses → show upstream's restriction, add none of our own).
- [ ] Apply upstream defaults (purity/categories, page size, whatever else the API returns). Settings edits are a gap unless the audit found a route → disabled control + labelled hand-off.
- [ ] Logout / switch: bump generation, delete account-scoped cache files and index rows, clear in-memory views; keep user downloads and applied wallpapers.
- [ ] Gate: session tests + Credential Manager smoke test. Commit `feat: account sessions`.

### Task 4: Image pipeline and caches (v0.3) — before any live gallery

- [ ] `ImageGuard` (core): per-class budgets — thumbnail: encoded ≤ 2 MB, ≤ 1024 px per side; preview: encoded ≤ 64 MB, ≤ 16,384 px per side, ≤ 150 MP; originals are streamed to disk and only decoded for preview under the preview budget. Header-only dimension check before full decode; `image::Limits` as a second layer only. Global decode semaphore (`max(1, cores/2)`) and decoded-bytes budget for textures in flight.
- [ ] `CacheStore` (thumbnails/previews only) as a single actor task. **One serialization boundary**: reservation, writer completion (`Finished`), eviction, and scope transitions (sign-in/out, switch, revocation) are all messages to this actor and are processed one at a time.
  - Reserve: writer asks for a reservation of the class maximum (thumbnail 2 MB, preview 64 MB) or `Content-Length` if smaller; actor evicts LRU as needed **before** granting. Staging files count against the cap from the moment of reservation.
  - Write: the actor assigns each reservation a unique staging path and id. The writer opens it with `create_new`, streams into it, and stops the moment bytes written would exceed its reservation — unknown-length bodies can never outgrow the reservation. The writer checks a cancellation token before every chunk.
  - **Single writer lifecycle — one terminal message.** There is no separate "commit" message. When a writer stops for any reason it first drops (closes) its file handle, then sends exactly one `Finished { reservation_id, outcome }` where `outcome = Complete { bytes } | Oversized | Failed | Cancelled`. Cancellation is cooperative (token), never a bare task abort. Because `Finished` is only sent after the handle is closed, the actor never renames or deletes a file that is still open.
  - On `Finished`, the actor decides in one step:
    - `Complete` **and** reservation not cancelled **and** its `SessionScope.generation` equals the current one → rename staging → content file inside the cache dir (unique internal names, so replace semantics are safe), shrink the charge to actual size, write the index row, retire the id. This is the only path that publishes.
    - anything else (stale generation, cancelled, oversized, failed) → retire the id, delete the staging file, and release its byte charge **only after the deletion succeeds**; a failed deletion keeps the charge, records an orphan, and is retried on the next scope transition and at startup.
    - a `Finished` for an already-retired or unknown id → ignored, nothing published.
  - **The actor never waits.** Scope transition: bump generation, mark all old-scope reservations cancelled and trip their tokens, delete the old account's *published* content files and index rows (charges released per successful deletion), then return to processing messages. Old-scope staging files are cleaned when their `Finished` arrives (and are guaranteed not to publish, since their generation is stale). For each cancelled reservation the actor schedules a 10 s timer message to itself; if `Finished` has not arrived by then, the reservation is marked orphaned (charge kept, path recorded for startup reconciliation) and a later `Finished` for it only triggers cleanup.
  - SQLite index (key, scope account id, size, last_access, state). Startup reconciliation deletes orphans and stale staging files before accepting requests.
- [ ] Cap: default 2 GB (owner may change), user-lowerable, hard max 10,000,000,000 bytes validated at input and enforced in the actor.
- [ ] Scope rules: reads and writes require the current `SessionScope`; guest and each account have separate namespaces; account entries deleted on logout. **Offline**: cached account-scoped content is shown only for the same signed-in account, labelled stale; any 401/403 on reconnect purges that account's cache. Guest offline content may be shown, labelled stale.
- [ ] Tests (incl. proptest): decompression bomb and oversized dimensions rejected before allocation; concurrent puts never exceed cap including staging; unknown-length body larger than its reservation aborted; eviction arithmetic; corrupt entry → delete + refetch; scope mismatch → miss; logout purge; **logout while a write is staging** (staging deleted, nothing published, charge released); **logout while the writer still holds the file open** (no delete or release until its `Finished`; actor keeps serving other messages meanwhile); writer that never sends `Finished` → orphan after 10 s, bytes stay charged; **`Finished{Complete}` arriving after logout** → not published, cleaned up; **failed cleanup** (simulated delete error) keeps bytes charged and is retried; **`Finished` for a retired id after a failed delete** → ignored, nothing published; normal `Complete` in the current scope publishes exactly once.
- [ ] Gate: `cargo test -p wallora-core --test image_guard --test cache` green. Commit `feat(core): guarded image pipeline and scoped cache`.

### Task 5: Discovery and search (v0.4)

- [ ] Tests: request generation ids drop superseded responses; duplicate page fetches de-duplicated; infinite scroll stops at `last_page`; random keeps `seed` across pages; typed query builder round-trips syntax tokens and passes raw text unchanged.
- [ ] Views: homepage sections, Latest, Toplist (+range), Random, Search — matching audited defaults and order. Filter bar: categories, purity (selectable levels follow upstream rules for the session), resolutions/atleast, ratios, colors, sorting/order.
- [ ] **Hot** and the **AI-art filter** ship only when their matrix rows have **both** behavior evidence (1b checks pass) **and** authorization evidence (upstream documentation or explicit permission) covering the route *and the specific parameter value* (`sorting=hot`, the AI filter parameter). Otherwise they remain gaps with a labelled browser hand-off.
- [ ] Gallery: virtualized grid from spike; thumbnails through `ImageGuard` + `CacheStore`; cancel on scroll-away; loading/empty/error states with retry; keyboard focus.
- [ ] Gate: tests green; manual side-by-side with the website per view. Commit `feat: discovery and search`.

### Task 6: Detail page and original downloads (v0.5)

- [ ] Tests: detail metadata (tags, uploader, source, purity, dimensions, file size/type, counts); `like:` similar search; tag → search; crop/scale per audit disposition; original download byte-exact (length = API `file_size`, decodes under preview budget, dimensions match); reserved names (`CON`, `NUL`, `COM1`, trailing dots/spaces), path separators and `..` in any server-derived name; zero disk space; interruption leaves only a partial; **collision tests**: target exists before start, target created during download, two downloads of the same id concurrently.
- [ ] Save: partial at `<dir>\.<name>.<uuid>.part` (same directory, unique); publish with `MoveFileExW` **without** `MOVEFILE_REPLACE_EXISTING` (fails if target exists) → retry with ` (n)` suffix; never overwrite. Default name `wallhaven-{id}.{ext}` from the API path's extension. Native save dialog via `rfd`.
- [ ] Copy-link actions (page URL, original URL, short URL if provided).
- [ ] Gate: `cargo test -p wallora-core --test downloads` + proptests green. Commit `feat: wallpaper details and original saves`.

### Task 6a: Collections — read (v0.6)

- [ ] Add `Provider::collections` / `collection_items`. Own collection items only when the account's username is `verified` (or an authorized account-bound route exists per the matrix); for accepted-but-unverified sessions (`unverifiable-private`, `unverifiable-empty`, `inconclusive`) show the authenticated list with an explanation that items need a verified username. (`mismatch` never reaches this view — it refuses sign-in in Task 3.) Other users' public collections; pagination with purity param. Tests: private collection as guest (upstream denies), empty collection, each non-verified identity state blocks items but not the list.
- [ ] Favorites: label a collection "Favorites" only if Task 1b confirmed the mapping for **this** `AccountId` (stable id, present when empty) and the username is verified. If the mapping is `unresolved`, show collections by their upstream names only and keep the matrix row unresolved.
- [ ] Writes (favorite add/remove, collection CRUD, privacy) shown as disabled with labelled "Open on wallhaven.cc" hand-off unless the matrix lists a supported route. No local shadow state that looks synced.
- [ ] Commit `feat: read upstream collections`.

### Task 6b: Profiles and tags (v0.7)

- [ ] Add `Provider::tag`. Uploader view via `@user` search; tag pages; tag navigation. Commit `feat: profiles and tags`.

### Task 6c: Remaining matrix rows (v0.8–v0.9)

- [ ] One test-first commit per row with a supported route (tests: denied roles, validation errors, refresh-after-write, mutation failure). Rows without a route get disposition `gap` + hand-off where useful.
- [ ] Gate: every row has a disposition and, where implemented, a test. Commit `docs: resolve parity inventory`.

### Task 7: v1.0 release gate

- [ ] Full CI green; MSI install, upgrade-over-previous, and uninstall tested on Windows 10 and 11.
- [ ] Manual parity walk-through, guest and account, row by row; DPI 100/150/200%; keyboard-only run; connectivity loss and rate-limit recovery.
- [ ] `THIRD_PARTY.md` (crate licenses via `cargo about` or `cargo deny list`; font licenses). Sign if a certificate exists; else README states "unsigned" and SmartScreen behavior.
- [ ] Label: "1.0 complete port" only if every required row is native and passing, or under option (A) exactly as the owner worded it. Otherwise beta/partial. Tag only after owner authorization.

## Phase B — desktop-only features

**Entry condition:** Task 7 has passed in full (all release checks) and produced a 1.0 label under the gate in force — either the original gate or an owner-revised gate (option A) recorded in `docs/decisions.md`. Owner approval of option A alone does not unblock Phase B; Task 7 must still pass under it. A beta/partial release does **not** unblock Phase B.

### Task 8: Download manager and local library (v1.1)

- [ ] Persistent queue (SQLite): progress, cancel, retry, concurrency (default 2, shares the image-host in-flight cap); Range resume only with `Accept-Ranges: bytes` and matching ETag/Last-Modified.
- [ ] Filename templates (`{id} {ext} {resolution} {purity} {category}`; default `wallhaven-{id}.{ext}`) through the Task 6 sanitizer and no-clobber publisher; configurable default folder; "downloaded" badge.
- [ ] Local library: user imports and optional licensed stock set; local favorites visually distinct from upstream collections.
- [ ] Commit `feat: download manager and local library`.

### Task 9: Native wallpaper control and history (v1.2)

Interface (`wallora-windows`): `monitors()`, `current(&MonitorId)`, `position()`, `slideshow_state()`, `apply(Target, &Path, Option<Position>)`, `Target = All | Monitor(MonitorId)`.

- [ ] Dedicated STA thread owns `IDesktopWallpaper`: `CoInitializeEx(COINIT_APARTMENTTHREADED)`, runs a message loop, receives commands over a channel (all mutations serialized), `CoUninitialize` on shutdown after releasing interfaces.
- [ ] Before apply: snapshot each affected monitor's current image into `%LOCALAPPDATA%\Wallora\history\`, plus global position (`GetPosition`) and slideshow state (`GetSlideshow` source items, `GetSlideshowOptions` shuffle flag + tick interval, `GetStatus`). If a snapshot fails, warn the user and record history as "undo unavailable" for that monitor rather than claiming undo.
- [ ] Undo restores, in order: slideshow (source via `SetSlideshow`, options via `SetSlideshowOptions`) if one was active, otherwise per-monitor images; then global position. Each restored item is re-read and the result shown per monitor.
- [ ] After apply: re-read actual per-monitor state; history records only what Windows reports as applied (partial failures visible).
- [ ] Remote "Set as wallpaper" downloads the original into `applied\` (outside cache eviction). Files referenced by active wallpaper or history are not deletable in-app; history retention (default 50) prunes unreferenced copies.
- [ ] **History tab** (`views/history.rs`): list of entries with thumbnail, target monitor(s), timestamp, per-monitor result; actions **Reapply** and **Undo** with independent availability: Reapply needs the applied image file to exist; Undo needs a successful snapshot whose files exist. A failed undo snapshot disables only Undo; a missing applied image disables only Reapply. Each disabled action shows why.
- [ ] Tests with `FakePlatform`: per-monitor/all, disconnect between enumerate and apply, missing file, partial failure, undo with deleted original, failed snapshot, undo restoring slideshow (source, shuffle, interval) and position, reapply. Manual smoke test on a real multi-monitor machine.
- [ ] Commit `feat: native wallpaper control and history`.

### Task 10: Adaptive appearance (v1.3)

- [ ] `AppearanceMode::{Static (default), Ambient, Adaptive}`. Ambient: blurred downscale (≤ 256 px) + translucent surfaces. Adaptive: palette extraction on a downscale with WCAG AA guard (≥ 4.5:1 text), falling back to static tokens.
- [ ] **Configurable source preference**: `FollowSelected | FollowApplied | SelectedThenApplied (default)`; react to OS wallpaper change (`WM_SETTINGCHANGE` with `SPI_SETDESKWALLPAPER`).
- [ ] Settings: blur intensity, surface opacity, font, reduced motion (default off), GPU default on with software fallback where the backend supports it.
- [ ] Palette/blur cached per image id, computed off-thread, stale results dropped. Tests: static default, opt-in modes, selection change, OS wallpaper change, no image, logout, reduced motion, missing font.
- [ ] Commit `feat: opt-in dynamic appearance`.

### Task 11: Rotation, tray, startup (v1.4)

- [ ] Rotation: local sets via `IDesktopWallpaper::SetSlideshow` where suitable; online sources (current home view + filters) via bounded prefetch (≤ 3 ahead) through the shared pacer, applied with `SetWallpaper`.
- [ ] Tray: Open / Next / Pause rotation / Exit; minimize-to-tray opt-in; Exit cancels workers and waits for in-flight file publication. Single-instance lock (named mutex) with focus-existing-window added here.
- [ ] Startup: opt-in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`; removed when disabled and on uninstall.
- [ ] Tests: interval, no eligible images, network failure, tray exit, startup disabled. Measure idle CPU < 0.5% and steady RSS.
- [ ] Commit `feat: rotation, tray and startup`.

## Cross-cutting

- **Before each milestone:** fmt, clippy `-D warnings` (workspace, all targets), tests (workspace, all targets), `cargo deny check`, release build, MSI build.
- **Secrets:** key only in Credential Manager (app) or `.env` (developer probe, git-ignored). Never in SQLite, logs, URLs, crash reports, fixtures, or commit messages.
- **Fixtures:** synthetic or scrubbed; generated PNG/JPEG for byte-exact tests; no catalog images in Git. Live tests behind `--features live-tests` + env var; never in CI.
- **Performance:** measured against the spike's reference machine and method; no redraw while idle.
- **Accessibility:** keyboard-complete UI is required for 1.0; screen-reader coverage documented per toolkit capability.
- **Privacy:** no telemetry; `docs/privacy.md` lists every host contacted and every file written.
- **Updates:** none automatic in v1.x. **i18n:** English only; user-facing strings centralized.

## Review focus

- Session change mid-request: scope + generation on every response and cache op (Tasks 3–5).
- 429 handling: pacer, shared cooldown, retries in budget (Task 2).
- Key isolation: origin check, no redirects with key (Task 2).
- Image safety and cache cap incl. staging (Task 4).
- No-clobber publication and collisions (Tasks 6, 8).
- Wallpaper undo and partial multi-monitor failure (Task 9).

## References

- https://wallhaven.cc/help/api · https://wallhaven.cc/terms
- https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-idesktopwallpaper
- https://docs.rs/iced/latest/iced/struct.Task.html · https://docs.rs/image/latest/image/struct.Limits.html
- https://docs.rs/keyring/ (feature `windows-native`) · https://github.com/volks73/cargo-wix
- https://github.com/ppolkadot/Salted-Flutters-Polaris (confirm license file before bundling)
