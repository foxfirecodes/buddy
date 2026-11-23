use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use fastcdc::v2020::StreamCDC;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Binding {
    pub path: PathBuf,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    #[serde(default)]
    identity: Option<(u64, u64)>,
}
#[cfg(windows)]
fn identity(dir: &Dir) -> Result<(u64, u64)> {
    let info = winapi_util::file::information(dir.try_clone()?.into_std_file())?;
    ensure!(
        info.file_index() != 0,
        "directory has no stable identity on this volume"
    );
    Ok((info.volume_serial_number(), info.file_index()))
}
fn anchor(path: &Path, kind: &Kind) -> Result<PathBuf> {
    Ok(match kind {
        Kind::Folder => path.to_owned(),
        Kind::File => path
            .parent()
            .context("file needs a parent directory")?
            .to_owned(),
    })
}
impl Binding {
    pub fn for_selection(path: &Path, kind: &Kind, reviewed: Option<&Self>) -> Result<Self> {
        let actual = Self::new(path, kind)?;
        ensure!(
            reviewed.is_none_or(|prior| prior == &actual),
            "selected root changed after preview; review the files again"
        );
        Ok(actual)
    }
    pub fn new(path: &Path, kind: &Kind) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let root = anchor(&path, kind)?
            .canonicalize()
            .with_context(|| format!("unavailable root {}", path.display()))?;
        ensure!(root.is_dir(), "choose an existing root directory");
        let path = match kind {
            Kind::Folder => root.clone(),
            Kind::File => root.join(path.file_name().context("missing filename")?),
        };
        if *kind == Kind::File
            && let Ok(metadata) = fs::symlink_metadata(&path)
        {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "select a regular file"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = fs::metadata(root)?;
            Ok(Self {
                path,
                dev: m.dev(),
                ino: m.ino(),
            })
        }
        #[cfg(windows)]
        {
            let dir = Dir::open_ambient_dir(root, ambient_authority())?;
            Ok(Self {
                path,
                identity: Some(identity(&dir)?),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self { path })
        }
    }
}
pub struct Cache(pub PathBuf, crate::index::Index);
impl Cache {
    pub fn open(path: PathBuf) -> Result<Self> {
        let index = crate::index::Index::open(&path.join("scan.sqlite"))?;
        Ok(Self(path, index))
    }
}
impl Cache {
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        ensure!(hash_ok(hash), "invalid cache key");
        let mut bytes = Vec::new();
        fs::File::open(self.0.join(hash))
            .with_context(|| format!("read chunk {hash}"))?
            .take(MAX_CHUNK as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_CHUNK && digest(&bytes) == hash,
            "corrupt cached chunk {hash}"
        );
        Ok(bytes)
    }
    pub fn put(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        ensure!(hash_ok(hash) && digest(bytes) == hash, "invalid chunk");
        let path = self.0.join(hash);
        if path.is_file() && self.get(hash).is_ok() {
            return Ok(());
        }
        let mut temp = tempfile::NamedTempFile::new_in(&self.0)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&path)?;
        sync_dir(&self.0)?;
        Ok(())
    }
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
struct Root {
    volume: crate::index::Volume,
    dir: Dir,
    base: PathBuf,
    kind: Kind,
    anchor: PathBuf,
}
impl Root {
    fn walk(
        &self,
        slot: &str,
        stage: Option<&str>,
        mut visit: impl FnMut(String, PathBuf, cap_std::fs::Metadata) -> Result<()>,
    ) -> Result<()> {
        let mut todo = vec![(slot.to_owned(), self.base.clone())];
        while let Some((logical, path)) = todo.pop() {
            let meta = match self.dir.symlink_metadata(&path) {
                Ok(m) => m,
                Err(e)
                    if e.kind() == ErrorKind::NotFound
                        && logical == slot
                        && self.kind == Kind::File =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            ensure!(
                !meta.file_type().is_symlink(),
                "symlink in selection: {logical}"
            );
            #[cfg(windows)]
            {
                use cap_std::fs::MetadataExt;
                ensure!(
                    meta.file_attributes() & 0x400 == 0,
                    "reparse point in selection: {logical}"
                );
            }
            if meta.is_dir() {
                let names = dir_names(&self.dir, &path)?;
                for name in &names {
                    if path == self.base && stage == Some(name) {
                        continue;
                    }
                    component(name)?;
                    todo.push((format!("{logical}/{name}"), path.join(name)));
                }
            } else {
                ensure!(meta.is_file(), "special file in selection: {logical}");
            }
            visit(logical, path, meta)?;
        }
        Ok(())
    }
}

pub struct Preview {
    pub binding: Binding,
    pub files: Vec<String>,
    pub bytes: u64,
}
pub fn preview(
    home: &Path,
    path: &Path,
    kind: Kind,
    slot: &str,
    cancelled: impl Fn() -> bool,
) -> Result<Preview> {
    let binding = Binding::new(path, &kind)?;
    let label = binding
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let recipe = Recipe {
        game: "preview".into(),
        title: "Selection".into(),
        retention: 0,
        slots: BTreeMap::from([(
            slot.to_owned(),
            Slot {
                kind,
                codec: raw_codec(),
                source: None,
                settings: false,
                locations: BTreeMap::new(),
            },
        )]),
    };
    let files = Files::open(
        &recipe,
        &BTreeMap::from([(slot.to_owned(), binding.clone())]),
        home,
    )?;
    let mut result = Preview {
        binding,
        files: vec![],
        bytes: 0,
    };
    let mut names = BTreeSet::new();
    files.roots[slot].walk(slot, None, |logical, _, meta| {
        ensure!(!cancelled(), "preview cancelled");
        portable_path(&logical, &mut names)?;
        if meta.is_file() {
            result.bytes = result
                .bytes
                .checked_add(meta.len())
                .context("selection size overflow")?;
            result.files.push(
                logical
                    .split_once('/')
                    .map_or(label.as_str(), |(_, name)| name)
                    .to_owned(),
            );
        }
        Ok(())
    })?;
    result.files.sort();
    Ok(result)
}
fn same_file_state(a: &cap_std::fs::Metadata, b: &cap_std::fs::Metadata) -> Result<bool> {
    if !a.is_file() || !b.is_file() || a.len() != b.len() || a.modified()? != b.modified()? {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        Ok((a.dev(), a.ino(), a.ctime(), a.ctime_nsec())
            == (b.dev(), b.ino(), b.ctime(), b.ctime_nsec()))
    }
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        Ok(a.creation_time() == b.creation_time() && a.file_attributes() == b.file_attributes())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(true)
    }
}
pub struct Files {
    roots: BTreeMap<String, Root>,
}
impl Files {
    pub fn open(
        recipe: &Recipe,
        bindings: &BTreeMap<String, Binding>,
        home: &Path,
    ) -> Result<Self> {
        recipe.validate()?;
        let mut roots = BTreeMap::new();
        let mut paths: Vec<&Path> = vec![];
        for (name, slot) in &recipe.slots {
            let b = bindings
                .get(name)
                .with_context(|| format!("bind slot {name} on this device"))?;
            let parent = anchor(&b.path, &slot.kind)?;
            let canonical = parent
                .canonicalize()
                .with_context(|| format!("root unavailable: {}", parent.display()))?;
            ensure!(
                parent == canonical,
                "root changed; review binding {}",
                parent.display()
            );
            ensure!(
                !home.starts_with(&b.path) && !b.path.starts_with(home),
                "save selection overlaps Buddy's own data"
            );
            ensure!(
                !paths
                    .iter()
                    .any(|p| p.starts_with(&b.path) || b.path.starts_with(p)),
                "overlapping slots at {}",
                b.path.display()
            );
            paths.push(&b.path);
            let dir = Dir::open_ambient_dir(&parent, ambient_authority())?;
            #[cfg(windows)]
            ensure!(
                Some(identity(&dir)?) == b.identity,
                "drive or root changed; review {}",
                parent.display()
            );
            #[cfg(unix)]
            {
                use cap_std::fs::MetadataExt;
                let m = dir.dir_metadata()?;
                ensure!(
                    m.dev() == b.dev && m.ino() == b.ino,
                    "drive or root changed; review {}",
                    parent.display()
                );
            }
            let base = if slot.kind == Kind::Folder {
                PathBuf::from(".")
            } else {
                PathBuf::from(b.path.file_name().context("missing filename")?)
            };
            roots.insert(
                name.clone(),
                Root {
                    volume: crate::index::Volume::new(&dir),
                    dir,
                    base,
                    kind: slot.kind.clone(),
                    anchor: parent,
                },
            );
        }
        Ok(Self { roots })
    }
    fn where_is(&self, key: &str) -> Result<(&Root, PathBuf)> {
        let (slot, rest) = key.split_once('/').unwrap_or((key, ""));
        let root = self.roots.get(slot).context("unknown slot")?;
        let mut rel = root.base.clone();
        if !rest.is_empty() {
            for part in rest.split('/') {
                component(part)?;
                rel.push(part);
            }
        }
        Ok((root, rel))
    }
    pub fn capture(&self, recipe: &Recipe, cache: &Cache, stage: Option<&str>) -> Result<Tree> {
        self.scan(recipe, cache, stage, stage.is_none())
    }
    pub fn scan(
        &self,
        recipe: &Recipe,
        cache: &Cache,
        stage: Option<&str>,
        allow_hints: bool,
    ) -> Result<Tree> {
        let mut tree = Tree::default();
        let mut stamps = BTreeMap::new();
        for (slot, root) in &self.roots {
            root.walk(slot, stage, |logical, path, meta| {
                let entry = if meta.is_dir() {
                    Entry::Directory
                } else {
                    let stamp = root.volume.stamp(&meta);
                    let absolute = root.anchor.join(&path);
                    let old = if allow_hints {
                        match &stamp {
                            Some(s) => cache.1.lookup(&absolute, s, cache)?,
                            None => None,
                        }
                    } else {
                        None
                    };
                    let entry = match old {
                        Some(entry) => entry,
                        None => {
                            let file = root.dir.open(&path)?;
                            ensure!(
                                same_file_state(&meta, &file.metadata()?)?,
                                "file replaced during capture: {logical}"
                            );
                            let entry = capture_file(file, cache, meta.len())?;
                            ensure!(
                                same_file_state(&meta, &root.dir.symlink_metadata(&path)?)?,
                                "file changed during capture: {logical}"
                            );
                            if let Some(stamp) = &stamp {
                                cache.1.remember(&absolute, stamp, &entry)?;
                            }
                            entry
                        }
                    };
                    stamps.insert(logical.clone(), meta);
                    entry
                };
                tree.0.insert(logical, entry);
                Ok(())
            })?;
        }
        tree.validate(recipe)?;
        // Detect additions, removals and edits that occurred elsewhere in the tree during the read.
        for (path, entry) in &tree.0 {
            let (root, rel) = self.where_is(path)?;
            if let Some(initial) = stamps.get(path) {
                ensure!(
                    same_file_state(initial, &root.dir.symlink_metadata(&rel)?)?,
                    "file changed during capture: {path}"
                );
            }
            if *entry == Entry::Directory {
                let actual = dir_names(&root.dir, &rel)?
                    .into_iter()
                    .filter(|s| rel != root.base || Some(s.as_str()) != stage)
                    .collect::<BTreeSet<_>>();
                let prefix = format!("{path}/");
                let expected = tree
                    .0
                    .range(prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&prefix))
                    .filter_map(|(k, _)| {
                        let rest = &k[prefix.len()..];
                        (!rest.contains('/')).then(|| rest.to_owned())
                    })
                    .collect::<BTreeSet<_>>();
                ensure!(
                    actual == expected,
                    "directory changed during capture: {path}"
                );
            }
        }
        Ok(tree)
    }

    pub fn apply(&self, recipe: &Recipe, cache: &Cache, plan: &Restore) -> Result<()> {
        plan.before.validate(recipe)?;
        plan.after.validate(recipe)?;
        let stage = format!(".buddy-stage-{}", plan.op);
        ensure!(key(&plan.op), "invalid restore operation");
        for name in plan.before.0.keys().chain(plan.after.0.keys()) {
            ensure!(
                !name.split('/').any(|s| s == stage),
                "staging name collision"
            );
        }
        let current = self.capture(recipe, cache, Some(&stage))?;
        for (k, v) in &current.0 {
            ensure!(
                Some(v) == plan.before.0.get(k) || Some(v) == plan.after.0.get(k),
                "unexpected local edit during restore at {k}; originals are cached"
            );
        }
        let mut stages = BTreeSet::new();
        for (k, target) in &plan.after.0 {
            if current.0.get(k) == Some(target) {
                continue;
            }
            if let Entry::File { hash, size, chunks } = target {
                let (root, _) = self.where_is(k)?;
                let stage_path = Path::new(&stage);
                if stages.insert(root.anchor.clone()) {
                    match root.dir.create_dir(stage_path) {
                        Ok(()) => (),
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => ensure!(
                            root.dir.symlink_metadata(stage_path)?.is_dir(),
                            "invalid staging directory"
                        ),
                        Err(e) => return Err(e.into()),
                    }
                }
                let temp = stage_path.join(digest(k.as_bytes()));
                if root.dir.symlink_metadata(&temp).is_ok() {
                    root.dir.remove_file(&temp)?;
                }
                let mut file = root
                    .dir
                    .open_with(&temp, OpenOptions::new().write(true).create_new(true))?;
                let mut digest = blake3::Hasher::new();
                let mut len = 0u64;
                for c in chunks {
                    let data = cache.get(&c.hash)?;
                    ensure!(data.len() == c.size as usize, "chunk length mismatch");
                    file.write_all(&data)?;
                    digest.update(&data);
                    len += data.len() as u64;
                }
                ensure!(
                    len == *size && digest.finalize().to_hex().as_str() == hash,
                    "restored file digest mismatch"
                );
                file.sync_all()?;
            }
        }
        let verified = self.capture(recipe, cache, Some(&stage))?;
        ensure!(
            verified == current,
            "local files changed during restore preparation"
        );
        for (k, entry) in current.0.iter().rev() {
            let target = plan.after.0.get(k);
            if target.is_none()
                || target
                    .is_some_and(|v| std::mem::discriminant(v) != std::mem::discriminant(entry))
            {
                let (root, rel) = self.where_is(k)?;
                if rel == Path::new(".") {
                    continue;
                }
                match entry {
                    Entry::Directory => root.dir.remove_dir(&rel)?,
                    Entry::File { .. } => root.dir.remove_file(&rel)?,
                }
                sync_dir(&root.anchor.join(rel.parent().unwrap_or(Path::new("."))))?;
            }
        }
        for (k, entry) in &plan.after.0 {
            if current.0.get(k) == Some(entry) {
                continue;
            }
            let (root, rel) = self.where_is(k)?;
            match entry {
                Entry::Directory => {
                    if rel != Path::new(".") {
                        root.dir.create_dir(&rel)?;
                    }
                }
                Entry::File { .. } => root.dir.rename(
                    Path::new(&stage).join(digest(k.as_bytes())),
                    &root.dir,
                    &rel,
                )?,
            }
            sync_dir(&root.anchor.join(rel.parent().unwrap_or(Path::new("."))))?;
        }
        for root in self.roots.values() {
            // Only journal-owned stage files are removed. Unexpected contents stop recovery.
            if root.dir.symlink_metadata(&stage).is_ok() {
                let names = dir_names(&root.dir, Path::new(&stage))?;
                if !names.is_empty() {
                    let owned: BTreeSet<_> = plan
                        .after
                        .0
                        .keys()
                        .chain(plan.before.0.keys())
                        .map(|k| digest(k.as_bytes()))
                        .collect();
                    for name in names {
                        ensure!(
                            owned.contains(&name),
                            "unexpected file in restore staging directory"
                        );
                        root.dir.remove_file(Path::new(&stage).join(name))?;
                    }
                }
                root.dir.remove_dir(&stage)?;
                sync_dir(&root.anchor)?;
            }
        }
        ensure!(
            self.scan(recipe, cache, None, false)? == plan.after,
            "restore verification failed"
        );
        Ok(())
    }
}
fn dir_names(dir: &Dir, rel: &Path) -> Result<Vec<String>> {
    dir.read_dir(rel)?
        .map(|e| {
            e?.file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-Unicode filename in {}", rel.display()))
        })
        .collect()
}
fn capture_file(f: cap_std::fs::File, cache: &Cache, len: u64) -> Result<Entry> {
    let mut hash = blake3::Hasher::new();
    let mut chunks = vec![];
    let mut total = 0;
    let mut add = |bytes: &[u8]| -> Result<()> {
        if !bytes.is_empty() {
            let h = digest(bytes);
            cache.put(&h, bytes)?;
            hash.update(bytes);
            chunks.push(Chunk {
                hash: h,
                size: bytes.len() as u32,
            });
            total += bytes.len() as u64;
        }
        Ok(())
    };
    if len <= 256 * 1024 {
        let mut bytes = vec![];
        f.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        add(&bytes)?;
    } else {
        for chunk in StreamCDC::new(f, 256 * 1024, 1024 * 1024, MAX_CHUNK as u32) {
            add(&chunk?.data)?;
        }
    }
    if total != len {
        bail!("file size changed during capture");
    }
    Ok(Entry::File {
        size: total,
        hash: hash.finalize().to_hex().to_string(),
        chunks,
    })
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Restore {
    #[serde(default)]
    pub rollback: bool,
    pub op: String,
    pub before: Tree,
    pub after: Tree,
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn replaced_root_requires_a_new_preview() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("drive");
        let home = tmp.path().join("profile");
        fs::create_dir(&path)?;
        fs::write(path.join("old.world"), b"first")?;
        let before = preview(&home, &path, Kind::Folder, "saves", || false)?;
        fs::rename(&path, tmp.path().join("removed-drive"))?;
        fs::create_dir(&path)?;
        fs::write(path.join("other.world"), b"second")?;
        assert!(Binding::for_selection(&path, &Kind::Folder, Some(&before.binding)).is_err());
        let after = preview(&home, &path, Kind::Folder, "saves", || false)?;
        assert_eq!(after.files, ["other.world"]);
        Binding::for_selection(&path, &Kind::Folder, Some(&after.binding))?;
        Ok(())
    }
    #[test]
    fn inventories_real_files_and_checks_portability() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("save");
        fs::create_dir_all(dir.join("player"))?;
        fs::write(dir.join("world"), b"data")?;
        fs::write(dir.join("player/character"), b"avatar")?;
        let home = tmp.path().join("client");
        let read = |p: &Path, kind| preview(&home, p, kind, "saves", || false);
        let result = read(&dir, Kind::Folder)?;
        assert_eq!(result.files, ["player/character", "world"]);
        assert_eq!(result.bytes, 10);
        assert_eq!(read(&dir.join("world"), Kind::File)?.files, ["world"]);
        assert!(read(&dir.join("missing"), Kind::File)?.files.is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&home, dir.join("outside"))?;
            assert!(read(&dir, Kind::Folder).is_err());
            fs::remove_file(dir.join("outside"))?;
            fs::write(dir.join("WORLD"), b"second")?;
            if dir.join("world").metadata()?.len() == 4 {
                assert!(read(&dir, Kind::Folder).is_err());
            }
        }
        assert!(preview(&home, &dir, Kind::Folder, "saves", || true).is_err());
        Ok(())
    }
}
