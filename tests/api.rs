use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use starbound_sync_server::{AppState, Config, api, blobs::BlobStore, db::Db, sha256_hex};
use tower::ServiceExt;

const NAME: &str = "0a1b2c3d-0000-4000-8000-123456789abc.player";

struct T {
    app: Router,
    _dir: tempfile::TempDir,
}

fn setup(history: i64) -> T {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.create_user("alice", &sha256_hex(b"tok-a")).unwrap();
    db.create_user("bob", &sha256_hex(b"tok-b")).unwrap();
    let state = Arc::new(AppState {
        db,
        blobs: BlobStore::new(dir.path().to_path_buf()).unwrap(),
        cfg: Config { history, ..Default::default() },
        write_lock: Default::default(),
    });
    T { app: api::router(state), _dir: dir }
}

async fn call(
    t: &T,
    method: &str,
    uri: &str,
    tok: &str,
    hdrs: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let mut r = Request::builder().method(method).uri(uri);
    if !tok.is_empty() {
        r = r.header("authorization", format!("Bearer {tok}"));
    }
    for (k, v) in hdrs {
        r = r.header(*k, *v);
    }
    let resp = t.app.clone().oneshot(r.body(Body::from(body)).unwrap()).await.unwrap();
    let s = resp.status();
    (s, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

fn js(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

fn url(name: &str) -> String {
    format!("/v1/packs/base10/files/{name}")
}

#[tokio::test]
async fn auth_required() {
    let t = setup(5);
    assert_eq!(call(&t, "GET", "/v1/packs/base10/files", "", &[], vec![]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&t, "GET", "/v1/packs/base10/files", "nope", &[], vec![]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&t, "GET", "/healthz", "", &[], vec![]).await.0, StatusCode::OK);
}

#[tokio::test]
async fn upload_download_roundtrip_and_conflicts() {
    let t = setup(5);
    let data = vec![7u8; 100_000];
    let new = [("if-none-match", "*"), ("x-machine", "laptop"), ("x-pack-version", "0.40.0")];

    // precondition is mandatory
    assert_eq!(call(&t, "PUT", &url(NAME), "tok-a", &[], data.clone()).await.0, StatusCode::PRECONDITION_REQUIRED);

    let (s, b) = call(&t, "PUT", &url(NAME), "tok-a", &new, data.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(js(&b)["version"], 1);

    // retry of identical upload is idempotent
    let (s, b) = call(&t, "PUT", &url(NAME), "tok-a", &new, data.clone()).await;
    assert_eq!((s, js(&b)["version"].as_i64()), (StatusCode::OK, Some(1)));

    // different content, creating again -> conflict with current head
    let (s, b) = call(&t, "PUT", &url(NAME), "tok-a", &new, vec![1, 2, 3]).await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED);
    assert_eq!(js(&b)["current"]["updated_by"], "laptop");

    // download
    let (s, b) = call(&t, "GET", &url(NAME), "tok-a", &[], vec![]).await;
    assert_eq!((s, b), (StatusCode::OK, data));

    // update with right If-Match, stale If-Match fails
    let (s, b) = call(&t, "PUT", &url(NAME), "tok-a", &[("if-match", "\"1\"")], vec![9, 9]).await;
    assert_eq!((s, js(&b)["version"].as_i64()), (StatusCode::CREATED, Some(2)));
    let (s, _) = call(&t, "PUT", &url(NAME), "tok-a", &[("if-match", "\"1\"")], vec![5]).await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED);

    // users are isolated
    assert_eq!(call(&t, "GET", &url(NAME), "tok-b", &[], vec![]).await.0, StatusCode::NOT_FOUND);

    // manifest
    let (_, b) = call(&t, "GET", "/v1/packs/base10/files", "tok-a", &[], vec![]).await;
    assert_eq!(js(&b)["files"][0]["version"], 2);
}

#[tokio::test]
async fn delete_restore_history() {
    let t = setup(5);
    let put = |v: &'static str, d: &'static [u8]| {
        let h: Vec<(&'static str, &'static str)> =
            if v == "*" { vec![("if-none-match", "*")] } else { vec![("if-match", v)] };
        (h, d.to_vec())
    };
    let (h, d) = put("*", b"one");
    call(&t, "PUT", &url(NAME), "tok-a", &h, d).await;
    let (h, d) = put("1", b"two");
    call(&t, "PUT", &url(NAME), "tok-a", &h, d).await;

    // tombstone
    let (s, b) = call(&t, "DELETE", &url(NAME), "tok-a", &[("if-match", "2")], vec![]).await;
    assert_eq!((s, js(&b)["deleted"].as_bool()), (StatusCode::OK, Some(true)));
    assert_eq!(call(&t, "GET", &url(NAME), "tok-a", &[], vec![]).await.0, StatusCode::NOT_FOUND);
    let (_, b) = call(&t, "GET", "/v1/packs/base10/files", "tok-a", &[], vec![]).await;
    assert_eq!(js(&b)["files"][0]["deleted"], true);

    // restore v1 over the tombstone
    let r = format!("{}/restore/1", url(NAME));
    let (s, b) = call(&t, "POST", &r, "tok-a", &[], vec![]).await;
    assert_eq!((s, js(&b)["version"].as_i64()), (StatusCode::CREATED, Some(4)));
    assert_eq!(call(&t, "GET", &url(NAME), "tok-a", &[], vec![]).await.1, b"one");

    let (_, b) = call(&t, "GET", &format!("{}/history", url(NAME)), "tok-a", &[], vec![]).await;
    assert_eq!(js(&b)["versions"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn history_pruned_and_blobs_collected() {
    let t = setup(2);
    call(&t, "PUT", &url(NAME), "tok-a", &[("if-none-match", "*")], b"v1".to_vec()).await;
    for v in 1..=3 {
        let c = format!("v{}", v + 1).into_bytes();
        call(&t, "PUT", &url(NAME), "tok-a", &[("if-match", &v.to_string())], c).await;
    }
    let (_, b) = call(&t, "GET", &format!("{}/history", url(NAME)), "tok-a", &[], vec![]).await;
    let versions: Vec<i64> =
        js(&b)["versions"].as_array().unwrap().iter().map(|v| v["version"].as_i64().unwrap()).collect();
    assert_eq!(versions, vec![4, 3]);
    // v1 is pruned, so restoring it is gone
    let (s, _) = call(&t, "POST", &format!("{}/restore/1", url(NAME)), "tok-a", &[], vec![]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // blob for v1 was GC'd from disk
    let sha = sha256_hex(b"v1");
    assert!(!t._dir.path().join("blobs").join(&sha[..2]).join(format!("{sha}.zst")).exists());
}

#[tokio::test]
async fn rejects_bad_names() {
    let t = setup(5);
    for bad in ["x.txt", "..%2Fetc.player", ".player", "a%20b.player"] {
        let (s, _) = call(&t, "PUT", &url(bad), "tok-a", &[("if-none-match", "*")], vec![1]).await;
        assert!(s == StatusCode::BAD_REQUEST || s == StatusCode::NOT_FOUND, "{bad}: {s}");
    }
    let (s, _) = call(&t, "GET", "/v1/packs/Bad%20Pack/files", "tok-a", &[], vec![]).await;
    assert_ne!(s, StatusCode::OK);
}

#[tokio::test]
async fn lease_flow() {
    let t = setup(5);
    let l = "/v1/packs/base10/lease";
    let body = |m: &str, f: bool| format!(r#"{{"machine":"{m}","force":{f}}}"#).into_bytes();
    let j = [("content-type", "application/json")];

    assert_eq!(call(&t, "POST", l, "tok-a", &j, body("desktop", false)).await.0, StatusCode::OK);
    // renew by same machine ok
    assert_eq!(call(&t, "POST", l, "tok-a", &j, body("desktop", false)).await.0, StatusCode::OK);
    // other machine blocked, told who
    let (s, b) = call(&t, "POST", l, "tok-a", &j, body("laptop", false)).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(js(&b)["holder"]["machine"], "desktop");
    // another user is unaffected
    assert_eq!(call(&t, "POST", l, "tok-b", &j, body("laptop", false)).await.0, StatusCode::OK);
    // non-holder can't release; holder can
    let (_, b) = call(&t, "DELETE", l, "tok-a", &j, body("laptop", false)).await;
    assert_eq!(js(&b)["released"], false);
    let (_, b) = call(&t, "DELETE", l, "tok-a", &j, body("desktop", false)).await;
    assert_eq!(js(&b)["released"], true);
    assert_eq!(call(&t, "POST", l, "tok-a", &j, body("laptop", false)).await.0, StatusCode::OK);
    // force takeover
    assert_eq!(call(&t, "POST", l, "tok-a", &j, body("desktop", true)).await.0, StatusCode::OK);
}
