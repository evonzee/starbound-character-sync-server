# Adding player sync to starbound-modpack-launcher

Handoff for an agent working in the **launcher** repo (`../starbound-modpack-launcher`,
Tauri 2 + Rust backend in `src-tauri/`, React/TS frontend in `src/`). The server
is this repo; **[README.md](../README.md) is the API reference**, this doc is how to use it
from the launcher. If they disagree, `src/api.rs` here is the truth.

## Goal

Sync each family member's Starbound characters between machines. The launcher
owns the game lifecycle, so there is no daemon: **pull before launch, push after
the game exits**. Local data is never destroyed without a backup.

## Facts about the launcher (verified)

- Save dir to sync: `<starbound_dir>/base10/storage/player/` (`starbound_dir`
  from `prefs::get_starbound_dir()`; `base10/` is created in
  `write_config_file_to_dir`, and `modpack.config` sets `storageDirectory` to it).
  Only `*.player` and `*.shipworld` files in that one directory (not recursive).
  Ignore everything else (`universe/` etc.).
- Pack version: `get_modpack_version("mods.json")` (installed) reads
  `metadata.version` from `mods.json`. Send it as `X-Pack-Version` on uploads.
- Launch: `launch` -> `launch_starbound` -> `run_starbound`, which does
  `Command::spawn()` then a **blocking** `child.wait()` inside an async fn.
  Pull goes before that; push goes after it returns (on any exit status).
  Consider moving the wait onto `spawn_blocking` / `tauri::async_runtime` so the
  push can be awaited cleanly and the UI stays responsive.
- Prefs: `prefs.rs` uses the `preferences` crate with string keys
  (`starbound_dir`). Add `sync_url`, `sync_token`, `sync_enabled`, `machine_name`
  the same way. Default `machine_name` to the hostname (a dependency-free option:
  `HOSTNAME`/`COMPUTERNAME` env, else `"unknown"`); it is shown to the user in
  "playing on X" warnings, so let them edit it.
- Already available: `reqwest` (add the `json` feature), `sha2` (note the
  existing code uppercases hex; the server uses **lowercase** hex; normalise),
  `serde`/`serde_json`, `tauri-plugin-dialog`, `tauri-plugin-notification`,
  `log(&window, ..)` / `set_status(..)` events for UI messages.
- `BASE_URL` is hardcoded for mod downloads. The sync server is a *different*
  host: make `sync_url` a setting, don't reuse `BASE_URL`.

## Server contract in brief

Base: `{sync_url}/v1/packs/{pack}/files`, header `Authorization: Bearer <token>`.
**pack** = slug, hardcode `"base10"` in one constant (`const PACK_ID`) -- the
launcher is single-pack today but make it a value passed down, not scattered,
because a switchable pack is planned.

- `GET  /files` -> `{files:[{name,version,sha256,size,pack_version,updated_by,updated_at,deleted}]}`
  (includes tombstones, `deleted:true`).
- `GET  /files/{name}` -> bytes; headers `X-Version`, `X-Sha256`, `X-Pack-Version`.
- `PUT  /files/{name}` -> body = raw bytes. **Must** send `If-Match: "<version>"`
  (last synced version) or `If-None-Match: *` (file never seen on server).
  Send `X-Machine`, `X-Pack-Version`, `X-Sha256`. `201` created / `200` identical
  to head (retry-safe) / `412 {current}` conflict / `428` forgot precondition.
- `DELETE /files/{name}` + `If-Match` -> tombstone.
- `GET /files/{name}/history`, `POST /files/{name}/restore/{version}`.
- Lease: `POST /v1/packs/{pack}/lease` `{machine, force?}` -> `200` or
  `409 {holder:{machine,expires_at}}`; `DELETE` same path releases; `GET` shows.
  TTL is 10 min, so **renew every ~5 min while the game runs**.
- Names must be `<id>.player` / `<id>.shipworld` (alnum, `-`, `_`).
- Treat 401 as "bad token" (surface it, don't retry). Network failure must
  never block launching the game: warn and continue.

## Design

### Module `src-tauri/src/sync/`

Keep **planning pure** and I/O separate so planning is unit-testable:

- `plan.rs` (pure): `fn plan(local, remote, base) -> Vec<(name, Action)>` where
  - `local`: `name -> {sha256, mtime}` from scanning the dir
  - `remote`: manifest entries from the server
  - `base`: last-synced state per name `{version, sha256}` from `sync_state.json`
  - `Action` = `Pull | Push | Conflict | Delete(local->remote) | DeletedRemotely | Noop`
- Decision table per file (L=local sha, R=remote head, B=base):
  - L==R sha -> `Noop` (record base = R.version/sha)
  - no B, only local -> `Push` (`If-None-Match: *`)
  - no B, only remote -> `Pull`
  - no B, both exist, differ -> `Conflict` (first sync on a second machine!)
  - B exists, L==B, R.version>B.version -> `Pull`
  - B exists, L!=B, R.version==B.version -> `Push` (`If-Match: B.version`)
  - B exists, L!=B and R.version>B.version -> `Conflict`
  - B exists, local missing, R unchanged -> remote delete? **Do not auto-delete
    on the server** from a missing local file in v1; treat as `Pull` (restore
    it locally). Deleting is an explicit UI action only.
  - R.deleted and L==B -> `DeletedRemotely`: move local file to the backup dir
    rather than deleting; R.deleted and L!=B -> `Conflict`.
- `client.rs` (I/O): thin reqwest wrapper over the endpoints above, returning
  typed errors (`Conflict(current)`, `Unauthorized`, `Network`, ...).
- `state.rs`: `sync_state.json` in the app config dir, keyed by `pack` then
  filename. Write atomically (temp + rename).
- `engine.rs`: scan dir -> fetch manifest -> `plan` -> execute -> update state.

### Safety rules (important; these are somebody's kids' characters)

1. **Atomic writes**: download to `<name>.tmp` in the same dir, verify sha256
   equals `X-Sha256`, then rename over the target.
2. **Back up before overwrite**: before any pull replaces an existing local
   file, copy it to `<starbound_dir>/base10/sync-backups/<timestamp>/<name>`.
   Keep the last ~20 backup sets, prune older.
3. **Never push a file the game may be writing**: only push after the game has
   exited, or (optional mid-session) when the file's mtime has been quiet >=5 s.
4. **Pack-version check**: if the remote file's `pack_version` is *newer* than
   the installed pack, do not pull it (it may lose items in an older pack);
   surface "update your modpack first". Compare as semver when parseable, else
   treat inequality as a warning the user can override.
5. **Conflicts are never auto-resolved.** Show both sides (name, size,
   `updated_by`, `updated_at`) and let the user choose "keep mine" (re-PUT with
   `If-Match: <current.version>` from the 412 body) or "keep server's" (pull,
   after the local backup). The loser remains in server history / local backup.
6. Missing/invalid config (no url/token) = sync silently disabled, launch works.

### Launch flow

1. If sync enabled: `POST lease` (`machine_name`). On `409`, show
   "Playing on `<holder>` since ..., continue anyway?" -> on yes retry with
   `force:true`; on no, abort the launch.
2. Run engine in **pull** mode (pull-only actions + report conflicts). If there
   are conflicts, stop and ask before launching.
3. Spawn the game; start a 5-min lease-renew timer while it runs.
4. On exit: stop timer, run engine in **push** mode, then `DELETE lease`.
   Do the release even if the push failed. If the push hit a conflict or
   network error, tell the user (notification + log) and leave local files
   untouched; the next launch's pull step will retry.

### Tauri commands to expose (names are suggestions)

`sync_get_settings`, `sync_set_settings{url,token,enabled,machine}`,
`sync_test_connection`, `sync_status` (per-file badge:
`synced|ahead|behind|conflict|local-only|remote-only`), `sync_now`,
`sync_resolve{name, keep: "local"|"remote"}`, `sync_history{name}`,
`sync_restore{name, version}`. Emit progress via the existing `log` event.

### Frontend (`src/App.tsx`)

Settings: server URL, token (password field), machine name, enable toggle,
"Test connection". Main: a "Sync" panel listing files grouped by character id
(pair `.player` + `.shipworld`) with a status badge, a "Sync now" button, a
conflict dialog, and a history/restore dialog. Character *names* are optional
polish (the `.player` file is SBON-wrapped JSON); until then show the id and let
the user attach a local label (store in sync state).

## Build order (ship each step)

1. `client.rs` + `state.rs` + `plan.rs` with unit tests for every row of the
   decision table above. Settings + `sync_test_connection`. Manual "Sync now".
2. Pre-launch pull + post-exit push wired into `launch`.
3. Conflict dialog + history/restore.
4. Lease + renew + pack-version check.
5. Character names / polish.

## Testing against a real server

    cd ../starbound-sync-server
    cargo run -- --data-dir /tmp/sbsync-dev add-user alice     # prints token
    cargo run -- --data-dir /tmp/sbsync-dev serve --listen 127.0.0.1:8080

Then point the launcher at `http://127.0.0.1:8080` with that token. Plain `http`
is fine for localhost; production is behind HTTPS. Verify with two different
`machine_name`s and two local save dirs to exercise pull, push, conflict and
lease paths.
