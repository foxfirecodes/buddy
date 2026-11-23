//! Large-file scan hints. Content is still verified before transfer and restore.
use crate::{files::Cache, model::Entry};
use anyhow::Result;
use cap_std::fs::{Dir, Metadata};
use std::path::Path;

pub struct Volume {
    #[cfg(target_os = "linux")]
    device: Option<u64>,
}
pub struct Stamp {
    value: String,
    changed: i64,
    observed: i64,
}
pub struct Hint {
    pub entry: Entry,
    pub trusted: bool,
}
impl Volume {
    pub fn new(dir: &Dir) -> Self {
        #[cfg(target_os = "linux")]
        {
            use cap_std::fs::MetadataExt;
            // Linux UAPI superblock magic numbers for ext-family, Btrfs and XFS.
            let supported = rustix::fs::fstatfs(dir)
                .is_ok_and(|s| matches!(s.f_type as u64, 0xef53 | 0x9123683e | 0x58465342));
            Self {
                device: supported
                    .then(|| dir.dir_metadata().ok().map(|m| m.dev()))
                    .flatten(),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = dir;
            Self {}
        }
    }
    pub fn stamp(&self, m: &Metadata) -> Option<Stamp> {
        #[cfg(target_os = "linux")]
        {
            use cap_std::fs::MetadataExt;
            if Some(m.dev()) != self.device || m.len() < 1024 * 1024 {
                return None;
            }
            let observed = crate::model::now();
            if m.ctime_nsec() == 0 || m.ctime() > observed {
                return None;
            }
            Some(Stamp {
                value: format!(
                    "{}:{}:{}:{}:{}:{}:{}",
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.ctime(),
                    m.ctime_nsec(),
                    m.mtime(),
                    m.mtime_nsec()
                ),
                changed: m.ctime(),
                observed,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = m;
            None
        }
    }
}
pub struct Index {
    db: rusqlite::Connection,
    boot: String,
}
impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let db = rusqlite::Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,boot TEXT,stamp TEXT,observed INTEGER,entry TEXT);")?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
        Ok(Self { db, boot })
    }
    pub fn lookup(&self, path: &Path, stamp: &Stamp, cache: &Cache) -> Result<Option<Hint>> {
        use rusqlite::OptionalExtension;
        if self.boot.is_empty() {
            return Ok(None);
        }
        let row: Option<(String, i64)> = self
            .db
            .query_row(
                "SELECT entry,observed FROM files WHERE path=? AND boot=? AND stamp=? AND observed <= ?",
                rusqlite::params![
                    path.to_string_lossy(),
                    self.boot,
                    stamp.value,
                    stamp.observed
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((entry, observed)) = row else {
            return Ok(None);
        };
        let entry: Entry = serde_json::from_str(&entry)?;
        if let Entry::File { chunks, .. } = &entry {
            let mut seen = std::collections::BTreeSet::new();
            for c in chunks {
                if seen.insert(&c.hash)
                    && !cache
                        .0
                        .join(&c.hash)
                        .metadata()
                        .is_ok_and(|m| m.len() == c.size as u64)
                {
                    return Ok(None);
                }
            }
        }
        // Bytes read too close to ctime can have changed in the same filesystem
        // clock tick, even if the file has since aged. Confirm the digest once
        // after the stamp matures before accepting metadata alone.
        Ok(Some(Hint {
            entry,
            trusted: stamp.changed < observed.saturating_sub(2),
        }))
    }
    pub fn remember(&self, path: &Path, stamp: &Stamp, entry: &Entry) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO files VALUES(?,?,?,?,?)",
            rusqlite::params![
                path.to_string_lossy(),
                self.boot,
                stamp.value,
                stamp.observed,
                serde_json::to_string(entry)?
            ],
        )?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::model::{Chunk, digest};
    #[test]
    fn a_recent_observation_needs_digest_confirmation_even_after_aging() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let cache = Cache::open(tmp.path().to_owned())?;
        let index = Index::open(&tmp.path().join("scan.sqlite"))?;
        let path = tmp.path().join("world");
        let mut stamp = Stamp {
            value: "identity".into(),
            changed: 100,
            observed: 100,
        };
        let hash = digest(b"x");
        cache.put(&hash, b"x")?;
        let entry = Entry::File {
            size: 1,
            hash: hash.clone(),
            chunks: vec![Chunk {
                size: 1,
                hash: hash.clone(),
            }],
        };
        index.remember(&path, &stamp, &entry)?;
        assert!(!index.lookup(&path, &stamp, &cache)?.unwrap().trusted);
        stamp.observed += 10;
        assert!(!index.lookup(&path, &stamp, &cache)?.unwrap().trusted);
        index.remember(&path, &stamp, &entry)?;
        assert!(index.lookup(&path, &stamp, &cache)?.unwrap().trusted);
        std::fs::remove_file(tmp.path().join(hash))?;
        assert!(index.lookup(&path, &stamp, &cache)?.is_none());
        Ok(())
    }
}
