//! HTTP API. All `/v1` routes need `Authorization: Bearer <token>`; data is
//! scoped per user and per pack.
//!
//! Concurrency: every file has an integer version (the ETag). Mutations must
//! send `If-Match: "<version you last synced>"` or `If-None-Match: *` (new
//! file); a mismatch is `412` with the server's current head so the client can
//! decide who wins.
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, FromRequestParts, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    Shared, sha256_hex,
    db::{Appended, FileMeta, LeaseOutcome, NewVersion, Precondition, User},
    error::ApiError,
};

type Res<T> = Result<T, ApiError>;

pub fn router(state: Shared) -> Router {
    let limit = state.cfg.max_upload_bytes;
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/packs", get(list_packs))
        .route("/v1/packs/{pack}/files", get(manifest))
        .route(
            "/v1/packs/{pack}/files/{name}",
            get(get_file).put(put_file).delete(delete_file),
        )
        .route("/v1/packs/{pack}/files/{name}/history", get(history))
        .route("/v1/packs/{pack}/files/{name}/restore/{version}", post(restore))
        .route("/v1/packs/{pack}/lease", get(get_lease).post(acquire_lease).delete(release_lease))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

// ---------- auth ----------

pub struct Auth(pub User);

impl FromRequestParts<Shared> for Auth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Res<Self> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;
        st.db
            .user_by_token(&sha256_hex(token.trim().as_bytes()))?
            .map(Auth)
            .ok_or(ApiError::Unauthorized)
    }
}

// ---------- validation / header helpers ----------

fn valid_pack(p: &str) -> Res<()> {
    let ok = (1..=64).contains(&p.len())
        && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    ok.then_some(()).ok_or_else(|| ApiError::BadRequest("pack must match [a-z0-9_-]{1,64}".into()))
}

/// `<uuid>.player` / `<uuid>.shipworld` style names (never used as paths).
fn valid_name(n: &str) -> Res<()> {
    let ok = n.len() <= 128
        && matches!(n.rsplit_once('.'), Some((stem, "player" | "shipworld"))
            if !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    ok.then_some(()).ok_or_else(|| {
        ApiError::BadRequest("file name must be <id>.player or <id>.shipworld".into())
    })
}

fn hdr<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

fn machine(h: &HeaderMap) -> String {
    hdr(h, "x-machine").unwrap_or("unknown").chars().take(64).collect()
}

fn precondition(h: &HeaderMap) -> Res<Precondition> {
    if let Some(v) = hdr(h, "if-match") {
        let n = v.trim_matches('"').parse::<i64>().map_err(|_| ApiError::BadRequest("If-Match must be a version number".into()))?;
        return Ok(Precondition::Version(n));
    }
    if hdr(h, "if-none-match") == Some("*") {
        return Ok(Precondition::Absent);
    }
    Err(ApiError::PreconditionRequired)
}

fn etag(v: i64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{v}\"")).unwrap()
}

fn meta_response(status: StatusCode, m: &FileMeta) -> Response {
    (status, [(header::ETAG, etag(m.version))], Json(m)).into_response()
}

// ---------- handlers ----------

async fn list_packs(State(st): State<Shared>, Auth(u): Auth) -> Res<Json<serde_json::Value>> {
    Ok(Json(json!({ "packs": st.db.packs(u.id)? })))
}

async fn manifest(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(pack): Path<String>,
) -> Res<Json<serde_json::Value>> {
    valid_pack(&pack)?;
    Ok(Json(json!({ "user": u.name, "pack": pack, "files": st.db.manifest(u.id, &pack)? })))
}

fn download(st: &Shared, m: &FileMeta) -> Res<Response> {
    if m.deleted {
        return Err(ApiError::NotFound);
    }
    let body = st.blobs.get(&m.sha256)?;
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, etag(m.version));
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert("x-sha256", HeaderValue::from_str(&m.sha256).unwrap());
    h.insert("x-version", HeaderValue::from(m.version));
    if let Some(pv) = m.pack_version.as_deref().and_then(|p| HeaderValue::from_str(p).ok()) {
        h.insert("x-pack-version", pv);
    }
    Ok((h, body).into_response())
}

/// Download the head. `?version=N` isn't a thing — use history + restore.
async fn get_file(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path((pack, name)): Path<(String, String)>,
) -> Res<Response> {
    valid_pack(&pack)?;
    valid_name(&name)?;
    let m = st.db.head(u.id, &pack, &name)?.ok_or(ApiError::NotFound)?;
    download(&st, &m)
}

async fn put_file(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path((pack, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Res<Response> {
    valid_pack(&pack)?;
    valid_name(&name)?;
    let pre = precondition(&headers)?;
    if body.is_empty() {
        return Err(ApiError::BadRequest("empty body".into()));
    }
    let sha = sha256_hex(&body);
    if let Some(claimed) = hdr(&headers, "x-sha256") {
        if !claimed.eq_ignore_ascii_case(&sha) {
            return Err(ApiError::BadRequest("x-sha256 does not match body".into()));
        }
    }
    let machine = machine(&headers);
    let pack_version = hdr(&headers, "x-pack-version").map(|s| s.chars().take(64).collect::<String>());

    let _w = st.write_lock.lock().await;
    {
        let (st2, sha2, body2) = (st.clone(), sha.clone(), body.clone());
        tokio::task::spawn_blocking(move || st2.blobs.put(&sha2, &body2))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))??;
    }
    let new = NewVersion {
        sha256: &sha,
        size: body.len() as i64,
        pack_version: pack_version.as_deref(),
        machine: &machine,
        deleted: false,
    };
    finish_append(&st, st.db.append(u.id, &pack, &name, pre, &new, st.cfg.history)?, StatusCode::CREATED)
}

fn finish_append(st: &Shared, r: Appended, created: StatusCode) -> Res<Response> {
    match r {
        Appended::Created { meta, orphans } => {
            for sha in orphans {
                st.blobs.delete(&sha);
            }
            Ok(meta_response(created, &meta))
        }
        Appended::Unchanged(m) => Ok(meta_response(StatusCode::OK, &m)),
        Appended::Conflict(cur) => Err(ApiError::PreconditionFailed(cur)),
    }
}

async fn delete_file(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path((pack, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Res<Response> {
    valid_pack(&pack)?;
    valid_name(&name)?;
    let pre = match precondition(&headers)? {
        Precondition::Absent => return Err(ApiError::NotFound),
        p => p,
    };
    if st.db.head(u.id, &pack, &name)?.is_none() {
        return Err(ApiError::NotFound);
    }
    let machine = machine(&headers);
    let new = NewVersion { sha256: "", size: 0, pack_version: None, machine: &machine, deleted: true };
    let _w = st.write_lock.lock().await;
    finish_append(&st, st.db.append(u.id, &pack, &name, pre, &new, st.cfg.history)?, StatusCode::OK)
}

async fn history(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path((pack, name)): Path<(String, String)>,
) -> Res<Json<serde_json::Value>> {
    valid_pack(&pack)?;
    valid_name(&name)?;
    Ok(Json(json!({ "versions": st.db.history(u.id, &pack, &name)? })))
}

/// Make an old version the new head (a new version number; history is kept).
/// `If-Match` is optional here.
async fn restore(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path((pack, name, version)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> Res<Response> {
    valid_pack(&pack)?;
    valid_name(&name)?;
    let _w = st.write_lock.lock().await;
    let target = st.db.version(u.id, &pack, &name, version)?.ok_or(ApiError::NotFound)?;
    if target.deleted {
        return Err(ApiError::BadRequest("that version is a deletion; nothing to restore".into()));
    }
    let pre = if hdr(&headers, "if-match").is_some() { precondition(&headers)? } else { Precondition::Any };
    let machine = machine(&headers);
    let new = NewVersion {
        sha256: &target.sha256,
        size: target.size,
        pack_version: target.pack_version.as_deref(),
        machine: &machine,
        deleted: false,
    };
    finish_append(&st, st.db.append(u.id, &pack, &name, pre, &new, st.cfg.history)?, StatusCode::CREATED)
}

// ---------- lease ----------

#[derive(Deserialize)]
struct LeaseReq {
    machine: String,
    /// Take over even if another machine holds it.
    #[serde(default)]
    force: bool,
}

async fn get_lease(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(pack): Path<String>,
) -> Res<Json<serde_json::Value>> {
    valid_pack(&pack)?;
    Ok(Json(json!({ "lease": st.db.lease(u.id, &pack)? })))
}

/// Acquire or renew. 409 + holder if another machine has a live lease.
async fn acquire_lease(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(pack): Path<String>,
    Json(req): Json<LeaseReq>,
) -> Res<Json<serde_json::Value>> {
    valid_pack(&pack)?;
    if req.machine.is_empty() || req.machine.len() > 64 {
        return Err(ApiError::BadRequest("machine must be 1-64 chars".into()));
    }
    match st.db.acquire_lease(u.id, &pack, &req.machine, st.cfg.lease_ttl_secs, req.force)? {
        LeaseOutcome::Granted(l) => Ok(Json(json!({ "lease": l }))),
        LeaseOutcome::Held(l) => Err(ApiError::LeaseHeld(l)),
    }
}

async fn release_lease(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(pack): Path<String>,
    Json(req): Json<LeaseReq>,
) -> Res<Json<serde_json::Value>> {
    valid_pack(&pack)?;
    Ok(Json(json!({ "released": st.db.release_lease(u.id, &pack, &req.machine)? })))
}
