# Wallora privacy inventory

Updated 2026-11-23 for the code at `main` (`3b782aa`), covering Tasks 1–3. The cache row below describes the pending Task 4 change on branch `t3/image-cache-pipeline`, which is not merged yet. This file must be updated in the same change as any new host, file, or stored field (plan: *Cross-cutting → Privacy*).

**Summary:** Wallora has no telemetry, no analytics, no crash upload, and no automatic update check. It talks only to Wallhaven, plus whatever proxy you configure. Everything it stores stays on your machine.

## Hosts contacted

| Host | When | What is sent |
| --- | --- | --- |
| `https://wallhaven.cc` path `/api/v1/…` | Sign-in checks, account refresh, search and wallpaper lookups | Request path and query. `User-Agent: Wallora/<version> (+https://github.com/ppolkadot/Wallora)`. The API key goes in an `X-API-Key` header **only** when you are signed in, and only to this exact origin (HTTPS, port 443, `/api/v1/` path). It is never put in a URL. Redirects are not followed. |
| `https://w.wallhaven.cc`, `https://th.wallhaven.cc` | Image downloads. The size-bounded fetch exists in the core library but **no screen uses it yet** (arrives with Task 4/5). | A plain GET with no API key, no cookies and no `Referer`. Image redirects are followed only within the same host and port, at most five times. |
| Your default browser → `https://wallhaven.cc/…` | When you press **Open on wallhaven.cc** | Only the validated Wallhaven URL is handed to Windows `ShellExecuteW`. Wallora sends nothing itself. |
| Your proxy, if configured | All of the above | Wallora honours environment and basic Windows proxy settings, plus an optional manual proxy URL in Settings (stored locally; URLs containing a username/password are rejected). |

Request pacing keeps API traffic under 40 requests per minute (Wallhaven's limit is 45).

## Files written

| Location | Content |
| --- | --- |
| `%LOCALAPPDATA%\Wallora\data\wallora.db` (SQLite) | `accounts`: random account id, **Wallhaven username**, verification state, active flag, timestamps. `account_cache`: cached copies of your upstream settings and your own collection list (names, counts) for the active account. `local_preferences`: proxy mode and URL. `pending_credential_cleanup`: account ids awaiting key removal. **The API key is never stored here.** |
| `%LOCALAPPDATA%\Wallora\logs\wallora.log.<date>` | Daily rolling application log (`info` level and above by default). Provider tests assert that the API key never appears in request logs or error text. |
| `%LOCALAPPDATA%\Wallora\logs\panic.log` | Appended on a crash: source location and panic message, **not scrubbed**. A panic message could in principle contain arbitrary text, so review this file before sharing it. |
| `%LOCALAPPDATA%\Wallora\cache\` | Arrives with Task 4 (branch `t3/image-cache-pipeline`, pending merge), so it is still empty in current builds. It will hold: `cache.lock` (exclusive lock, one process at a time); `cache_index.db`, a SQLite index of cached entries holding the namespace (guest or the random local account id), cache key, file path, size, checksum, last-access order, timestamps and pending deletions; `objects/` (published thumbnail and preview files); `staging/` (in-progress downloads); `cache.active` (an empty marker present while the cache is open — if it is still there at the next start, the app crashed or exited without an orderly cache shutdown, so all account-scoped entries are discarded at startup as a safety measure, while guest entries are kept); and `cache_index.db.corrupt-<timestamp>` (an unusable index moved aside, after which the cache rebuilds and deletes unindexed files). The index is separate from `wallora.db` and records which wallpapers were cached, but never the API key. Guest content uses one namespace and each account its own; an account's entries are deleted on sign-out, when switching to another account, on key revocation, and on a 403 during account refresh. Guest content is kept. |
| `%APPDATA%\Wallora\` | Created, but nothing is written there yet. |

## Windows Credential Manager

Your API key is saved as a Generic Credential with service name `Wallora`, one entry per account id. Removing an account from Wallora deletes its entry; if deletion fails, the account id is queued in `pending_credential_cleanup` and retried.

## Installer

Per-machine MSI into `Program Files\Wallora`. It does not modify `PATH` (CI verifies the MSI has no `Environment` table) and does not register startup entries. Startup and tray options are not implemented yet.

## Not collected

No usage statistics, no device identifiers, no IP-address logging by Wallora, no remote error reporting, no advertising or tracking SDKs. Wallhaven itself sees ordinary web-request data (your IP address, user agent, requested URLs) as for any API client.

## Developer-only items (not shipped)

`crates/wallora-core/examples/probe.rs` is an opt-in developer tool. It reads `WALLHAVEN_API_KEY` and `WALLHAVEN_USERNAME` from the environment or a git-ignored `.env`, never prints them, and is not part of the release build.
