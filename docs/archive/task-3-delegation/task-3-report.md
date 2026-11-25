# Wallora Task 3 Review Report (Sessions and Preferences)

## Commit Summary

- **Branch:** `feat/foundation`
- **Base SHA:** `fd1c181b7787e15128eb5c997e2c3b137f95f330`
- **Task 3 Commit SHA:** `9864e8ac705bb0770dc4761fcd97e302c84a58b0` (`feat: account sessions`)
- **Working Tree:** Only the four pre-existing unstaged docs files remain modified and unstaged (`docs/2026-11-18-wallora-plan-v2.md`, `docs/api-capabilities.md`, `docs/decisions.md`, `docs/parity-matrix.md`). No remote pushes or PRs were created.

## Implementation Choices

### 1. Core Domain (`crates/wallora-core/src/domain/`)
- Added `AccountId::generate()` (UUIDv4), `validate_username`, `UsernameVerification` (`Verified`, `UnverifiablePrivate`, `UnverifiableEmpty`, `Inconclusive` — `Mismatch` is intentionally unrepresentable as persisted state), `IdentityCheckOutcome`, `AccountRecord`, `UserSettings`, `BrowseDefaults`, `ProxyConfig` (`System`, `Direct`, `Manual(String)` with URL validation rejecting embedded credentials, non-HTTP(S) schemes, queries, and fragments), `LocalPreferences`, and `WALLHAVEN_SETTINGS_URL` in `domain/account.rs`.
- Added `CollectionSummary` in `domain/collection.rs` (list metadata only; handles Wallhaven's `0`/`1` or boolean `public` field; leaves item fetching and Favorites mapping to Task 6a).
- Added `TopRange::from_api_str`, `Dimensions::parse_wxh`, and `deserialize_per_page` in `domain/search.rs` to map supported upstream `/settings` values into `BrowseDefaults` / `SearchQuery`.

### 2. Provider, Origin, Pacer & Proxy Seams (`crates/wallora-core/src/provider/`)
- Extended `Provider` with `user_settings`, `own_collections`, `public_collections` (always unkeyed), `user_settings_with_key`, and `own_collections_with_key`.
- Extracted `authorize_secret` in `provider/origin.rs`, reused by `authorize` so ephemeral sign-in probes enforce the exact same `OriginPolicy` and sensitive-header marking.
- Ephemeral sign-in requests use opaque monotonic probe IDs (`ephemeral:{probe_id}:{url}`) in the coalesce map so API keys never enter coalesce keys, logs, or error strings.
- `public_collections` always calls `get_json` with `SessionScope::guest(generation)` so `/collections/{username}` is strictly unkeyed during sign-in and re-verification.
- Wrapped `api_client`, `image_client`, and `proxy` in `RwLock<HttpTransport>` inside `WallhavenClientInner`. `WallhavenClient::set_proxy` hot-swaps the underlying `reqwest::Client` instances in place while keeping the single shared `ApiPacer` (and any active 429 cooldown), `image_sem`, `OriginPolicy`, `ImageOriginPolicy`, and `CredentialStore`.

### 3. SQLite Persistence (`crates/wallora-core/src/storage/`)
- Added versioned SQLite migrations (`PRAGMA user_version = 1`) in `storage/migrations.rs` and `SqliteStore` (`SessionStorage` trait) in `storage/db.rs` at `%LOCALAPPDATA%\Wallora\data\wallora.db`.
- Separated account state (`accounts`, `account_cache` with `ON DELETE CASCADE`) from local machine preferences (`local_preferences`, singleton `id = 1`). Zero secret columns exist in SQLite.
- Enabled WAL journal mode, `foreign_keys = ON`, and `busy_timeout = 5000`; multi-table mutations run inside SQLite transactions.

### 4. Session Lifecycle & Verification (`crates/wallora-core/src/services/session.rs`)
- Pure `evaluate_username_verification` covers all 5 outcomes (`Verified`, `Mismatch`, `UnverifiablePrivate`, `UnverifiableEmpty`, `Inconclusive`). In accordance with `docs/api-capabilities.md`, an ambiguous HTTP 404 on `/collections/{username}` (including with an all-private own list) classifies as `Inconclusive`, not `Mismatch`.
- Two-phase `probe_sign_in`: validates key against `/settings` first (401 → `InvalidApiKey` with zero identity requests; other errors → `TemporaryFailure`), then compares keyed `/collections` against unkeyed `/collections/{username}`. Performs no CredentialStore or SQLite writes.
- `SessionManager` checks `start_scope == self.scope` on async completions (`commit_sign_in`, `apply_refresh`, `handle_provider_result`), discarding stale completions and stale 401s.
- Partial-failure safety:
  - `commit_sign_in` saves the API key to `CredentialStore` first, then commits SQLite; on SQLite failure it performs compensating `credentials.delete(&account_id)` rollback and never exposes the credential.
  - `update_proxy` updates the single `WallhavenClient` transport and persists to SQLite; on SQLite failure it rolls back the client transport to the prior `ProxyConfig`.
  - `restore` verifies the active account's key exists in `CredentialStore`; on missing or unreadable credential it falls back to Guest, bumps generation, purges persisted `account_cache`, and notifies the user.
- Mid-session 401 on an active account revokes the session (generation bump, credential deletion, persisted account cache purge, in-memory reset, user notice). Guest 401 surfaces upstream's restriction without adding client-side filtering or triggering revocation.
- Logout and account switch bump the scope generation and purge in-memory/persisted account cache while preserving `local_preferences`.

### 5. Windows Platform & Provisional Iced App (`crates/wallora-windows/`, `crates/wallora-app/`)
- Implemented `WindowsCredentialStore` (`wallora-windows::credentials`) using `keyring` 3 (`windows-native` only, no mock backend) with sanitized error messages, idempotent `delete`, unit tests, and an `#[ignore]` smoke test using a unique UUID service/target with RAII cleanup.
- Implemented `validate_wallhaven_handoff_url` (strictly `https://wallhaven.cc/...`, no credentials) and `open_wallhaven_in_browser` via `ShellExecuteW` in `wallora-windows`.
- Wired `Route::Account`, masked API key input (`.secure(true)`), redacted `Debug` on `Message`, own collections list (names/counts/visibility only), saved accounts switcher, read-only upstream defaults with labelled `Open on wallhaven.cc` hand-off, separate local proxy preferences (`System` / `Direct` / `Manual`), and active `BrowseDefaults` display on `Search` using a single `Arc<WallhavenClient>`.

## Verification & Test Evidence

All local foundation and Task 3 gates ran against synthetic credentials and local mocks only (no `.env` read, no live Wallhaven calls):

| Gate | Command | Result |
| --- | --- | --- |
| Formatting | `cargo fmt --all -- --check` | PASS (0 diffs) |
| Clippy (default) | `cargo clippy --locked --workspace --all-targets -- -D warnings` | PASS (0 warnings) |
| Clippy (`test-util`) | `cargo clippy --locked --workspace --all-targets --features wallora-core/test-util -- -D warnings` | PASS (0 warnings) |
| Tests (default) | `cargo test --locked --workspace --all-targets` | PASS (`wallora-app` 3, `wallora-core` 31, `wallora-windows` 3 passed / 1 ignored) |
| Tests (`test-util`) | `cargo test --locked --workspace --all-targets --features wallora-core/test-util` | PASS (`wallora-app` 3, `wallora-core` 31 unit + 9 `key_isolation` + 13 `provider_contract` + 3 `search_serialization` + 14 `session`, `wallora-windows` 3 passed / 1 ignored) |
| Windows Credential Manager smoke | `cargo test --locked -p wallora-windows -- --ignored --nocapture` | PASS (`credentials::tests::windows_credential_manager_smoke_write_read_delete` ok in 0.02s against real Windows Credential Manager with unique synthetic UUID target and scoped RAII delete) |
| Dependency audit | `cargo deny --locked check` | PASS (`advisories ok, bans ok, licenses ok, sources ok`) |
| Release build | `cargo build --locked --release` | PASS |
| Local MSI build + verify | Pinned WiX v3.14.1 (`6AC824E1642D6F7277D0ED7EA09411A508F6116BA6FAE0AA5F2C7DAA2FF43D31`) + `cargo wix --package wallora-app --no-build --nocapture --bin-path ...` + `scripts/verify-msi.ps1` | PASS (`Verified wallora-app-0.1.0-x86_64.msi (8798208 bytes): exe0 present, no Environment table`) |

### Regression Coverage Added
- `crates/wallora-core/tests/session.rs` (14 integration tests): all 5 username verification outcomes (verified, mismatch, unverifiable-private, unverifiable-empty, inconclusive including ambiguous 404), invalid key refusing sign-in with zero identity calls, unkeyed `/collections/{username}` enforced on sign-in and re-verification, mid-session 401 revocation, stale async completions and stale 401 discard, logout and account switch cache purge with local preference preservation, Guest NSFW upstream refusal with no client-side filtering, manual proxy routing through a loopback HTTP proxy while preserving an active 429 cooldown on the single `ApiPacer`, SQLite/Credential Manager partial failures and rollbacks, and raw SQLite file + WAL + tracing log secret-leak checks.
- `crates/wallora-app/src/app.rs` (3 unit tests): `Message` `Debug` secret redaction, single `Arc<WallhavenClient>` pointer identity preserved across proxy mode changes, stale 401 discard vs active 401 revocation.
- `crates/wallora-windows/src/credentials.rs` & `lib.rs` (3 unit tests + 1 ignored live Credential Manager smoke test): error sanitization, Wallhaven hand-off URL allowlist, and real Windows Credential Manager write/read/delete round-trip.

## Remaining Concerns & Follow-Ups

1. **Open pre-Task-3 gates (authorized deferred):** Task 1 signed-in website audit and hosted CI remain open per owner authorization.
2. **Task 4 image-cache purge hookup:** `SessionManager` bumps scope generation and purges Task 3's persisted `account_cache` + in-memory state on logout/switch/401 revocation; Task 4 will plug its image-cache purge actor into this lifecycle.
3. **Task 6a collection items & Favorites:** Task 3 exposes own collection summaries (names/counts/visibility) only; item access and Favorites mapping remain gated on `verified` / Task 6a evidence.
4. **Manual proxy scope:** `ProxyConfig::Manual` accepts `http://` and `https://` proxy URLs without embedded credentials (`reqwest` `socks` feature is not enabled).
5. **Duplicate dependency warning:** `cargo deny --locked check` passes, with a non-fatal duplicate-version warning from `keyring 3.6.3` pulling `windows-sys 0.60.2` / `windows-targets 0.53.5` alongside existing `windows-targets 0.52.6`.
6. **Re-signing in with the same username:** Signing in again with an already-saved username creates a new opaque `AccountId` entry (leaving prior saved entries intact until signed out). If deduplication by username is desired later, it should be decided alongside Task 6a account management UX.
