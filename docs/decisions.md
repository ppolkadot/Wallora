# Wallora Architecture & Scope Decisions

## 1. Windows Proxy Behavior (`reqwest` `0.12.28`, `system-proxy`) — Task 1a / Network Policy

- **Status:** Verified via crate configuration and `hyper-util` `system-proxy` implementation audit (2026-11-18).
- **What works out of the box:** `reqwest` with `system-proxy` reads `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` environment variables as well as the basic Windows HKCU WinINET registry keys (`Software\Microsoft\Windows\CurrentVersion\Internet Settings\ProxyEnable` + a single host/URL `ProxyServer`).
- **Known Windows limitations:** Upstream `hyper-util`'s Windows registry reader does **not** parse:
  - Per-scheme `ProxyServer` strings (e.g. `http=proxy:8080;https=proxy:8443`),
  - PAC (`AutoConfigURL`) or WPAD auto-discovery scripts, or
  - `<local>` and IP-wildcard `ProxyOverride` rules.
- **Decision:** Documented as a known limitation; a manual proxy setting will be added in Task 3 so users behind PAC/WPAD or per-protocol Windows proxies can configure an explicit proxy URL.

---

## 2. GUI Toolkit Spike (`spikes/iced-gallery`) — Task 1d

- **Decision status:** **Settled by the owner on 2026-11-23: Iced is mandatory** (see §4). It was deferred on 2026-11-19 pending hardware checks; §5 now scopes those checks to the reference machine. `iced = "=0.14.0"` stays pinned.

### Reference Machine Used for Automated Run (2026-11-18)

| Component | Observed Value |
|---|---|
| CPU | 12th Gen Intel(R) Core(TM) i5-12400F |
| GPU | NVIDIA GeForce GT 340 (Direct3D 10.1 legacy GPU — no hardware DX12 or Vulkan driver) |
| RAM | 15.85 GB |
| Display | 1440×900 @ 74 Hz (below 1600×1000 spike target; single DPI only) |
| OS | Microsoft Windows 11 Pro 10.0.26200 (Build 26200) |

### Measured Automated Results (`cargo build --locked --release -p iced-gallery`, `--benchmark`)

| Criterion | `wgpu` (default) | `tiny-skia` (`ICED_BACKEND=tiny-skia`) | Target / Threshold | Outcome |
|---|---|---|---|---|
| 2,000-tile off-thread Tokio JPEG decode wall time | `353 ms` | `168 ms` | Off UI thread via `Task` | **Pass** |
| Resident Set Size (RSS) after 2,000 decoded RGBA tiles | `276.93 MB` | `262.46 MB` | `<= 500 MB` | **Pass** |
| Scripted 120-frame scroll `p50` | `79.89 ms` | `79.75 ms` | — | — |
| Scripted 120-frame scroll `p95` | `92.31 ms` | `92.96 ms` | `<= 16.7 ms` (record vs `8.33 ms`) | **Fail on this GPU** (likely software fallback on GT 340) |
| Scripted 120-frame scroll `p99` / `max` | `97.20 ms` / `101.48 ms` | `103.08 ms` / `136.89 ms` | — | — |
| Idle redraws (2 s steady-state window after 200 ms settle) | `0` (`2014 ms`) | `0` (`2006 ms`) | `0` continuous redraws | **Pass** |
| Idle CPU during 2 s wait | `0 ms` (`0.00%`) | `0 ms` (`0.00%`) | `~0%` | **Pass** |
| Custom `.ttf` font load (`Segoe UI` via `iced::font::load`) | `true` | `true` | Loads cleanly | **Pass** |
| Startup | Starts cleanly | Starts cleanly | Both backends start | **Pass** |
| Virtualized row rendering + keyboard nav (`Arrows`/`PgUp`/`PgDn`/`Home`/`End`) | Implemented | Implemented | Full grid keyboard nav | **Code verified** |

### Why Decision Is Deferred & What Needs a Human on Real Hardware

1. **Frame timing (`p95 = 92.31 ms` vs `<= 16.7 ms`):** Because the test host's GeForce GT 340 lacks DirectX 12 and Vulkan hardware support, `wgpu` falls back to software rasterization (matching `tiny-skia`'s ~80 ms median frame time). Re-run `target\release\iced-gallery.exe --benchmark` on a machine with a hardware DX12/Vulkan GPU before deciding whether to accept Iced or trigger the Slint fallback spike.
2. **Viewport & multi-DPI visual check:** Test interactively (`target\release\iced-gallery.exe`) on a `>= 1600×1000` display at **100%**, **150%**, and **200%** Windows DPI scaling to confirm layout crispness, font scaling, and scroll feel.
3. **Live resize smoothness:** Drag-resize the spike window manually to verify there are no visual stalls or device-loss panics.
4. **Accessibility (Narrator):** Run Windows Narrator over the spike window and record its output. Per the plan, incomplete screen-reader support in Iced is a documented limitation rather than an automatic fallback trigger, but must be recorded for v1.0.
5. **Font licensing (deferred before bundling):** Confirm whether `Salted Flutters Polaris` (owner's repo) has an explicit redistribution license or verify `Google Sans` license terms before bundling either font in `crates/wallora-app`.

---

## 3. Parity Scope Checkpoint — Task 1e

> **Status: Option A approved by the owner on 2026-11-19.**

### Gap Summary (from `docs/api-capabilities.md` & `docs/parity-matrix.md`)

Based on `https://wallhaven.cc/help/api` and the API and guest website observations recorded through 2026-11-19:

- **Missing routes:** The public Wallhaven API is read-only (`GET` only). The following site features have no official API route:
  - Favorite / unfavorite wallpaper
  - Collection CRUD & public/private toggles
  - Account settings updates
  - Wallpaper uploads
  - Tagging & moderation actions (add/remove/report/vote tags)
  - Profile comments (read & post). Wallpaper comments remain unconfirmed as a site feature.
  - Forums (read & post)
- **Website-only gaps:** Exact homepage and profile coverage, Hot, crop/scale, the tag directory, and the Few tags queue. AI-art search has no documented API filter. Signed-in control visibility still needs checking.
- **Behavior to verify:** The supplied account's public collection lookup returned 404. Task 6a still needs signed-in Favorites label, empty-state and mutation evidence. API tag-similarity behavior and native original-byte equivalence still need verification.
- **Phase B entry:** Task 7 must pass in full under the revised gate before desktop-only work starts.

### Options for Owner Decision

- **(A) Revise the parity gate** — Redefine 1.0 to mean *"complete for supported routes"*, with an explicit list of documented gaps and labelled external-browser hand-offs (`Open on wallhaven.cc`). *Requires explicit owner sign-off here as a scope revision.*
- **(B) Keep the original parity gate** — Releases stay labelled **beta / partial client** until all required parity gaps close; **Phase B remains blocked** until Task 7 passes in full under the original gate (or under an owner-revised gate if the owner later chooses Option A).
- **(C) Pursue upstream authorization / partnership** for specific gaps in parallel with **(B)** — Optional, never a launch dependency.

**Owner decision (2026-11-19): Option A.** Wallora 1.0 means *complete for supported routes*. Document each gap and provide a labelled external-browser hand-off (`Open on wallhaven.cc`) where applicable. The Task 1c inventory and Task 7 release checks remain required.

---

## 4. GUI Toolkit — Iced is mandatory

> **Status: Owner decision, 2026-11-23.**

Wallora's GUI toolkit is **Iced** (`iced = "=0.14.0"`). The Slint fallback in plan Task 1d is **not pursued**, including after the recorded p95 frame-time miss in §2. Task 5 builds its gallery on Iced. Performance work, if needed, happens inside Iced (renderer choice, tile virtualization, layout cost) and is measured against the §2 method. Accessibility gaps in Iced stay documented limitations (plan Task 1d), not fallback triggers.

## 5. Hardware verification scope

> **Status: Owner decision, 2026-11-23.**

Hardware-dependent checks (frame timing, DPI 100/150/200%, live resize, Narrator, MSI install/upgrade/uninstall walkthroughs) run **only on the §2 reference machine** (i5-12400F, GeForce GT 340, 1440×900, Windows 11 Pro 10.0.26200). Results are valid for that machine only, and release notes must not claim wider hardware coverage. The GT 340 has no DX12/Vulkan hardware path, so `wgpu` runs on software fallback there. Consequence: the "GPU required" limitation in plan Task 1d is recorded for that machine, and Windows 10 coverage in Task 7 stays unverified until a machine is available (it is documented as such, not assumed).

## 6. Font licensing — Salted Flutters Polaris

> **Status: Owner statement recorded 2026-11-23; licence text checked the same day.**

The owner describes the font as MIT-licensed. The `LICENSE.txt` in `ppolkadot/Salted-Flutters-Polaris` is actually a **custom permissive licence** ("Salted Flutters Polaris Font License", version 1.0, 4 November 2026), not MIT. GitHub lists it as "Other". It permits embedding in applications and redistribution, including commercial use, subject to these conditions:

1. Redistributed font files must ship with an unmodified copy of that licence as `LICENSE.txt`.
2. The metadata line `Salted Flutters Polaris - Created by ppoolar` must remain in every style.
3. The font may not be sold or bundled for its own value. Modified versions must use the same licence and must not use the family name as their primary name.

Bundling in `crates/wallora-app` is therefore **cleared, with those conditions**, and they are mirrored in `THIRD_PARTY.md`. The owner may relicense the font under MIT or add an MIT grant; if so, update this section and `THIRD_PARTY.md`. Google Sans is not needed. System-font fallback stays in place.
