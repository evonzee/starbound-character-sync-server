use std::{fs, path::PathBuf, sync::Arc};

use clap::{Parser, Subcommand};
use starbound_sync_server::{AppState, Config, api, blobs::BlobStore, db::Db, new_token, sha256_hex};

#[derive(Parser)]
#[command(about = "Player-save sync server for the Starbound modpack launcher")]
struct Cli {
    /// Directory holding the SQLite database and blobs.
    #[arg(long, env = "SYNC_DATA_DIR", default_value = "./data", global = true)]
    data_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server.
    Serve {
        #[arg(long, env = "SYNC_LISTEN", default_value = "0.0.0.0:8080")]
        listen: String,
        /// Versions kept per file.
        #[arg(long, env = "SYNC_HISTORY", default_value_t = 20)]
        history: i64,
        #[arg(long, env = "SYNC_MAX_UPLOAD_MB", default_value_t = 32)]
        max_upload_mb: usize,
        /// Seconds an advisory "playing on" lease lasts without renewal.
        #[arg(long, env = "SYNC_LEASE_TTL", default_value_t = 600)]
        lease_ttl: i64,
    },
    /// Create a user and print their bearer token (shown once).
    AddUser { name: String },
    /// Issue a new token for an existing user (invalidates the old one).
    ResetToken { name: String },
    /// Revoke a user's token.
    RevokeUser { name: String },
    ListUsers,
}

fn open_db(dir: &PathBuf) -> Db {
    fs::create_dir_all(dir).expect("create data dir");
    Db::open(&dir.join("sync.db")).expect("open database")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let cli = Cli::parse();
    let db = open_db(&cli.data_dir);
    match cli.cmd {
        Cmd::AddUser { name } => {
            let t = new_token();
            db.create_user(&name, &sha256_hex(t.as_bytes())).expect("create user (name taken?)");
            println!("{t}");
        }
        Cmd::ResetToken { name } => {
            let t = new_token();
            if db.rotate_token(&name, &sha256_hex(t.as_bytes())).unwrap() {
                println!("{t}");
            } else {
                eprintln!("no such user");
                std::process::exit(1);
            }
        }
        Cmd::RevokeUser { name } => {
            if !db.revoke_user(&name).unwrap() {
                eprintln!("no such user");
                std::process::exit(1);
            }
        }
        Cmd::ListUsers => {
            for (n, at, revoked) in db.list_users().unwrap() {
                println!("{n}\tcreated={at}{}", if revoked { "\trevoked" } else { "" });
            }
        }
        Cmd::Serve { listen, history, max_upload_mb, lease_ttl } => {
            let state = Arc::new(AppState {
                db,
                blobs: BlobStore::new(cli.data_dir.clone()).expect("blob dir"),
                cfg: Config {
                    history,
                    max_upload_bytes: max_upload_mb << 20,
                    lease_ttl_secs: lease_ttl,
                },
                write_lock: Default::default(),
            });
            let l = tokio::net::TcpListener::bind(&listen).await.expect("bind");
            tracing::info!("listening on {listen}, data in {}", cli.data_dir.display());
            axum::serve(l, api::router(state))
                .with_graceful_shutdown(shutdown())
                .await
                .unwrap();
        }
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("sigterm handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
