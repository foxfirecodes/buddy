//! Large-file scan hints. Content is still verified before transfer and restore.
use crate::{files::Cache, model::Entry};
use anyhow::Result;
use cap_std::fs::{Dir, Metadata};
use std::path::Path;

pub struct Volume {
    #[cfg(target_os = "linux")]
    device: Option<u64>,
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
    pub fn stamp(&self, m: &Metadata) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            use cap_std::fs::MetadataExt;
            if Some(m.dev()) != self.device || m.len() < 1024 * 1024 {
                return None;
            }
            // A coarse timestamp, clock rollback or recent write is re-read. A caller cannot
            // restore ctime with utimensat when replacing bytes and preserving mtime.
            if m.ctime_nsec() == 0 || m.ctime() >= crate::model::now() - 2 {
                return None;
            }
            Some(format!(
                "{}:{}:{}:{}:{}:{}:{}",
                m.dev(),
                m.ino(),
                m.len(),
                m.ctime(),
                m.ctime_nsec(),
                m.mtime(),
                m.mtime_nsec()
            ))
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
    pub fn lookup(&self, path: &Path, stamp: &str, cache: &Cache) -> Result<Option<Entry>> {
        use rusqlite::OptionalExtension;
        if self.boot.is_empty() {
            return Ok(None);
        }
        let entry: Option<String> = self
            .db
            .query_row(
                "SELECT entry FROM files WHERE path=? AND boot=? AND stamp=? AND observed <= ?",
                rusqlite::params![
                    path.to_string_lossy(),
                    self.boot,
                    stamp,
                    crate::model::now()
                ],
                |r| r.get(0),
            )
            .optional()?;
        let Some(entry) = entry else { return Ok(None) };
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
        Ok(Some(entry))
    }
    pub fn remember(&self, path: &Path, stamp: &str, entry: &Entry) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO files VALUES(?,?,?,?,?)",
            rusqlite::params![
                path.to_string_lossy(),
                self.boot,
                stamp,
                crate::model::now(),
                serde_json::to_string(entry)?
            ],
        )?;
        Ok(())
    }
}
