# Wallora v1.0 Release Readiness Implementation Plan

**Goal.** Close the gaps in the supported Windows routes and collect the evidence required for a v1.0 release.

**Approach.** Keep the existing Rust workspace and Iced UI. Fix release behavior at the app, provider, or installer boundary that owns it, then verify the resulting candidate with fixtures, guest live requests, hosted CI, and the reference-machine walkthrough.

**Requirements.** [Task 7 and the Phase A baseline](../../2026-11-18-wallora-plan-v2.md#task-7-v10-release-gate) · [Owner decisions](../../decisions.md)

- **Date:** 2026-11-25
- **Baseline.** Local `main` is `a6a3301`, eight commits ahead of `origin/main` at `6462fb1`. The pending changes are six release documents and this plan. The working tree has no code changes.
- **Scope in force:** owner-approved option A ([decisions §3](../../decisions.md#3-parity-scope-checkpoint--task-1e)): 1.0 = *complete for supported routes*, every gap documented with an applicable `Open on wallhaven.cc` hand-off, Task 7 checks still required. Hardware checks only on the reference machine ([decisions §5](../../decisions.md#5-hardware-verification-scope)).
- **Method.** Claude Opus 5.5 High inspected the release requirements, current docs, app and core routes, CI, and installer source. Codex independently checked GitHub and ran the build, test, and live guest checks below.
- **Scope of the proposal.** The owner authorized publication of the current beta to `main` and a GitHub pre-release. The fixes and v1.0 checks below are follow-up work. The validation-branch option below records the original review recommendation.

## 1. Verdicts

| Question | Verdict | Conditions |
| --- | --- | --- |
| Commit the six documentation files | **Yes, after small corrections.** The edits are documentation only and correctly label the build **beta / partial**. | Apply D1–D4 below before committing. |
| Push `a6a3301` (plus the docs commit) for hosted CI validation | **Defensible as a validation push to a `feat/*` branch, with owner authorization.** CI runs on `push` to `main` and `feat/*` (`.github/workflows/ci.yml`). A scan of `origin/main..a6a3301` found no secret-like strings. | Do **not** push `main` to `origin/main` just to validate: that publishes 8 feature commits on the default branch. Push to `feat/v1-gate-validation` instead (P1). It is still an outward-facing action, so it needs explicit owner approval. |
| Release as `1.0 — complete for supported routes` | **Beta / partial.** Phase B stays blocked. | Fix the offline state transition and verify keyboard coverage, Similarity, release packaging, and the remaining Task 1c and Task 7 gates. Treat the Start Menu shortcut as a packaging improvement. |

### Fresh verification at `a6a3301`

| Check | Result |
| --- | --- |
| Local build and tests | Formatting, both Clippy lanes, both test lanes (171 and 281 passed, one Credential Manager test ignored in each), and the release build passed. The prior evidence ledger records cargo-deny and the unsigned MSI static checks at the same code SHA. |
| Remote | GitHub main is at 6462fb1. The push dry run reported a fast-forward to a6a3301. GitHub returned push access, no branch rules for main, and zero Actions runs for a6a3301. The dry run excludes the pending documentation. |
| Live SFW guest features | A scratch harness using the production Rust client returned 24 results each for Latest, Toplist, Random, and nature search. It decoded one wallpaper thumbnail and preview, read detail and tag metadata, and returned tag results, uploader results, public collections and public collection items. |
| Live original download | The Windows publisher published 1,176,033 bytes for 3l5xev. The SHA-256 matched an independent CDN download (dd658a724b5899219ddbf7f8e400d9aeb3a0e0e8383f19e1f967c59f57b950ec). The scratch harness bypassed the GUI Save dialog and the installed user profile. |
| Live Similarity search | The production client received HTTP 403 on both SFW queries for 3l5xev, including the app's default sort and category parameters. Similarity behavior remains open. The cause of the 403 is undetermined. |

## 2. Evidence classes used below

- **CONFIRMED:** reproducible from repository content alone (file text, WiX tables, recorded MSI query), with a reproduction step given.
- **INSPECTION:** found by reading code. High confidence, but not executed in this review. A proposed test reproduces it.
- **LIVE.** Observed through the production client against Wallhaven. The response shows current behavior in this environment. Its cause can still be open.
- **UNVERIFIED:** needs a manual run, owner session, reference hardware or owner decision. Not evidence either way.

## 3. Findings, ranked by what they block

`Docs` = blocks the documentation commit · `Push` = blocks the CI validation push · `1.0` = blocks the 1.0 label.

| # | Finding | Class | Blocks | Where |
| --- | --- | --- | --- | --- |
| R1 | **Keyboard-complete UI is required for 1.0** (plan *Cross-cutting → Accessibility*), but keyboard handling covers only the gallery grid (arrows, PgUp/PgDn, Home/End, Enter) and Escape on Detail. No Tab or Shift+Tab focus traversal, no route shortcuts, no keyboard path to buttons: route navigation, filter toggles, Detail actions (Download, Copy URL, More like this, tags), every `Open on wallhaven.cc` hand-off, Sign out, Settings apply buttons. Text inputs submit with Enter, but nothing moves focus into them by keyboard. | INSPECTION (confirm in a keyboard-only run) | 1.0 | `crates/wallora-app/src/app.rs:719` `gallery_keyboard_event`, `:879` `detail_keyboard_event`, `:1152` `WalloraApp::subscription`; `widgets/gallery_grid.rs` `GridKey::from_iced_key` |
| R2 | **MSI ships no third-party notices.** The installer contains Wallora's MIT text and executable. Audit redistribution and attribution for the dependencies linked into that executable, then package the notices they require. | CONFIRMED for MSI contents | 1.0 packaging | `crates/wallora-app/wix/main.wxs`, `THIRD_PARTY.md` |
| R3 | **No Start Menu shortcut.** The MSI has no `Shortcut` table. A shortcut would give users a normal launch entry after installation. | CONFIRMED | Packaging improvement | `crates/wallora-app/wix/main.wxs` |
| R4 | **Version metadata disagrees with the README.** The README says v0.9. Cargo, the MSI and User-Agent use 0.1.0, and the manifest hard-codes 0.1.0.0. Task 7 needs two MSI versions with the same upgrade code. | CONFIRMED for metadata. Upgrade behavior still needs an install run. | 1.0 | `Cargo.toml`, `crates/wallora-app/wallora.manifest`, `crates/wallora-app/wix/main.wxs`, `README.md` |
| R5 | **Two required (`y`) parity rows can't pass as recorded.** (a) *Identify the owner's Favorites collection* is `unresolved`. (b) *View own collection items* has never been exercised live: the supplied username 404s, so items stay gated. Plan Task 7 says "Resolve outstanding evidence into explicit dispositions". Either an owner session settles them, or the owner reclassifies them in `decisions.md`. *Wallpaper comments* (`gap / unresolved`, not required) also needs a final disposition. | UNVERIFIED / owner decision | 1.0 | `docs/parity-matrix.md` rows *Identify the owner's Favorites collection*, *View own collection items*, *Wallpaper comments* |
| R6 | **More like this received HTTP 403 from Wallhaven.** Earlier evidence reports a 403 for another sample. The production client received it twice for 3l5xev on 2026-11-25. The app uses the generic access-denied message. Verify the normal guest route and provide a contextual hand-off for the denial. | LIVE + INSPECTION | 1.0 parity | `crates/wallora-app/src/app.rs` DetailMoreLikeThis, `crates/wallora-core/src/provider/error.rs` |
| R7 | **The AI-art hand-off probably points at the wrong page.** `UnsupportedFeature::AiArtFilter` opens `https://wallhaven.cc/search` with the text "Set it on wallhaven.cc". The matrix records that the guest search toolbar shows **no** AI-art toggle, and the only known AI setting is the keyed `ai_art_filter` in account settings. | INSPECTION + UNVERIFIED (needs the signed-in check) | 1.0 | `crates/wallora-core/src/services/discovery.rs:123` |
| R8 | **Task 7 gate coverage is incomplete in the ledger.** It doesn't track: (a) Narrator or screen-reader documentation (decisions §2 item 4 says it "must be recorded for v1.0"; plan *Accessibility*); (b) any performance or idle-redraw measurement of the **app** (only the spike was measured; plan *Cross-cutting → Performance*); (c) a re-run of the Credential Manager smoke test, which is ignored in CI and last passed at `5590726`; (d) upgrade with an existing data directory (SQLite `local_preferences` schema v3, cache index); (e) uninstall leaving `%APPDATA%`/`%LOCALAPPDATA%\Wallora` and Credential Manager entries behind, which `docs/privacy.md` doesn't document. | INSPECTION of the docs | 1.0 | `docs/release-gate-evidence.md`, `docs/privacy.md` |
| R9 | **Windows 10 is a binding requirement but can't be verified.** The requirements baseline says "Windows 10/11" and the README lists Windows 10 as supported. Decision §5 documents Windows 10 as unverified, but nobody has decided whether 1.0 may **ship** with it unverified. The ledger lists this as owner action 5. | Owner decision | 1.0 | `docs/2026-11-17-wallhaven-desktop.md:15`, `README.md:25`, decisions §5 |
| R10 | **Hosted CI has never run the candidate.** The last green hosted run is `6462fb1`, so local runs are the only evidence for 8 commits. | CONFIRMED (`git status -sb`: ahead 8) | 1.0 (fixed by P1) | `.github/workflows/ci.yml` |
| R11 | **Overstatement in the uncommitted plan annotation.** Task 7 item 3 says the `THIRD_PARTY.md` item "needs only the final owner confirmation that the release ships unsigned". R2 shows the notices aren't shipped, so the item isn't one confirmation away. | CONFIRMED | Docs | `docs/2026-11-18-wallora-plan-v2.md:241` |
| R12 | **README states a test claim that isn't true for the app.** "Only Windows 11 on one reference machine has been tested so far." The app has never run on the reference machine; only the spike did. The 2026-11-25 smoke ran on a different machine. The ledger says this correctly; the README doesn't. | CONFIRMED (README vs ledger *Hardware scope*) | Docs (cheap fix; pre-existing text) | `README.md:25` |
| R13 | **Release wording needs to match the evidence.** Local checks excluded hosted artifact upload, and the v0.9 heading conflicts with the installer version. The Task 1c note should say the recent guest re-check covered selected routes. | CONFIRMED | Docs | `docs/2026-11-18-wallora-plan-v2.md`, `README.md` |
| R14 | **Gate runs on a development PC write into the real user profile.** `AppPaths::resolve` uses Known Folders via `directories::BaseDirs`, and there is no override. The ledger records that an `APPDATA`/`LOCALAPPDATA` redirect failed and the run created, then deleted, both `Wallora` directories. Every future smoke or recovery run risks the same. | CONFIRMED (ledger) + INSPECTION (`crates/wallora-core/src/storage/paths.rs:35`) | none (test-safety hardening, recommended before Phase 4) | `storage/paths.rs`, `wallora-app/src/main.rs` |
| R15 | **Hand-off validator hardening.** `validate_wallhaven_handoff_url` checks the scheme, userinfo and host but not the port: `https://wallhaven.cc:8443/x` passes. Three near-duplicate wallpaper-id URL builders differ: `DetailUnsupportedAction::wallpaper_url` doesn't reject control characters, while `CollectionUnsupportedAction::favorite_url` and `ProfileTagUnsupportedAction::tag_moderation_url` do. Every current caller passes constants or API ids, so the practical risk is low. | INSPECTION | none (defence in depth, before 1.0) | `crates/wallora-windows/src/lib.rs:23`; `services/discovery.rs:87`, `services/collections.rs:125`, `services/profiles_tags.rs:96` |
| R16 | **CI provenance could improve.** The workflow uses moving action references and default token permissions. It builds on main and feature branches. Decide whether release artifacts will use the existing exact-SHA CI build or a dedicated packaging workflow. | INSPECTION | Optional hardening | `.github/workflows/ci.yml` |
| R17 | **The app never sets the cache offline.** The cache actor starts online and uses its connectivity state to label hits stale. Wallora's sole call to `set_connectivity` is in a test, so production transport failures never activate the offline badge. | CONFIRMED by call-site inspection | 1.0 runtime recovery | `crates/wallora-app/src/app.rs`, `crates/wallora-core/src/services/cache/actor.rs` |

Correct the release wording before committing the docs. The code is eligible for hosted CI validation after authorization.

## 4. What the build actually supports (for release notes)

| Class | Features | Evidence |
| --- | --- | --- |
| **Native, official API** | Latest, Toplist and range, Random with seed, search and filters, wallpaper details and original downloads, public collections, tag detail and browsing, and uploader search. Account settings and own collection reads have automated coverage. The own-items route requires a verified username. Similarity is implemented and returned 403 during live testing. | The fresh verification table records the live guest scope. The GUI Save dialog, other account flows and manual side-by-side parity remain open. |
| **Browser hand-off (`Open on wallhaven.cc`)** | Homepage sections; Hot; AI-art filter (target suspect, R7); image-based "Search for similar"; Crop & Scale; favorite/unfavorite; collection CRUD and privacy; user profile and profile comments; account settings update; upload; tag directory; tagging and moderation; forums read and post; Few tags / Untagged | Automated URL tests in `tests/discovery.rs`, `tests/profiles_tags.rs`, `tests/collections.rs` and `wallora-app::app::tests`. Real browser launch has not been checked manually. |
| **Unresolved** | Favorites label (no collection is labelled "Favorites"); wallpaper comments | R5 |
| **Phase B, not in 1.0** | Wallpaper control (`IDesktopWallpaper`), history and undo, download manager and library, adaptive appearance, rotation, tray, startup, single-instance, bundled font | Plan Tasks 8–11; README "Planned, after 1.0" |

## 5. Plan

Start with the documentation corrections and validation push. Run independent app fixes, packaging, and permitted owner-session evidence in parallel when practical. Signed-in evidence must precede changes to hand-off targets that depend on it. Run the reference-machine gate on the resulting candidate.

### Phase 0 — documentation commit (label stays beta / partial)

| Step | Change | Acceptance |
| --- | --- | --- |
| D1 (R11) | Update the Task 7 licence and signing annotation in `docs/2026-11-18-wallora-plan-v2.md`. Record that the inventory was reviewed and MSI notice packaging remains open. Retain the unsigned and SmartScreen status. | The annotation distinguishes licence inventory, shipped notices, and signing. |
| D2 (R13a) | In `:239`, change "every local CI step passes" to "every CI step except artifact upload passes locally". | Wording matches the ledger's table. |
| D3 (R12, R13) | Update the README heading to pre-release or beta until version metadata agrees. Name Windows 10 and 11 as intended platforms and describe the app's observed Windows 11 smoke separately from pending reference-PC and Windows 10 verification. | README claims match the evidence ledger. |
| D4 (R13) | Narrow the Task 1c annotation to the selected guest routes rechecked on 2026-11-25. Add the fresh guest smoke, 403, and offline wiring finding to `docs/release-gate-evidence.md`, `docs/implementation-status.md`, and the affected matrix rows as appropriate. | Scope, exact SHA and test boundaries are clear. Manual parity checkboxes stay open. |
| D5 | Optional: in `release-gate-evidence.md` *Owner actions needed*, add one line pointing to this plan for R1–R4 and R7–R8, which manual runs alone can't close. | Ledger and plan cross-reference each other. |

Commands (local only; no push):

```powershell
git status --short         # expect the 6 release docs and this plan file
git add README.md THIRD_PARTY.md docs/2026-11-18-wallora-plan-v2.md docs/implementation-status.md docs/parity-matrix.md docs/release-gate-evidence.md docs/superpowers/plans/2026-11-25-wallora-v1-release-readiness.md
git commit -m "docs: record release gate evidence and v1 readiness plan"
git show --stat HEAD       # confirm no files under crates/ or .github/
```

The code tree of the docs commit is identical to `a6a3301`, so CI on it validates the same code. Record its SHA in the ledger after P1.

### Phase 1 — hosted CI validation push (**owner approval required**; label stays beta / partial)

| Step | Action | Acceptance |
| --- | --- | --- |
| **P1** | Push the docs commit to a non-default branch. CI triggers on `feat/*`: `git push origin HEAD:refs/heads/feat/v1-gate-validation`. Don't move `origin/main`. | One `CI` run on that SHA, all steps green, including `Verify MSI artifact` and `Upload MSI artifact`. |
| P2 | Compare the hosted results with the local ledger. Both test lanes should report 171 and 281 passed with 1 ignored. Note the `wallora-msi` artifact size and SHA-256. The MSI won't be byte-identical to the local one (Product and Package ids are `*`), so compare the payload exe after an administrative extract if needed. | Counts match, or differences are explained in the ledger. |
| P3 | Update `docs/release-gate-evidence.md` (*Hosted CI, exact commit*) and `docs/implementation-status.md` (*Verification*) with the run URL and SHA, in a docs-only commit on the same branch. | R10 closed. |
| **P4** | Merging to `main` and pushing `origin/main` is a separate owner decision, not part of validation. | — |

### Phase 2 — code and configuration fixes before any 1.0 attempt (no hardware; each is its own reviewed commit)

Run the standard gate after each step: `cargo fmt --all -- --check`; both Clippy lanes; both test lanes (`cargo test --locked --workspace --all-targets [--features wallora-core/test-util]`); `cargo deny --locked check`; `cargo build --locked --release`; local `cargo wix` with the CI static checks. Push only to `feat/*`, with approval.

| Step | Change (files / symbols) | Tests and acceptance | Depends on |
| --- | --- | --- | --- |
| C1 (R1) | Add keyboard traversal to `WalloraApp::subscription` and the route views. Reuse Iced focus operations for native focusable widgets. Give actionable controls a focus target and a visible focus indicator. Cover navigation, filters, Detail actions, hand-offs, sign-in and settings. Enter and Space should use the same messages as pointer actions. Preserve text editing and gallery keys. | App tests cover traversal order, activation and text input behavior. A reference-PC keyboard-only run verifies every supported route. | — |
| C2 (R2) | Inventory the dependency versions linked into the executable and read their packaged licence and notice files. Preserve the required texts and attributions in `THIRD-PARTY-NOTICES.txt`. Install it with `crates/wallora-app/wix/main.wxs` and extend the CI MSI file-table check. | The expected notice file is present in the extracted MSI. Review the actual dependency notices against the shipped file. | — |
| C3 (R3) | If a Start Menu entry is in release scope, add a per-machine `ProgramMenuFolder` shortcut in `crates/wallora-app/wix/main.wxs` and make uninstall remove it. | Verify the MSI Shortcut table and launch and uninstall behavior. | Package UX choice |
| C4 (R4) | Build a lower-version baseline and a higher-version candidate with the same stable upgrade code. Keep Cargo, the manifest, MSI, README, and User-Agent aligned to their respective build versions. In `crates/wallora-app/build.rs`, check or derive the hard-coded manifest version from `CARGO_PKG_VERSION`. Retain the current downgrade guard. | Cargo manifest, MSI metadata, and User-Agent match for each build. Phase 4 proves the upgrade over existing data. | Candidate version choice |
| C5 (R6) | Handle `ProviderError::Forbidden` for `DetailMoreLikeThis` in `wallora-app/src/app.rs`. Keep the original wallpaper context when launching this search and show its existing validated wallpaper-page hand-off on a current, matching denial. Update the parity matrix after recording the live 403 evidence. | Fixture tests prove the hand-off appears for this action only, unrelated 403s retain their existing error, and stale responses carry no hand-off into the next search. Live parity remains open until the official path works or the owner records a disposition. | — |
| C6 (R7) | Verify where the AI-art control appears in Phase 3. Change `UnsupportedFeature::AiArtFilter` in `services/discovery.rs` only if the signed-in evidence identifies a different applicable URL. | Existing app hand-off test asserts the verified URL and accurate description. Record any upstream gap in the matrix. | O4 |
| C7 (R15) | Hardening: reject non-443 ports in `validate_wallhaven_handoff_url`, and consolidate the three wallpaper-id URL builders into one `wallpaper_page_url(&WallpaperId)` in core that rejects control characters, `%`, `/`, `\`, `?`, `#` and `@`. | Unit tests: `https://wallhaven.cc:8443/x` → `Err`; an id containing `\u{0007}` → `Err` from every caller. | — |
| C8 (R14) | Test isolation: a developer-only `WALLORA_DATA_ROOT` environment variable (or a `--data-root <dir>` flag), read in `main.rs` and passed to `AppPaths::from_roots`. Document it in `docs/privacy.md` *Developer-only items*. Credential Manager stays the real store, so smoke runs must not sign in. | App test: with the override set, every directory resolves under the root. A manual smoke leaves `%APPDATA%`/`%LOCALAPPDATA%` untouched. | — |
| C9 (R16) | CI: add `permissions: contents: read`; pin actions to commit SHAs; pin the toolchain action. Optionally add a `workflow_dispatch` or tag-triggered release job that builds the MSI from the tagged commit and records its SHA-256. | CI green on `feat/*`; the release-job decision is recorded in `decisions.md`. | owner decision on release provenance |
| C10 (R8e) | After testing uninstall, document its impact on application files, profile data and the local credential store in `docs/privacy.md`. | Documentation agrees with observations from the Windows installer run. | H10 |
| C11 (R17) | Wire `CacheStore::set_connectivity` into `WalloraApp` and its cache host. Accept current-scope transport failures as offline signals and current-scope successful network responses as recovery signals. Keep cache hits, cancellations, old generations, authorization failures, and 429 responses from toggling connectivity. Update visible cached thumbnails and previews when connectivity changes. Preserve session scope checks. | Add deterministic tests for a warm gallery going offline, stale badges on loaded and cached images, recovery after a successful retry, scope switches, delayed responses, and 429 separation. Verify the manual run in H5. | — |

### Phase 3 — owner-session evidence (Task 1c and the account half of Task 7) — **owner-driven**

Boundary: the owner signs in on their own browser and does every account mutation. The agent may watch and record only non-identifying facts. No credential is entered, read or stored by an agent. Only SFW content.

| Step | Record | Closes |
| --- | --- | --- |
| **O1** | The real site username, kept out of docs, and whether the API `Default` collection is the site's Favorites (label and empty state). | R5a input |
| **O2** | Owner-performed favorite add, then removal, then restoration. Afterwards, a keyed `GET /collections` id comparison (≤ 3 requests, ≥ 2.5 s apart). | Favorites row → `confirmed` per `AccountId` or a final `gap` |
| **O3** | With the real username: whether the own-items path verifies. If the account stays all-private, decide whether *View own collection items* is "implemented, verifiable only for accounts with a public collection". | R5b |
| **O4** | AI-art control location; Hot visibility; settings, upload, tagging, comment and forum controls; whether wallpaper comments exist when signed in. | R7, *Wallpaper comments* row |
| O5 | Update `docs/parity-matrix.md` and `docs/api-capabilities.md`. | — |

If the owner declines a session: record in `decisions.md` that Favorites stays an `unresolved` → `gap` disposition for 1.0 (no "Favorites" label, upstream name shown), and that own items are untested live. That is acceptable under option A only with that written decision.

### Phase 4 — reference-machine Task 7 runs — **reference PC, elevation and owner authorization required**

Use a lower-version MSI and a higher-version candidate built by hosted CI. Record the candidate SHA, each MSI SHA-256, and the hardware scope in `docs/release-gate-evidence.md`.

| Step | Check | Acceptance |
| --- | --- | --- |
| **H1** | Clean install of the lower-version MSI (per-machine, elevated). | App launches, metadata matches the lower version, and the notices are installed. If a Start Menu entry ships, it launches the app. |
| **H2** | Guest row-by-row parity walkthrough against wallhaven.cc, SFW only, at 100%, 150% and 200% DPI. The display-scaling change is a system setting and needs explicit approval. | Every `y` row: same items and order as the site for the same query at the same moment, or the difference explained. Every gap row: its hand-off opens the right page in the default browser. Set the matrix `equivalence verified` column per row. |
| **H3** | Account walkthrough (owner signs in inside Wallora using their key; the agent never handles it). | Account rows as H2. Sign-out purges account views and cache (privacy table). |
| **H4** | Keyboard-only run of every route (after C1). | Every action in section 4 reachable and operable without a mouse; focus is always visible. |
| **H5** | Start with a warm SFW gallery. Test network loss and recovery in the app. Exercise the 429 flow with a local scripted provider using the normal app response handling, so the manual case uses a predictable `Retry-After` and leaves Wallhaven untouched. | Cached images show the stale badge during the outage. A successful retry clears it. The app shows a distinct rate-limit state and allows recovery after cooldown. Scope boundaries and existing downloads remain intact. |
| **H6** | Original download through the GUI Save dialog: save twice to the same name, and once over an existing file. | Byte-identical to the API `file_size` and an independent hash; the second save gets a ` (1)` suffix; the existing file is untouched; the UI shows the actual saved path. |
| **H7** | Credential Manager smoke with a **synthetic** key: `cargo test -p wallora-windows -- --ignored`. It writes and then deletes a test credential, so it needs approval. | Pass. R8c closed. |
| **H8** | Performance and idle: app idle CPU and redraws for 60 s on Home with loaded thumbnails; scroll frame times recorded with the §2 method. Owner decides acceptance on the GT 340 software path. | Numbers recorded with a "valid for reference machine only" note. R8b closed. |
| **H9** | Narrator pass over Home, Detail and Settings. | Output recorded as a documented limitation. R8a closed. |
| **H10** | Upgrade over the lower-version installation with existing preferences, an SFW cache and an owner-driven test session. Test downgrade and uninstall afterward. | Upgrade leaves one Add/Remove Programs entry and preserves the expected user data. Downgrade is rejected. Uninstall removes app binaries and any installed shortcut. Record what happens to app profile data and credentials. |
| H11 | Windows 10: run H1, H2 (guest subset) and H10 if a machine exists; otherwise apply decision O-W10. | R9 closed either way. |

### Phase 5 — label and tag

| Step | Action | Acceptance |
| --- | --- | --- |
| L1 | Fill Task 7 checkboxes in `docs/2026-11-18-wallora-plan-v2.md`; close or carry over the unchecked Task 1 gate, the 1b `hot` and Favorites checks, and the Task 5 gate, each with a written disposition. | No unchecked box without a disposition. |
| L2 | Label `1.0 — complete for supported routes` after the mandatory code and packaging checks pass, every required parity row has evidence or an owner-approved written disposition, applicable hand-offs work, and hosted CI is green for the exact release SHA. Keep `beta / partial` while any gate is open. | Record the result in the ledger and README. |
| **L3** | Owner authorizes the tag (`v1.0.0`) and the release upload of the CI-built MSI, unsigned unless a certificate exists (decision O-SIGN). | Tag points at the CI-green SHA; the MSI SHA-256 is recorded. Phase B unblocks only now. |

## 6. Owner decisions needed

| Id | Decision | Needed before |
| --- | --- | --- |
| O-PUSH | Authorize the push to `feat/v1-gate-validation` (P1). Merging to `origin/main` is separate (P4). | Phase 1 |
| O-VER | Choose the lower and higher MSI version numbers and the release label. Keep the upgrade code stable across both installers. | C4 and Phase 4 |
| O-SESSION | Provide the signed-in session and do the favorite mutation yourself (Phase 3), or accept the documented fallback dispositions. | Phase 3 |
| O-HW | Reference-PC session with elevation, DPI changes and a Credential Manager write (Phase 4). | Phase 4 |
| O-W10 | Ship 1.0 with Windows 10 documented as unverified (README requirement wording changes accordingly), or wait for a machine. | L2 |
| O-PERF | Accept or reject the reference-machine software-`wgpu` frame times for 1.0 (H8). | L2 |
| O-SIGN | Unsigned release accepted, or provide a certificate. | L3 |
| O-REL | Whether the release MSI must come from a tag-triggered CI job (C9). | L3 |

## 7. Boundaries for whoever executes this plan

- No pushes, tags, merges, installs, DPI or system-setting changes, or Credential Manager writes without the matching owner approval above.
- Never read, print or store the owner's API key, password or real username in docs, logs or commits. Use synthetic fixtures for every automated test.
- Live Wallhaven requests: SFW only, read-only, ≥ 2.5 s apart, ≤ 25 per session, stop on the first 429. Never trigger rate limiting on purpose.
- Hardware-dependent results count only from the reference machine (decision §5). Label anything else "not reference hardware".
- Manual smoke runs on a development PC should use C8's data-root override once it exists. Until then, check for and record any directories the run creates, as the 2026-11-25 ledger did.
