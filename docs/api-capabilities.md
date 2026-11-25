# Wallhaven API Capabilities Audit (Task 1b)

- **Audit date:** 2026-11-18
- **Probe command:** `cargo run -p wallora-core --example probe`
- **Authorization reference:** `https://wallhaven.cc/help/api` (fetched 2026-11-18)
- **Probe parameters:** 20/25 requests sent, minimum 2.1 s spacing, 0 HTTP 429 responses, `X-API-Key` loaded from `.env` (redacted; never printed or committed). `WALLHAVEN_USERNAME` was not supplied, so username-parameterized collection checks were skipped.

## Upstream Facts Verification

| Area | Status | Probe & Docs Evidence (2026-11-18) |
|---|---|---|
| Wallpaper info (`GET /api/v1/w/{id}`) | **partially confirmed** | `GET /w/94x38z` (doc example) returned `404`, so guest SFW success still needs a fresh known-live SFW id. `GET /w/<nsfw_id>` returned `401` (`{"error": "<string>"}`) as guest and `200` (`purity=nsfw`) with `X-API-Key`. Full object schema verified in `/search` + keyed `/w/<nsfw_id>`. |
| Search (`GET /api/v1/search`) | **confirmed** | `purity=111` (guest) -> `200`, 24 items with purities `{"sfw", "sketchy"}` (NSFW excluded). Documented sortings `date_added`, `relevance` (`q=nature`), `random`, `views`, `favorites`, `toplist` all returned `200`. |
| Search `sorting=hot` | **unresolved** | `GET /search?sorting=hot`, `GET /search`, and `GET /search?sorting=not_a_sort` all returned `200` with differing top IDs (`hot!=def: true`, `hot!=bad: true`, `def==bad: false`). However `https://wallhaven.cc/help/api` does **not** document `hot`, and manual side-by-side comparison with `https://wallhaven.cc/hot` was not run. Remains `unresolved` (and unauthorized) pending manual audit. |
| AI-art filter | **differs** / **missing** | `https://wallhaven.cc/help/api` documents **no** `/search` parameter for filtering AI art (`missing` / `unsupported` for search). However, `GET /api/v1/settings` (keyed) returned `ai_art_filter: number` (`differs` from docs example). |
| Search `meta` | **confirmed** | Observed fields: `current_page: number`, `last_page: number`, `per_page: number`, `total: number`, `query: null \| string \| { id: number, tag: string }`, `seed: null \| string` (`string` when `sorting=random`, `null` otherwise). |
| Query syntax | **partially confirmed** | `q=nature` -> `200` (`meta.query: string`), `q=id:1` -> `200` (`meta.query: { id: number, tag: string }`). `q=like:94x38z` -> `403` for the same id whose detail returned `404`; the reason is undetermined and the result establishes neither working similar search nor lack of support. `like:<live_sfw_id>`, `@user`, `type:`, `+tag`, `-tag` were not separately probed within the 25-request budget. |
| Tag (`GET /api/v1/tag/{id}`) | **confirmed** | `GET /tag/1` (guest) -> `200`, fields: `id: number`, `name: string`, `alias: string`, `category_id: number`, `category: string`, `purity: string`, `created_at: string`. |
| User settings (`GET /api/v1/settings`) | **differs** | Guest -> `401` (`{"error": "<string>"}`). Keyed -> `200`, fields: `thumb_size: string`, `per_page: string`, `purity: array<string>`, `categories: array<string>`, `resolutions: array<string>`, `aspect_ratios: array<string>`, `toplist_range: string`, `tag_blacklist: array<string>`, `user_blacklist: array<string>`, **`ai_art_filter: number`**. Confirms no username is returned. |
| Collections (`GET /api/v1/collections*`) | **unresolved** | Keyed `GET /collections` -> `200` (`count=1`, entry fields `id: number`, `label: string`, `views: number`, `public: number`, `count: number`, `has Default/Favorites label=true`). `/collections/{username}` and `/collections/{username}/{id}` were skipped (no `WALLHAVEN_USERNAME`), and Favorites ID stability / empty-Favorites visibility were not verified across two runs + website mutation. |
| Auth (`X-API-Key` header) | **confirmed** | `X-API-Key` header authenticated `/search?purity=001`, `/w/<nsfw_id>`, `/settings`, and `/collections`. |
| Rate limit (45 req/min -> 429) | **unresolved** | Not triggered live during the 20-request probe at >= 2.1 s spacing (per plan, rate limiting is tested with mocks, never live). Documented limit in `https://wallhaven.cc/help/api` is 45 req/min -> HTTP 429. |
| Image hosts (`w.wallhaven.cc`, `th.wallhaven.cc`) | **confirmed** | Search and wallpaper responses provide full/thumbnail URLs on `w.wallhaven.cc` and `th.wallhaven.cc` (live CDN byte download not probed in 1b). |
| Crop / scale | **unresolved** | Not present in `https://wallhaven.cc/help/api`; awaiting Task 1c manual website audit. |
| Writes (favorites, collections CRUD, upload, comments, tags, forums, settings edits) | **missing** | `https://wallhaven.cc/help/api` states the API "allows for basic GET requests via URL" and documents no write endpoints. Awaiting Task 1c manual audit to confirm there are no authorized web endpoints. |


## Targeted follow-up (2026-11-19)

Read-only requests used at least 2.1 s API spacing and aborted on a 429. The local key and username were loaded without printing their values.

| Area | Result |
|---|---|
| Key validation | Keyed `GET /settings` returned 200. |
| Own collections | Keyed `GET /collections` returned 200 on two passes. Shape check found one collection with a private flag and a Default/Favorites candidate label. |
| Supplied username | The unkeyed public-list route returned 404 on both passes. The combination of a 404 and an all-private keyed list does not establish why the public lookup failed. No shared ID verified identity. |
| Public comparison | Unkeyed public list and collection-item requests returned 200 for a public account observed in the guest browser. |
| Favorites mapping | Signed-in website label, empty state, and ID stability across a test mutation remain unverified. |
| Guest SFW detail and similar | `GET /w/e8r2y8` returned 200. Website similar search found 8 results and the corresponding documented API query returned 403. |
| Original CDN | Direct unauthenticated fetch returned an HTTP error for `e8r2y8`. The independent reviewer matched the detail API path to the website original for `3l5xev` and retrieved a 1,176,033-byte JPEG. Native byte equivalence remains a Task 6 check. |

## Favorites mapping follow-up (2026-11-23)

Probe: read-only `curl` requests (not the committed probe example) with at least 2.5 s spacing and 6 requests in total. The local key and username were loaded from `.env` without printing; no identifiers or collection ids are recorded here.

| Check | Result |
|---|---|
| Keyed `GET /collections`, two passes | 200 both times, byte-identical bodies. One collection: label `Default`, `public = 0`, `count = 2`, `views = 0`. The id was identical across the passes. |
| Keyed `GET /collections/{username}` | 404 `{"error":"Nothing here"}`. |
| Keyed `GET /collections/{username}/{id}?purity=111` | 404 `Nothing here`. |
| Unkeyed `GET /collections/{username}/{id}?purity=100` | Not valid evidence: the request was sent with an empty id because of a local scripting error, and returned 404. It was not repeated. |
| Website label, empty state, id stability after a favorite add/remove | **Not obtained.** The browser session was not signed in, a Cloudflare check blocked automation, and the handoff to sign in timed out. |

**Disposition:** the Favorites mapping stays `unresolved`. The API shows one private `Default` collection holding the account's 2 items, which is consistent with it being the site's Favorites, but that is a candidate, not evidence. Because the supplied username 404s even with the key (so the username may differ from the account's real one), account collection *items* remain unreachable under Task 3's verification rules. Next evidence needed: the real username and the Favorites page label from a signed-in browser. The add/remove stability test mutates the account and needs explicit owner action.
