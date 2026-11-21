# Running starbound-sync-server

Operator guide. API details are in [../README.md](../README.md); client work is in
[client-integration.md](client-integration.md).

## docker compose

Image: `base10/starbound-character-sync-server` (built by the GitHub Action).

```yaml
playersync:
    image: base10/starbound-character-sync-server
    ports:
      - 21010:8080
    volumes:
      - ./playersync:/data
    restart: unless-stopped
    # optional tuning:
    # environment:
    #   SYNC_HISTORY: "20"        # versions kept per file
    #   SYNC_MAX_UPLOAD_MB: "32"
    #   SYNC_LEASE_TTL: "600"     # seconds before an unrenewed lease expires
```

Port 21010 on the host is published on all interfaces, so the sync server is
reachable directly over plain HTTP unless your firewall or proxy prevents it.
Tokens travel in a header, so only expose it through HTTPS (see below), or bind
it to localhost with `127.0.0.1:21010:8080` and let the proxy reach it there.

The container runs as uid **10001**, so the data directory must be writable by it:

    mkdir -p playersync && sudo chown 10001 playersync

(With rootless podman, add `:U` to the volume instead: `./playersync:/data:U`.)

To build locally instead of pulling, replace `image:` with `build: .` (see
`compose.yaml` in the repo root).

    docker compose up -d playersync
    docker compose logs -f playersync
    curl http://127.0.0.1:21010/healthz        # -> ok

## HTTPS

The server speaks plain HTTP; put it behind your existing proxy. Tokens travel in
the `Authorization` header, so do **not** expose the port directly.

Caddy:

    sync.example.org {
        reverse_proxy 127.0.0.1:21010
    }

nginx (raise the body limit above `SYNC_MAX_UPLOAD_MB`):

    location / {
        proxy_pass http://127.0.0.1:21010;
        client_max_body_size 40m;
    }

## Managing users and tokens

One bearer token per person. Tokens are stored hashed and **shown only once**, so
copy it when it's printed. Run these against the running stack:

    # create a user; prints their token
    docker compose exec playersync starbound-sync-server add-user eric

    # list users (shows revoked ones)
    docker compose exec playersync starbound-sync-server list-users

    # lost or leaked token: issue a new one (the old one stops working immediately)
    docker compose exec playersync starbound-sync-server reset-token eric

    # cut someone off (their data is kept)
    docker compose exec playersync starbound-sync-server revoke-user eric

    # re-enable a revoked user by issuing a fresh token
    docker compose exec playersync starbound-sync-server reset-token eric

Notes:
- Each user's data is separate. Create one user per person, and give them the
  token for the launcher's sync settings (URL + token).
- Users can't be renamed or deleted; revoke instead. Adding an existing name fails.
- Without compose: `docker exec <container> starbound-sync-server add-user eric`.
  With podman, replace `docker` with `podman`.

## Quick manual test with curl

    URL=https://sync.example.org
    TOKEN=<token from add-user>
    ID=11111111-2222-3333-4444-555555555555

    # upload a new file (the precondition header is mandatory)
    curl -X PUT "$URL/v1/packs/base10/files/$ID.player" \
         -H "Authorization: Bearer $TOKEN" -H "If-None-Match: *" \
         -H "X-Machine: testbox" --data-binary @some.player

    # manifest, history, download
    curl -H "Authorization: Bearer $TOKEN" "$URL/v1/packs/base10/files"
    curl -H "Authorization: Bearer $TOKEN" "$URL/v1/packs/base10/files/$ID.player/history"
    curl -H "Authorization: Bearer $TOKEN" "$URL/v1/packs/base10/files/$ID.player" -o out.player

    # "my kid deleted their character": restore version 3 as a new head
    curl -X POST -H "Authorization: Bearer $TOKEN" \
         "$URL/v1/packs/base10/files/$ID.player/restore/3"

## Configuration reference

| Env var | Flag | Default | Meaning |
|---|---|---|---|
| `SYNC_DATA_DIR` | `--data-dir` | `/data` in the image, `./data` otherwise | database + blobs |
| `SYNC_LISTEN` | `--listen` | `0.0.0.0:8080` | bind address |
| `SYNC_HISTORY` | `--history` | 20 | versions kept per file; older ones are pruned and their data deleted |
| `SYNC_MAX_UPLOAD_MB` | `--max-upload-mb` | 32 | max single upload |
| `SYNC_LEASE_TTL` | `--lease-ttl` | 600 | seconds a lease lives without renewal |

## Backups

Everything is in the data directory (`./playersync`): `sync.db` (+ `sync.db-wal`/`-shm` while
running) and `blobs/`. Simplest safe backup: stop the container briefly, copy
the data directory, start it again:

    docker compose stop playersync && tar czf sbsync-$(date +%F).tgz playersync && docker compose start playersync

Because the version history is itself an undo for accidental deletes, backups
mostly protect against losing the server. For a hot backup of the database use
`sqlite3 playersync/sync.db ".backup out.db"` on the host, plus a copy of `blobs/`
taken *after* it (blobs are only ever added or garbage-collected).

## Upgrading

    docker compose pull playersync && docker compose up -d playersync

The schema is created on startup (`CREATE TABLE IF NOT EXISTS`); there are no
migrations yet, so check release notes before upgrading across schema changes.

## Troubleshooting

- **Container exits with a database/permission error**: the data dir isn't
  writable by uid 10001 (see above).
- **Launcher gets 401**: wrong/old token, or the user was revoked.
- **412 on upload**: not a fault; another machine pushed first. The launcher
  should offer a conflict choice.
- **413 on upload**: file exceeds `SYNC_MAX_UPLOAD_MB` (or your proxy's limit).
- **409 on lease**: another machine is marked as playing; it expires after the
  TTL, or the client can force it.
