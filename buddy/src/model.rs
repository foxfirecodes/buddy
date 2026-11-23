use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

pub const MAX_CHUNK: usize = 4 * 1024 * 1024;
pub fn id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
pub fn when(time: i64) -> String {
    chrono::DateTime::from_timestamp(time, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_default()
}
pub fn key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
pub fn hash_ok(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn component(s: &str) -> Result<()> {
    let device = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    ensure!(
        !s.is_empty()
            && s.len() <= 255
            && !s.ends_with([' ', '.'])
            && !s
                .chars()
                .any(|c| c.is_control() || r#"\/:*?"<>|"#.contains(c))
            && !matches!(
                device.as_str(),
                "AUX" | "PRN" | "NUL" | "CON" | "CONIN$" | "CONOUT$"
            )
            && !["COM", "LPT"]
                .iter()
                .any(|p| device.strip_prefix(p).is_some_and(|n| matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                ))),
        "unsupported portable filename {s:?}"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub root: String,
    pub relative: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    pub kind: Kind,
    #[serde(default = "raw_codec")]
    pub codec: String,
    #[serde(default)]
    pub settings: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub locations: BTreeMap<String, Location>,
}
pub fn raw_codec() -> String {
    "raw-v1".to_owned()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recipe {
    pub game: String,
    pub title: String,
    pub slots: BTreeMap<String, Slot>,
    pub retention: u32,
}
impl Recipe {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            key(&self.game) && !self.title.trim().is_empty() && self.title.len() <= 200,
            "invalid game identity"
        );
        ensure!(
            (1..=100).contains(&self.slots.len()) && self.retention <= 1000,
            "invalid slot count or retention"
        );
        for (name, slot) in &self.slots {
            ensure!(key(name), "invalid slot ID");
            ensure!(
                slot.codec == "raw-v1",
                "unsupported file format {} for {name}",
                slot.codec
            );
            for (runtime, loc) in &slot.locations {
                ensure!(key(runtime) && key(&loc.root), "invalid location");
                if !loc.relative.is_empty() {
                    for part in loc.relative.split('/') {
                        component(part)?;
                    }
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Chunk {
    pub hash: String,
    pub size: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Entry {
    Directory,
    File {
        size: u64,
        hash: String,
        chunks: Vec<Chunk>,
    },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tree(pub BTreeMap<String, Entry>);
impl Tree {
    pub fn project(&self, scope: &Recipe) -> Tree {
        let mut entries = BTreeMap::new();
        for (k, v) in &self.0 {
            if scope.slots.contains_key(k.split('/').next().unwrap_or("")) {
                entries.insert(k.clone(), v.clone());
            }
        }
        // A folder slot added after this snapshot represents an empty directory at that revision.
        for (name, s) in &scope.slots {
            if s.kind == Kind::Folder {
                entries.entry(name.clone()).or_insert(Entry::Directory);
            }
        }
        Tree(entries)
    }
    pub fn overlay(&self, scope: &Recipe, selected: &Tree) -> Tree {
        let mut full = self.clone();
        full.0
            .retain(|k, _| !scope.slots.contains_key(k.split('/').next().unwrap_or("")));
        full.0.extend(selected.0.clone());
        full
    }
    pub fn changes(&self, base: Option<&Tree>) -> String {
        let Some(base) = base else {
            return "First comparison on this device".into();
        };
        let added = self.0.keys().filter(|k| !base.0.contains_key(*k)).count();
        let edited = self
            .0
            .iter()
            .filter(|(k, v)| base.0.get(*k).is_some_and(|b| b != *v))
            .count();
        let removed = base.0.keys().filter(|k| !self.0.contains_key(*k)).count();
        format!("{added} added · {edited} changed · {removed} removed")
    }
    pub fn validate(&self, recipe: &Recipe) -> Result<()> {
        let mut folded = BTreeSet::new();
        for (name, entry) in &self.0 {
            ensure!(name.len() <= 4096, "path too long");
            let mut parts = name.split('/');
            let slot = recipe
                .slots
                .get(parts.next().unwrap_or(""))
                .ok_or_else(|| anyhow::anyhow!("unknown slot in {name}"))?;
            for part in parts {
                component(part)?;
            }
            if let Some((parent, _)) = name.rsplit_once('/') {
                ensure!(
                    self.0.get(parent) == Some(&Entry::Directory),
                    "missing parent directory for {name}"
                );
            } else {
                ensure!(
                    matches!(
                        (&slot.kind, entry),
                        (Kind::File, Entry::File { .. }) | (Kind::Folder, Entry::Directory)
                    ),
                    "wrong kind for {name}"
                );
            }
            ensure!(
                folded.insert(name.nfc().collect::<String>().to_lowercase()),
                "case or Unicode filename collision at {name}"
            );
            if let Entry::File { size, hash, chunks } = entry {
                ensure!(hash_ok(hash), "invalid file digest");
                let mut total = 0u64;
                for c in chunks {
                    ensure!(
                        c.size > 0 && c.size as usize <= MAX_CHUNK && hash_ok(&c.hash),
                        "invalid chunk"
                    );
                    total = total
                        .checked_add(c.size as u64)
                        .ok_or_else(|| anyhow::anyhow!("invalid file size"))?;
                }
                ensure!(total == *size, "invalid chunk lengths");
            }
        }
        Ok(())
    }
    pub fn chunks(&self) -> Result<BTreeMap<String, u32>> {
        let mut result = BTreeMap::new();
        for entry in self.0.values() {
            if let Entry::File { chunks, .. } = entry {
                for c in chunks {
                    if result
                        .insert(c.hash.clone(), c.size)
                        .is_some_and(|v| v != c.size)
                    {
                        bail!("inconsistent chunk lengths");
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn populated(&self) -> bool {
        self.0
            .iter()
            .any(|(p, e)| p.contains('/') || matches!(e, Entry::File { .. }))
    }
    pub fn summary(&self) -> String {
        let (mut count, mut bytes) = (0u64, 0u64);
        for entry in self.0.values() {
            if let Entry::File { size, .. } = entry {
                count += 1;
                bytes += size;
            }
        }
        format!(
            "{count} {}, {bytes} {}",
            if count == 1 { "file" } else { "files" },
            if bytes == 1 { "byte" } else { "bytes" }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Revision {
    pub id: i64,
    pub origin: String,
    pub time: i64,
    pub tree: Tree,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Held {
    pub generation: i64,
    pub device: String,
    pub since: i64,
    pub expires: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub game: String,
    pub generation: i64,
    pub session: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Acquisition {
    Busy(Held),
    Granted {
        lease: Lease,
        head: Option<Revision>,
        recipe: Recipe,
        revision: i64,
        shared: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub url: String,
    pub account: String,
    pub device: String,
    pub token: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Login {
    pub account: String,
    pub password: String,
    pub device: String,
    pub invite: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Backup {
    pub id: i64,
    pub origin: String,
    pub time: i64,
    pub reason: String,
    pub head: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameInfo {
    pub recipe: Recipe,
    pub revision: i64,
    pub shared: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Games,
    Catalog,
    SaveRecipe {
        recipe: Recipe,
        expected: i64,
        shared: bool,
    },
    Acquire {
        game: String,
        session: String,
        takeover: Option<i64>,
    },
    Renew {
        lease: Lease,
    },
    Release {
        lease: Lease,
    },
    Missing {
        hashes: Vec<String>,
    },
    Commit {
        lease: Lease,
        expected: Option<i64>,
        tree: Tree,
        close: bool,
        op: String,
    },
    Conflict {
        lease: Lease,
        expected: i64,
        local: Tree,
        time: i64,
        op: String,
    },
    Preserve {
        game: String,
        slots: Vec<String>,
        local: Tree,
        time: i64,
        op: String,
    },
    Backups {
        game: String,
    },
    Pin {
        game: String,
        id: i64,
    },
    UnpinCurrent {
        game: String,
        id: i64,
    },
    Delete {
        game: String,
        id: i64,
    },
    ReadRevision {
        game: String,
        id: i64,
    },
    Devices,
    Revoke {
        device: String,
    },
}
impl Action {
    /// The server stores the result for these writes in the same transaction as the write.
    pub fn operation_id(&self) -> Option<&str> {
        match self {
            Self::Commit { op, .. } | Self::Conflict { op, .. } | Self::Preserve { op, .. } => {
                Some(op)
            }
            _ => None,
        }
    }
}
