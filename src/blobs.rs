//! Content-addressed blob store: `<root>/blobs/<aa>/<sha256>.zst`.
use std::{fs, io, path::PathBuf};

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    fn path(&self, sha: &str) -> PathBuf {
        self.root.join("blobs").join(&sha[..2]).join(format!("{sha}.zst"))
    }

    /// Compress and store `data` under `sha` (idempotent, atomic).
    pub fn put(&self, sha: &str, data: &[u8]) -> io::Result<()> {
        let dest = self.path(sha);
        if dest.exists() {
            return Ok(());
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = self.root.join("tmp").join(format!("{sha}.{}", std::process::id()));
        fs::write(&tmp, zstd::encode_all(data, 3)?)?;
        fs::rename(&tmp, &dest)
    }

    pub fn get(&self, sha: &str) -> io::Result<Vec<u8>> {
        zstd::decode_all(fs::File::open(self.path(sha))?)
    }

    pub fn delete(&self, sha: &str) {
        let _ = fs::remove_file(self.path(sha));
    }
}
