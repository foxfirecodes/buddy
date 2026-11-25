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
    io::{ErrorKind, Read, Seek, Write},
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
    fn matches_dir(&self, dir: &Dir) -> Result<bool> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            let m = dir.dir_metadata()?;
            Ok(m.dev() == self.dev && m.ino() == self.ino)
        }
        #[cfg(windows)]
        {
            Ok(Some(identity(dir)?) == self.identity)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = dir;
            Ok(true)
        }
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
fn check_link(meta: &cap_std::fs::Metadata, logical: &str) -> Result<()> {
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
    Ok(())
}

impl Root {
    fn open_file(
        &self,
        logical: &str,
        path: &Path,
        meta: &cap_std::fs::Metadata,
    ) -> Result<cap_std::fs::File> {
        check_link(meta, logical)?;
        ensure!(meta.is_file(), "special file in selection: {logical}");
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(target_os = "linux")]
        {
            use cap_std::fs::OpenOptionsExt;
            use rustix::fs::OFlags;
            options.custom_flags((OFlags::NONBLOCK | OFlags::NOFOLLOW).bits() as i32);
        }
        let file = self.dir.open_with(path, &options)?;
        ensure!(
            same_file_state(meta, &file.metadata()?)?,
            "file replaced during capture: {logical}"
        );
        Ok(file)
    }

    fn read_file(
        &self,
        logical: &str,
        path: &Path,
        meta: &cap_std::fs::Metadata,
        cache: &Cache,
        allow_hints: bool,
        expected: Option<&Entry>,
    ) -> Result<Entry> {
        if let Some(entry) = expected {
            ensure!(
                matches!(entry, Entry::File { size, .. } if *size == meta.len()),
                "unexpected local edit during restore at {logical}"
            );
        }
        let stamp = self.volume.stamp(meta);
        let absolute = self.anchor.join(path);
        let hint = match &stamp {
            Some(stamp) if allow_hints => cache.1.lookup(&absolute, stamp, cache)?,
            _ => None,
        };
        if let Some(hint) = &hint
            && hint.trusted
            && expected.is_none_or(|known| known == &hint.entry)
        {
            return Ok(hint.entry.clone());
        }
        let mut file = self.open_file(logical, path, meta)?;
        // Verification only needs the manifest's whole-file digest. The same
        // check can confirm a recently installed file whose ctime was too
        // fresh to trust when its bytes were last observed.
        let candidate = expected.or_else(|| hint.as_ref().map(|h| &h.entry));
        let entry = if let Some(entry @ Entry::File { hash, .. }) = candidate {
            let mut digest = blake3::Hasher::new();
            digest.update_reader(&mut file)?;
            if digest.finalize().to_hex().as_str() == hash {
                entry.clone()
            } else {
                ensure!(
                    expected.is_none(),
                    "unexpected local edit during restore at {logical}"
                );
                file.rewind()?;
                capture_file(file, cache, meta.len())?
            }
        } else {
            capture_file(file, cache, meta.len())?
        };
        ensure!(
            same_file_state(meta, &self.dir.symlink_metadata(path)?)?,
            "file changed during capture: {logical}"
        );
        if let Some(stamp) = &stamp {
            cache.1.remember(&absolute, stamp, &entry)?;
        }
        Ok(entry)
    }

    fn walk(
        &self,
        slot: &str,
        stage: Option<&str>,
        rules: &Slot,
        mut visit: impl FnMut(String, PathBuf, cap_std::fs::Metadata) -> Result<()>,
    ) -> Result<()> {
        let mut todo = vec![(slot.to_owned(), self.base.clone())];
        while let Some((logical, path)) = todo.pop() {
            if rules.ignores_path(logical_path(&logical).1) {
                continue;
            }
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
            check_link(&meta, &logical)?;
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

#[derive(Clone, Debug)]
pub enum ReviewedBinding {
    Existing(Binding),
    Missing {
        base: Binding,
        relative: PathBuf,
        ancestors: Vec<Binding>,
    },
}

impl ReviewedBinding {
    pub fn validate(&self, path: &Path, kind: &Kind) -> Result<()> {
        match self {
            Self::Existing(binding) => {
                Binding::for_selection(path, kind, Some(binding))?;
            }
            Self::Missing {
                base,
                relative,
                ancestors,
            } => {
                Binding::for_selection(&base.path, &Kind::Folder, Some(base))?;
                ensure!(
                    base.path.join(relative) == path,
                    "destination changed since preview"
                );
                let folder = if *kind == Kind::File {
                    relative.parent().context("missing parent")?
                } else {
                    relative.as_path()
                };
                let mut current = base.path.clone();
                for (index, part) in folder.components().enumerate() {
                    ensure!(
                        matches!(part, std::path::Component::Normal(_)),
                        "invalid destination"
                    );
                    current.push(part);
                    if let Some(prior) = ancestors.get(index) {
                        Binding::for_selection(&current, &Kind::Folder, Some(prior))?;
                    } else {
                        match fs::symlink_metadata(&current) {
                            Err(e) if e.kind() == ErrorKind::NotFound => (),
                            Ok(_) => bail!("destination changed since preview"),
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
            }
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn resolve(&self, path: &Path, kind: &Kind) -> Result<Binding> {
        self.resolve_with_created(path, kind, &mut BTreeMap::new())
    }
    pub fn resolve_with_created(
        &self,
        path: &Path,
        kind: &Kind,
        created: &mut BTreeMap<PathBuf, Binding>,
    ) -> Result<Binding> {
        match self {
            Self::Existing(binding) => Binding::for_selection(path, kind, Some(binding)),
            Self::Missing {
                base,
                relative,
                ancestors,
            } => {
                Binding::for_selection(&base.path, &Kind::Folder, Some(base))?;
                ensure!(
                    base.path.join(relative) == path,
                    "destination changed since preview"
                );
                let folder = if *kind == Kind::File {
                    relative.parent().context("missing parent")?
                } else {
                    relative.as_path()
                };
                let mut dir = Dir::open_ambient_dir(&base.path, ambient_authority())?;
                ensure!(base.matches_dir(&dir)?, "mapped root changed since preview");
                let mut current = base.path.clone();
                for (index, part) in folder.components().enumerate() {
                    let std::path::Component::Normal(part) = part else {
                        bail!("invalid destination")
                    };
                    current.push(part);
                    if let Some(reviewed) = ancestors.get(index) {
                        Binding::for_selection(&current, &Kind::Folder, Some(reviewed))?;
                    } else if let Some(reviewed) = created.get(&current) {
                        Binding::for_selection(&current, &Kind::Folder, Some(reviewed))?;
                    } else {
                        if let Err(error) = dir.create_dir(part) {
                            if error.kind() == ErrorKind::AlreadyExists {
                                return Err(error).context("destination changed since preview");
                            }
                            return Err(error).with_context(|| {
                                format!("create destination directory {}", current.display())
                            });
                        }
                        created.insert(current.clone(), Binding::new(&current, &Kind::Folder)?);
                    }
                    let opened = cap_primitives::fs::open_dir_nofollow(
                        &dir.into_std_file(),
                        Path::new(part),
                    )?;
                    dir = Dir::from_std_file(opened);
                    let expected = ancestors
                        .get(index)
                        .or_else(|| created.get(&current))
                        .context("missing directory identity")?;
                    ensure!(
                        expected.matches_dir(&dir)?,
                        "destination changed since preview"
                    );
                }
                let binding = Binding::new(path, kind)?;
                ensure!(
                    binding.path == path && binding.matches_dir(&dir)?,
                    "destination changed since preview"
                );
                Ok(binding)
            }
        }
    }
}

pub fn preview_missing(
    home: &Path,
    path: &Path,
    kind: Kind,
    _slot: &str,
    base: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<Preview> {
    ensure!(!cancelled(), "preview cancelled");
    let original_base = std::path::absolute(base)?;
    let path = std::path::absolute(path)?;
    let relative = path
        .strip_prefix(&original_base)
        .context("destination outside mapped root")?
        .to_owned();
    let base = Binding::new(base, &Kind::Folder)?;
    let path = base.path.join(&relative);
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "choose a game-specific location"
    );
    ensure!(
        !home.starts_with(&path) && !path.starts_with(home),
        "save selection overlaps Buddy's own data"
    );
    let mut current = base.path.clone();
    let mut ancestors = vec![];
    let folder = if kind == Kind::File {
        relative.parent().context("missing parent")?
    } else {
        relative.as_path()
    };
    for part in folder.components() {
        ensure!(!cancelled(), "preview cancelled");
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) => {
                ensure!(
                    m.is_dir() && !m.file_type().is_symlink(),
                    "destination contains a link or non-directory"
                );
                ancestors.push(Binding::new(&current, &Kind::Folder)?);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(
        !anchor(&path, &kind)?.exists(),
        "destination already exists; review again"
    );
    Ok(Preview {
        binding: ReviewedBinding::Missing {
            base,
            relative,
            ancestors,
        },
        files: vec![],
        bytes: 0,
    })
}

pub struct Preview {
    pub binding: ReviewedBinding,
    pub files: Vec<String>,
    pub bytes: u64,
}
pub fn preview(
    home: &Path,
    path: &Path,
    kind: Kind,
    slot: &str,
    ignores: BTreeSet<String>,
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
                codec: if ignores.is_empty() {
                    raw_codec()
                } else {
                    FILTERED_CODEC.into()
                },
                ignores,
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
        binding: ReviewedBinding::Existing(binding),
        files: vec![],
        bytes: 0,
    };
    let mut names = BTreeSet::new();
    files.roots[slot].walk(slot, None, &recipe.slots[slot], |logical, _, meta| {
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

#[derive(Debug)]
pub(crate) enum SaveLocation {
    Unavailable(PathBuf),
    Changed(PathBuf),
}

impl std::fmt::Display for SaveLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(path) => write!(
                f,
                "Buddy couldn’t open this save location. Reconnect its drive or check permissions.\n\n{}",
                path.display()
            ),
            Self::Changed(path) => write!(
                f,
                "The drive or root changed. Reconnect the original save location at {} or review it in Edit files.",
                path.display()
            ),
        }
    }
}
impl std::error::Error for SaveLocation {}

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
            let unavailable = || SaveLocation::Unavailable(b.path.clone());
            ensure!(
                parent == parent.canonicalize().with_context(unavailable)?,
                SaveLocation::Changed(b.path.clone())
            );
            let dir =
                Dir::open_ambient_dir(&parent, ambient_authority()).with_context(unavailable)?;
            ensure!(
                b.matches_dir(&dir).with_context(unavailable)?,
                SaveLocation::Changed(b.path.clone())
            );
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
        self.read_tree(recipe, cache, stage, allow_hints, None)
    }
    fn read_tree(
        &self,
        recipe: &Recipe,
        cache: &Cache,
        stage: Option<&str>,
        allow_hints: bool,
        expected: Option<&Tree>,
    ) -> Result<Tree> {
        let mut tree = Tree::default();
        let mut stamps = BTreeMap::new();
        for (slot, root) in &self.roots {
            root.walk(slot, stage, &recipe.slots[slot], |logical, path, meta| {
                let entry = if meta.is_dir() {
                    Entry::Directory
                } else {
                    let expected_file = match expected {
                        Some(tree) => Some(tree.0.get(&logical).with_context(|| {
                            format!("unexpected local edit during restore at {logical}")
                        })?),
                        None => None,
                    };
                    let entry =
                        root.read_file(&logical, &path, &meta, cache, allow_hints, expected_file)?;
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
                let (slot, relative) = logical_path(path);
                let actual = dir_names(&root.dir, &rel)?
                    .into_iter()
                    .filter(|s| rel != root.base || Some(s.as_str()) != stage)
                    .filter(|name| {
                        let rules = &recipe.slots[slot];
                        rules.ignores.is_empty()
                            || !rules.ignores_path(&if relative.is_empty() {
                                name.clone()
                            } else {
                                format!("{relative}/{name}")
                            })
                    })
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
        // File slots may be absent from the entire inventory. Catch a newly appeared
        // slot as well as additions beneath the directories checked above.
        for (slot, root) in &self.roots {
            if root.kind == Kind::File
                && !recipe.slots[slot].ignores_path("")
                && !tree.0.contains_key(slot)
            {
                match root.dir.symlink_metadata(&root.base) {
                    Err(e) if e.kind() == ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                    Ok(_) => bail!("file appeared during capture: {slot}"),
                }
            }
        }
        tree.prune_ignore_parents(recipe);
        if let Some(expected) = expected {
            ensure!(
                &tree == expected,
                "unexpected local edit during restore: file inventory changed"
            );
        }
        Ok(tree)
    }

    pub fn apply_new(&self, recipe: &Recipe, cache: &Cache, plan: &Restore) -> Result<()> {
        self.restore(recipe, cache, plan, false)
    }
    pub fn apply(&self, recipe: &Recipe, cache: &Cache, plan: &Restore) -> Result<()> {
        self.restore(recipe, cache, plan, true)
    }
    fn restore(&self, recipe: &Recipe, cache: &Cache, plan: &Restore, replay: bool) -> Result<()> {
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
        // The journal owns its original staging files even if a later recipe excludes
        // one of its destinations. Only the still-managed paths may be touched on replay.
        let journal = plan;
        let filtered;
        let plan = if replay && recipe.slots.values().any(|s| !s.ignores.is_empty()) {
            filtered = Restore {
                rollback: plan.rollback,
                op: plan.op.clone(),
                before: plan.before.project(recipe),
                after: plan.after.project(recipe),
            };
            &filtered
        } else {
            plan
        };
        let current = if replay {
            self.recover_inventory(recipe, cache, plan, &stage)?
        } else {
            self.read_tree(recipe, cache, None, true, Some(&plan.before))?
        };
        self.stage_files(cache, &stage, &current, &plan.after)?;
        self.install_staged(recipe, cache, plan, &stage, &current, replay)?;
        self.clear_staging(&stage, journal)?;
        self.read_tree(recipe, cache, None, !replay, Some(&plan.after))
            .context("restore verification failed")?;
        Ok(())
    }

    fn recover_inventory(
        &self,
        recipe: &Recipe,
        cache: &Cache,
        plan: &Restore,
        stage: &str,
    ) -> Result<Tree> {
        // Read surviving bytes to repair cached chunks before resuming or rolling back.
        let mut current = self.scan(recipe, cache, Some(stage), false)?;
        for (name, entry) in &mut current.0 {
            let candidates = [plan.after.0.get(name), plan.before.0.get(name)];
            // Prefer the target when both versions contain the same bytes. Its
            // chunk boundaries can differ even if the source layout exactly matches.
            let matching = candidates
                .into_iter()
                .flatten()
                .find(|candidate| match (&*entry, candidate) {
                    (Entry::Directory, Entry::Directory) => true,
                    (
                        Entry::File { size, hash, .. },
                        Entry::File {
                            size: s, hash: h, ..
                        },
                    ) => size == s && hash == h,
                    _ => false,
                })
                .with_context(|| {
                    format!("unexpected local edit during restore at {name}; originals are cached")
                })?;
            if matching != entry {
                self.repair_chunks(name, matching, cache)?;
                *entry = matching.clone();
            }
        }
        for (name, old) in &plan.before.0 {
            // Same-kind replacements are atomic. Neither one nor an untouched
            // file may legitimately disappear halfway through this journal.
            if plan
                .after
                .0
                .get(name)
                .is_some_and(|new| std::mem::discriminant(old) == std::mem::discriminant(new))
            {
                ensure!(
                    current.0.contains_key(name),
                    "unexpected deletion during restore at {name}"
                );
            }
        }
        Ok(current)
    }

    fn stage_files(&self, cache: &Cache, stage: &str, current: &Tree, after: &Tree) -> Result<()> {
        let mut stages = BTreeSet::new();
        for (k, target) in &after.0 {
            if current.0.get(k) == Some(target) {
                continue;
            }
            if let Entry::File { hash, size, chunks } = target {
                let (root, _) = self.where_is(k)?;
                let stage_path = Path::new(stage);
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
        Ok(())
    }

    fn install_staged(
        &self,
        recipe: &Recipe,
        cache: &Cache,
        plan: &Restore,
        stage: &str,
        current: &Tree,
        replay: bool,
    ) -> Result<()> {
        self.read_tree(recipe, cache, Some(stage), !replay, Some(current))
            .context("local files changed during restore preparation")?;
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
                    Entry::Directory => match root.dir.remove_dir(&rel) {
                        Ok(()) => (),
                        Err(e) if e.kind() == ErrorKind::DirectoryNotEmpty && target.is_none()
                            && recipe.slots[logical_path(k).0].contains_ignore(logical_path(k).1) => (),
                        Err(e) => return Err(e).with_context(|| format!("Cannot replace folder {k}. Move any excluded contents aside and retry.")),
                    },
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
                        match root.dir.create_dir(&rel) {
                            Err(e)
                                if e.kind() == ErrorKind::AlreadyExists
                                    && recipe.slots[logical_path(k).0]
                                        .contains_ignore(logical_path(k).1) =>
                            {
                                let meta = root.dir.symlink_metadata(&rel)?;
                                check_link(&meta, k)?;
                                ensure!(meta.is_dir(), "expected folder at {k}");
                            }
                            other => other?,
                        }
                    }
                }
                Entry::File { .. } => {
                    root.dir
                        .rename(Path::new(stage).join(digest(k.as_bytes())), &root.dir, &rel)?
                }
            }
            sync_dir(&root.anchor.join(rel.parent().unwrap_or(Path::new("."))))?;
        }
        Ok(())
    }

    fn clear_staging(&self, stage: &str, plan: &Restore) -> Result<()> {
        for root in self.roots.values() {
            // Only journal-owned stage files are removed. Unexpected contents stop recovery.
            if root.dir.symlink_metadata(stage).is_ok() {
                let names = dir_names(&root.dir, Path::new(stage))?;
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
                        root.dir.remove_file(Path::new(stage).join(name))?;
                    }
                }
                root.dir.remove_dir(stage)?;
                sync_dir(&root.anchor)?;
            }
        }
        Ok(())
    }
}
impl Files {
    fn repair_chunks(&self, name: &str, original: &Entry, cache: &Cache) -> Result<()> {
        let Entry::File { size, hash, chunks } = original else {
            bail!("cannot repair chunks of a directory")
        };
        let (root, path) = self.where_is(name)?;
        let meta = root.dir.symlink_metadata(&path)?;
        let mut file = root.open_file(name, &path, &meta)?;
        ensure!(meta.len() == *size, "local file size changed: {name}");
        let mut full = blake3::Hasher::new();
        let mut buffer = Vec::new();
        for chunk in chunks {
            ensure!(chunk.size as usize <= MAX_CHUNK, "invalid chunk size");
            buffer.resize(chunk.size as usize, 0);
            file.read_exact(&mut buffer)?;
            // Use the original boundaries, which may differ from this client's chunker.
            cache.put(&chunk.hash, &buffer)?;
            full.update(&buffer);
        }
        ensure!(
            full.finalize().to_hex().as_str() == hash
                && same_file_state(&meta, &root.dir.symlink_metadata(&path)?)?,
            "local file changed while preserving rollback data: {name}"
        );
        Ok(())
    }

    // The restore journal must support an offline rollback. A scan hint only confirms
    // chunk sizes, so check the originals we are about to overwrite or delete.
    pub fn prepare_rollback(
        &self,
        before: &Tree,
        after: &Tree,
        cache: &Cache,
        target_chunks: &mut BTreeMap<String, u32>,
    ) -> Result<()> {
        let mut verified = BTreeSet::new();
        for (name, original) in &before.0 {
            let Entry::File { chunks, .. } = original else {
                continue;
            };
            if after.0.get(name) == Some(original) {
                continue;
            }
            for chunk in chunks {
                if verified.insert(&chunk.hash)
                    && !cache
                        .get(&chunk.hash)
                        .is_ok_and(|d| d.len() == chunk.size as usize)
                {
                    self.repair_chunks(name, original, cache)
                        .with_context(|| format!("preserving rollback data for {name}"))?;
                    verified.extend(chunks.iter().map(|c| &c.hash));
                    break;
                }
            }
            for chunk in chunks {
                if let Some(target_size) = target_chunks.remove(&chunk.hash) {
                    ensure!(target_size == chunk.size, "inconsistent chunk lengths");
                }
            }
        }
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
mod rollback_tests {
    use super::*;

    #[test]
    fn verifies_only_changed_files_and_repairs_their_originals() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let save = tmp.path().join("save");
        let cache_dir = tmp.path().join("cache");
        fs::create_dir(&save)?;
        fs::create_dir(&cache_dir)?;
        let cache = Cache::open(cache_dir)?;
        let recipe: Recipe = serde_json::from_value(serde_json::json!({
            "game":"test", "title":"Test", "retention":3, "slots":{"save":{"kind":"Folder"}}
        }))?;
        let binding = Binding::new(&save, &Kind::Folder)?;
        let files = Files::open(
            &recipe,
            &BTreeMap::from([("save".into(), binding)]),
            &cache.0,
        )?;
        fs::write(save.join("remove"), b"original")?;
        fs::write(save.join("keep"), b"unchanged")?;
        let before = files.capture(&recipe, &cache, None)?;
        let mut after = before.clone();
        let moved = after.0.remove("save/remove").unwrap();
        after.0.insert("save/elsewhere".into(), moved);
        let original = digest(b"original");
        let unchanged = digest(b"unchanged");
        fs::write(cache.0.join(&original), b"damaged!")?;
        fs::write(cache.0.join(&unchanged), b"unrelated")?;
        let mut target = after.chunks_since(Some(&before))?;
        assert_eq!(target, BTreeMap::from([(original.clone(), 8)]));
        files.prepare_rollback(&before, &after, &cache, &mut target)?;
        assert!(target.is_empty());
        assert_eq!(cache.get(&original)?, b"original");
        assert!(cache.get(&unchanged).is_err());

        let mut backup = after.chunks()?;
        files.prepare_rollback(&before, &after, &cache, &mut backup)?;
        assert_eq!(backup, BTreeMap::from([(unchanged, 9)]));
        fs::write(cache.0.join(&original), b"damaged!")?;
        fs::write(save.join("remove"), b"external")?;
        assert!(
            files
                .prepare_rollback(&before, &after, &cache, &mut backup)
                .is_err()
        );
        assert_eq!(fs::read(save.join("remove"))?, b"external");
        Ok(())
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn missing_recipe_destination_is_created_only_on_approval() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().join("games");
        fs::create_dir(&base)?;
        let home = tmp.path().join("buddy");
        let path = base.join("game/saves");
        let reviewed = preview_missing(&home, &path, Kind::Folder, "saves", &base, || false)?;
        assert!(!path.exists());
        reviewed.binding.resolve(&path, &Kind::Folder)?;
        assert!(path.is_dir());
        Ok(())
    }

    #[test]
    fn missing_sibling_destinations_share_created_ancestor() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().join("games");
        fs::create_dir(&base)?;
        let home = tmp.path().join("buddy");
        let a = base.join("game/players");
        let b = base.join("game/worlds");
        let first = preview_missing(&home, &a, Kind::Folder, "players", &base, || false)?;
        let second = preview_missing(&home, &b, Kind::Folder, "worlds", &base, || false)?;
        first.binding.validate(&a, &Kind::Folder)?;
        second.binding.validate(&b, &Kind::Folder)?;
        let mut created = BTreeMap::new();
        first
            .binding
            .resolve_with_created(&a, &Kind::Folder, &mut created)?;
        second
            .binding
            .resolve_with_created(&b, &Kind::Folder, &mut created)?;
        assert!(a.is_dir() && b.is_dir());
        Ok(())
    }

    #[test]
    fn missing_file_creates_parent_but_not_file() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().join("games");
        fs::create_dir(&base)?;
        let path = base.join("game/save.dat");
        let reviewed = preview_missing(
            &tmp.path().join("buddy"),
            &path,
            Kind::File,
            "save",
            &base,
            || false,
        )?;
        reviewed.binding.resolve(&path, &Kind::File)?;
        assert!(path.parent().unwrap().is_dir());
        assert!(!path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn missing_destination_rejects_symlink_after_preview() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().join("games");
        fs::create_dir(&base)?;
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside)?;
        let path = base.join("game/saves");
        let reviewed = preview_missing(
            &tmp.path().join("buddy"),
            &path,
            Kind::Folder,
            "save",
            &base,
            || false,
        )?;
        std::os::unix::fs::symlink(&outside, base.join("game"))?;
        assert!(reviewed.binding.resolve(&path, &Kind::Folder).is_err());
        assert!(!outside.join("saves").exists());
        Ok(())
    }

    #[test]
    fn missing_destination_rejects_changed_ancestor() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().join("games");
        fs::create_dir_all(base.join("game"))?;
        let path = base.join("game/saves");
        let reviewed = preview_missing(
            &tmp.path().join("buddy"),
            &path,
            Kind::Folder,
            "save",
            &base,
            || false,
        )?;
        fs::rename(base.join("game"), base.join("old"))?;
        fs::create_dir(base.join("game"))?;
        assert!(reviewed.binding.resolve(&path, &Kind::Folder).is_err());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn replaced_root_requires_a_new_preview() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("drive");
        let home = tmp.path().join("profile");
        fs::create_dir(&path)?;
        fs::write(path.join("old.world"), b"first")?;
        let before = preview(
            &home,
            &path,
            Kind::Folder,
            "saves",
            Default::default(),
            || false,
        )?;
        fs::rename(&path, tmp.path().join("removed-drive"))?;
        fs::create_dir(&path)?;
        fs::write(path.join("other.world"), b"second")?;
        assert!(before.binding.resolve(&path, &Kind::Folder).is_err());
        let after = preview(
            &home,
            &path,
            Kind::Folder,
            "saves",
            Default::default(),
            || false,
        )?;
        assert_eq!(after.files, ["other.world"]);
        after.binding.resolve(&path, &Kind::Folder)?;
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
        let read = |p: &Path, kind| preview(&home, p, kind, "saves", Default::default(), || false);
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
        assert!(
            preview(
                &home,
                &dir,
                Kind::Folder,
                "saves",
                Default::default(),
                || true
            )
            .is_err()
        );
        Ok(())
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;

    struct Setup {
        _tmp: tempfile::TempDir,
        saves: PathBuf,
        cache: Cache,
        recipe: Recipe,
        files: Files,
        plan: Restore,
    }
    fn setup() -> Result<Setup> {
        let tmp = tempfile::tempdir()?;
        let saves = tmp.path().join("saves");
        let profile = tmp.path().join("profile");
        fs::create_dir(&saves)?;
        fs::create_dir(&profile)?;
        fs::write(saves.join("unchanged"), vec![1u8; 1024 * 1024])?;
        fs::write(saves.join("world"), b"local")?;
        let recipe: Recipe = serde_json::from_value(serde_json::json!({
            "game":"test", "title":"Game", "retention":3, "slots":{"saves":{"kind":"Folder"}}
        }))?;
        let bindings = BTreeMap::from([("saves".into(), Binding::new(&saves, &Kind::Folder)?)]);
        let files = Files::open(&recipe, &bindings, &profile)?;
        let cache = Cache::open(profile)?;
        let before = files.capture(&recipe, &cache, None)?;
        let mut after = before.clone();
        let bytes = b"remote";
        let hash = digest(bytes);
        cache.put(&hash, bytes)?;
        after.0.insert(
            "saves/world".into(),
            Entry::File {
                size: bytes.len() as u64,
                hash: hash.clone(),
                chunks: vec![Chunk {
                    hash,
                    size: bytes.len() as u32,
                }],
            },
        );
        Ok(Setup {
            _tmp: tmp,
            saves,
            cache,
            recipe,
            files,
            plan: Restore {
                op: "try".into(),
                rollback: false,
                before,
                after,
            },
        })
    }

    #[test]
    fn replay_leaves_a_newly_ignored_destination_and_removes_its_staging_file() -> Result<()> {
        let mut s = setup()?;
        let stage = ".buddy-stage-try";
        s.files
            .stage_files(&s.cache, stage, &s.plan.before, &s.plan.after)?;
        s.recipe
            .slots
            .get_mut("saves")
            .unwrap()
            .set_ignores(BTreeSet::from(["world".into()]))?;
        fs::write(s.saves.join("world"), b"independent local data")?;
        s.files.apply(&s.recipe, &s.cache, &s.plan)?;
        assert_eq!(fs::read(s.saves.join("world"))?, b"independent local data");
        assert!(!s.saves.join(stage).exists());
        Ok(())
    }

    #[test]
    fn same_size_edit_with_preserved_mtime_while_staging_stops_install() -> Result<()> {
        let s = setup()?;
        let current = s
            .files
            .read_tree(&s.recipe, &s.cache, None, true, Some(&s.plan.before))?;
        let stage = ".buddy-stage-try";
        s.files
            .stage_files(&s.cache, stage, &current, &s.plan.after)?;
        let path = s.saves.join("unchanged");
        let mtime = fs::metadata(&path)?.modified()?;
        let mut file = fs::OpenOptions::new().write(true).open(&path)?;
        file.write_all(b"x")?;
        file.set_modified(mtime)?;
        drop(file);
        let err = s
            .files
            .install_staged(&s.recipe, &s.cache, &s.plan, stage, &current, false)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unexpected local edit during restore at saves/unchanged")
        );
        assert_eq!(fs::read(s.saves.join("world"))?, b"local");
        assert_eq!(fs::read(path)?[0], b'x');
        Ok(())
    }

    #[test]
    fn missing_untouched_file_stops_fresh_restore_and_journal_replay() -> Result<()> {
        let s = setup()?;
        fs::remove_file(s.saves.join("unchanged"))?;
        assert!(s.files.apply_new(&s.recipe, &s.cache, &s.plan).is_err());
        assert!(s.files.apply(&s.recipe, &s.cache, &s.plan).is_err());
        assert_eq!(fs::read(s.saves.join("world"))?, b"local");
        assert!(!s.saves.join("unchanged").exists());
        Ok(())
    }

    #[test]
    fn corrupt_destination_chunk_never_replaces_the_original() -> Result<()> {
        let s = setup()?;
        fs::write(s.cache.0.join(digest(b"remote")), b"broken")?;
        let err = s.files.apply_new(&s.recipe, &s.cache, &s.plan).unwrap_err();
        assert!(format!("{err:#}").contains("corrupt cached chunk"));
        assert_eq!(fs::read(s.saves.join("world"))?, b"local");
        Ok(())
    }

    #[test]
    fn foreign_chunk_boundaries_survive_replay_and_offline_rollback() -> Result<()> {
        let mut s = setup()?;
        let parts: [&[u8]; 2] = [b"re", b"mote"];
        let Entry::File { chunks, .. } = s.plan.after.0.get_mut("saves/world").unwrap() else {
            unreachable!()
        };
        chunks.clear();
        for part in parts {
            let hash = digest(part);
            s.cache.put(&hash, part)?;
            chunks.push(Chunk {
                hash,
                size: part.len() as u32,
            });
        }
        s.files.apply_new(&s.recipe, &s.cache, &s.plan)?;
        let first = digest(parts[0]);
        fs::remove_file(s.cache.0.join(&first))?;
        let canonical = s.files.scan(&s.recipe, &s.cache, None, false)?;
        assert_ne!(canonical, s.plan.after);
        let original = std::mem::replace(&mut s.plan.before, canonical);
        s.files.apply(&s.recipe, &s.cache, &s.plan)?;
        assert_eq!(s.cache.get(&first)?, parts[0]);
        s.plan.before = original;
        fs::write(s.cache.0.join(&first), b"xx")?;
        std::mem::swap(&mut s.plan.before, &mut s.plan.after);
        s.plan.rollback = true;
        let mut chunks = s.plan.after.chunks_since(Some(&s.plan.before))?;
        s.files
            .prepare_rollback(&s.plan.before, &s.plan.after, &s.cache, &mut chunks)?;
        assert_eq!(s.cache.get(&first)?, parts[0]);
        s.files.apply(&s.recipe, &s.cache, &s.plan)?;
        assert_eq!(fs::read(s.saves.join("world"))?, b"local");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rollback_repair_rejects_a_special_file_before_reading() -> Result<()> {
        let s = setup()?;
        fs::remove_file(s.cache.0.join(digest(b"local")))?;
        let path = s.saves.join("world");
        fs::remove_file(&path)?;
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        let error = s
            .files
            .prepare_rollback(
                &s.plan.before,
                &s.plan.after,
                &s.cache,
                &mut BTreeMap::new(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("special file in selection"));
        Ok(())
    }
}
