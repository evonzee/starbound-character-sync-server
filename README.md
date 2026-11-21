# starbound-sync-server

Player-save sync for the Starbound modpack launcher. Stores each user's
`storage/player/*.player` / `*.shipworld` files, grouped by **pack**, with
version history, optimistic-concurrency conflict detection and an advisory
"currently playing on" lease. Plain HTTP; run it behind your HTTPS proxy.

## Run

    docker compose up -d --build
    docker compose exec sync starbound-sync-server add-user eric   # prints token once
    # also: reset-token, revoke-user, list-users

Config (flags or env): `SYNC_DATA_DIR` (/data), `SYNC_LISTEN`, `SYNC_HISTORY`
(versions kept per file, default 20), `SYNC_MAX_UPLOAD_MB` (32),
`SYNC_LEASE_TTL` (600 s). Data = `sync.db` (SQLite) + `blobs/` (zstd,
content-addressed). Back up the whole directory.

## Concepts

- **pack**: slug `[a-z0-9_-]{1,64}` in the URL (e.g. `base10`). Everything is
  scoped per user *and* pack, so the launcher can later switch packs by
  changing one string. Each upload may carry `X-Pack-Version` (the pack's
  `mods.json` version); the launcher decides whether a mismatch warns/refuses.
- **file name**: `<id>.player` or `<id>.shipworld`.
- **version**: per-file integer, the ETag. Every change (including delete and
  restore) appends a new version; the highest is the head.
- Deletes are tombstones (`deleted: true` in the manifest).

## API (all but `/healthz` need `Authorization: Bearer <token>`)

| | |
|---|---|
| `GET /v1/packs` | packs this user has data in |
| `GET /v1/packs/{pack}/files` | manifest: `{files:[{name,version,sha256,size,pack_version,updated_by,updated_at,deleted}]}` |
| `GET .../files/{name}` | head content; headers `ETag`, `X-Version`, `X-Sha256`, `X-Pack-Version`. 404 if missing/deleted |
| `PUT .../files/{name}` | upload. **Requires** `If-Match: "<last synced version>"` or `If-None-Match: *`. Optional `X-Machine`, `X-Pack-Version`, `X-Sha256`. `201` new head, `200` if identical to head (safe retry), `412 {current}` on conflict, `428` if no precondition |
| `DELETE .../files/{name}` | tombstone; requires `If-Match` |
| `GET .../files/{name}/history` | all retained versions, newest first |
| `POST .../files/{name}/restore/{version}` | re-publish an old version as a new head (optional `If-Match`) |
| `GET/POST/DELETE /v1/packs/{pack}/lease` | JSON `{machine, force?}`. POST acquires/renews (`409 {holder}` if another machine holds a live lease; `force` takes over). DELETE releases if you hold it. Expires after the TTL, so crashes don't strand it |

Client flow: pull anything newer → take lease → play (renew lease
periodically) → push changed files with `If-Match` = version last synced →
release lease. On `412`, keep both sides and ask the user; the loser stays in
history either way.

## Dev

    cargo test
