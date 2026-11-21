//! SQLite metadata. Every file is an append-only list of versions; the highest
//! version is the "head". Deletes are tombstone versions (`deleted = 1`).
use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileMeta {
    pub name: String,
    pub version: i64,
    /// sha256 of the uncompressed content; empty for tombstones.
    pub sha256: String,
    pub size: i64,
    pub pack_version: Option<String>,
    /// Machine that uploaded this version.
    pub updated_by: String,
    pub updated_at: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lease {
    pub machine: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
}

pub enum Precondition {
    /// File must not exist yet (`If-None-Match: *`).
    Absent,
    /// Head must be exactly this version (`If-Match: "<n>"`).
    Version(i64),
    /// No check (server-initiated actions such as restore).
    Any,
}

pub struct NewVersion<'a> {
    pub sha256: &'a str,
    pub size: i64,
    pub pack_version: Option<&'a str>,
    pub machine: &'a str,
    pub deleted: bool,
}

pub enum Appended {
    /// New head created. `orphans` are blob hashes no longer referenced.
    Created { meta: FileMeta, orphans: Vec<String> },
    /// Request was already satisfied by the current head (idempotent retry).
    Unchanged(FileMeta),
    Conflict(Option<FileMeta>),
}

pub enum LeaseOutcome {
    Granted(Lease),
    Held(Lease),
}

pub struct Db {
    conn: Mutex<Connection>,
}

const COLS: &str = "name, version, sha256, size, pack_version, machine, created_at, deleted";

fn row_meta(r: &Row) -> rusqlite::Result<FileMeta> {
    Ok(FileMeta {
        name: r.get(0)?,
        version: r.get(1)?,
        sha256: r.get(2)?,
        size: r.get(3)?,
        pack_version: r.get(4)?,
        updated_by: r.get(5)?,
        updated_at: r.get(6)?,
        deleted: r.get::<_, i64>(7)? != 0,
    })
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::from_conn(Connection::open(path)?)
    }

    pub fn from_conn(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS users (
               id INTEGER PRIMARY KEY,
               name TEXT NOT NULL UNIQUE,
               token_hash TEXT NOT NULL UNIQUE,
               created_at INTEGER NOT NULL,
               revoked INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS versions (
               user_id INTEGER NOT NULL REFERENCES users(id),
               pack TEXT NOT NULL,
               name TEXT NOT NULL,
               version INTEGER NOT NULL,
               sha256 TEXT NOT NULL,
               size INTEGER NOT NULL,
               pack_version TEXT,
               machine TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               deleted INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (user_id, pack, name, version)
             );
             CREATE INDEX IF NOT EXISTS versions_sha ON versions(sha256);
             CREATE TABLE IF NOT EXISTS leases (
               user_id INTEGER NOT NULL REFERENCES users(id),
               pack TEXT NOT NULL,
               machine TEXT NOT NULL,
               expires_at INTEGER NOT NULL,
               PRIMARY KEY (user_id, pack)
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- users ----

    pub fn create_user(&self, name: &str, token_hash: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO users (name, token_hash, created_at) VALUES (?1, ?2, ?3)",
            params![name, token_hash, now()],
        )?;
        Ok(())
    }

    pub fn user_by_token(&self, token_hash: &str) -> rusqlite::Result<Option<User>> {
        self.conn()
            .query_row(
                "SELECT id, name FROM users WHERE token_hash = ?1 AND revoked = 0",
                [token_hash],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)? }),
            )
            .optional()
    }

    /// (name, created_at, revoked)
    pub fn list_users(&self) -> rusqlite::Result<Vec<(String, i64, bool)>> {
        let c = self.conn();
        let mut s = c.prepare("SELECT name, created_at, revoked FROM users ORDER BY id")?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?
            .collect()
    }

    pub fn revoke_user(&self, name: &str) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("UPDATE users SET revoked = 1 WHERE name = ?1", [name])? > 0)
    }

    /// Replace a user's token (also un-revokes).
    pub fn rotate_token(&self, name: &str, token_hash: &str) -> rusqlite::Result<bool> {
        Ok(self.conn().execute(
            "UPDATE users SET token_hash = ?2, revoked = 0 WHERE name = ?1",
            params![name, token_hash],
        )? > 0)
    }

    // ---- files ----

    pub fn packs(&self, user: i64) -> rusqlite::Result<Vec<String>> {
        let c = self.conn();
        let mut s = c.prepare("SELECT DISTINCT pack FROM versions WHERE user_id = ?1 ORDER BY pack")?;
        s.query_map([user], |r| r.get(0))?.collect()
    }

    pub fn manifest(&self, user: i64, pack: &str) -> rusqlite::Result<Vec<FileMeta>> {
        let c = self.conn();
        let mut s = c.prepare(&format!(
            "SELECT {COLS} FROM versions v
             WHERE user_id = ?1 AND pack = ?2
               AND version = (SELECT MAX(version) FROM versions
                              WHERE user_id = v.user_id AND pack = v.pack AND name = v.name)
             ORDER BY name"
        ))?;
        s.query_map(params![user, pack], row_meta)?.collect()
    }

    pub fn head(&self, user: i64, pack: &str, name: &str) -> rusqlite::Result<Option<FileMeta>> {
        head_in(&self.conn(), user, pack, name)
    }

    pub fn version(
        &self,
        user: i64,
        pack: &str,
        name: &str,
        version: i64,
    ) -> rusqlite::Result<Option<FileMeta>> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {COLS} FROM versions
                     WHERE user_id = ?1 AND pack = ?2 AND name = ?3 AND version = ?4"
                ),
                params![user, pack, name, version],
                row_meta,
            )
            .optional()
    }

    pub fn history(&self, user: i64, pack: &str, name: &str) -> rusqlite::Result<Vec<FileMeta>> {
        let c = self.conn();
        let mut s = c.prepare(&format!(
            "SELECT {COLS} FROM versions
             WHERE user_id = ?1 AND pack = ?2 AND name = ?3 ORDER BY version DESC"
        ))?;
        s.query_map(params![user, pack, name], row_meta)?.collect()
    }

    /// Atomically append a new version if `pre` holds, then prune history to
    /// the newest `keep` versions.
    pub fn append(
        &self,
        user: i64,
        pack: &str,
        name: &str,
        pre: Precondition,
        new: &NewVersion,
        keep: i64,
    ) -> rusqlite::Result<Appended> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head = head_in(&tx, user, pack, name)?;

        // Idempotent retry: head already is what the caller wants.
        if let Some(h) = &head {
            let same = if new.deleted { h.deleted } else { !h.deleted && h.sha256 == new.sha256 };
            if same {
                return Ok(Appended::Unchanged(h.clone()));
            }
        }
        let ok = match (&pre, &head) {
            (Precondition::Any, _) => true,
            (Precondition::Absent, None) => true,
            (Precondition::Version(v), Some(h)) => h.version == *v,
            _ => false,
        };
        if !ok {
            return Ok(Appended::Conflict(head));
        }

        let version = head.as_ref().map_or(1, |h| h.version + 1);
        let at = now();
        tx.execute(
            "INSERT INTO versions
             (user_id, pack, name, version, sha256, size, pack_version, machine, created_at, deleted)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                user, pack, name, version, new.sha256, new.size, new.pack_version,
                new.machine, at, new.deleted as i64
            ],
        )?;

        // Prune old versions, collecting blobs that become unreferenced.
        let cutoff = version - keep;
        let mut orphans = Vec::new();
        if cutoff > 0 {
            let doomed: Vec<String> = {
                let mut s = tx.prepare(
                    "SELECT DISTINCT sha256 FROM versions
                     WHERE user_id=?1 AND pack=?2 AND name=?3 AND version<=?4 AND sha256 != ''",
                )?;
                s.query_map(params![user, pack, name, cutoff], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            tx.execute(
                "DELETE FROM versions WHERE user_id=?1 AND pack=?2 AND name=?3 AND version<=?4",
                params![user, pack, name, cutoff],
            )?;
            for sha in doomed {
                let n: i64 =
                    tx.query_row("SELECT COUNT(*) FROM versions WHERE sha256=?1", [&sha], |r| r.get(0))?;
                if n == 0 {
                    orphans.push(sha);
                }
            }
        }
        tx.commit()?;
        Ok(Appended::Created {
            meta: FileMeta {
                name: name.into(),
                version,
                sha256: new.sha256.into(),
                size: new.size,
                pack_version: new.pack_version.map(Into::into),
                updated_by: new.machine.into(),
                updated_at: at,
                deleted: new.deleted,
            },
            orphans,
        })
    }

    // ---- leases (advisory "currently playing on <machine>") ----

    pub fn lease(&self, user: i64, pack: &str) -> rusqlite::Result<Option<Lease>> {
        lease_in(&self.conn(), user, pack)
    }

    pub fn acquire_lease(
        &self,
        user: i64,
        pack: &str,
        machine: &str,
        ttl: i64,
        force: bool,
    ) -> rusqlite::Result<LeaseOutcome> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(l) = lease_in(&tx, user, pack)? {
            if l.machine != machine && !force {
                return Ok(LeaseOutcome::Held(l));
            }
        }
        let l = Lease { machine: machine.into(), expires_at: now() + ttl };
        tx.execute(
            "INSERT INTO leases (user_id, pack, machine, expires_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(user_id, pack) DO UPDATE SET machine=?3, expires_at=?4",
            params![user, pack, l.machine, l.expires_at],
        )?;
        tx.commit()?;
        Ok(LeaseOutcome::Granted(l))
    }

    /// Release only if `machine` holds it. Returns whether anything was released.
    pub fn release_lease(&self, user: i64, pack: &str, machine: &str) -> rusqlite::Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM leases WHERE user_id=?1 AND pack=?2 AND machine=?3",
            params![user, pack, machine],
        )? > 0)
    }

    /// Is `sha` still referenced by any version? (used before deleting blobs)
    pub fn blob_referenced(&self, sha: &str) -> rusqlite::Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE sha256=?1)",
            [sha],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }
}

fn head_in(c: &Connection, user: i64, pack: &str, name: &str) -> rusqlite::Result<Option<FileMeta>> {
    c.query_row(
        &format!(
            "SELECT {COLS} FROM versions
             WHERE user_id=?1 AND pack=?2 AND name=?3 ORDER BY version DESC LIMIT 1"
        ),
        params![user, pack, name],
        row_meta,
    )
    .optional()
}

fn lease_in(c: &Connection, user: i64, pack: &str) -> rusqlite::Result<Option<Lease>> {
    c.query_row(
        "SELECT machine, expires_at FROM leases WHERE user_id=?1 AND pack=?2 AND expires_at > ?3",
        params![user, pack, now()],
        |r| Ok(Lease { machine: r.get(0)?, expires_at: r.get(1)? }),
    )
    .optional()
}
