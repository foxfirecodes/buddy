use crate::{
    local::{Local, roots},
    model::{Kind, digest},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize, Serialize)]
struct Steam {
    id: Option<u32>,
}
#[derive(Default, Deserialize, Serialize)]
struct FileRule {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    when: Vec<Condition>,
}
#[derive(Deserialize, Serialize)]
struct Condition {
    os: Option<String>,
    store: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct Game {
    #[serde(default)]
    steam: Steam,
    #[serde(default)]
    files: BTreeMap<String, FileRule>,
}
#[derive(Clone)]
pub struct Suggestion {
    pub path: PathBuf,
    pub kind: Kind,
    pub settings: bool,
    pub source: String,
}

// Index once on explicit local import. Only the games with IDs and file rules need disk entries.
pub fn import(local: &Local, path: &Path) -> Result<usize> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024 * 1024, "manifest exceeds 64 MiB");
    let catalog: BTreeMap<String, Game> =
        serde_yaml_ng::from_slice(&bytes).context("parse Ludusavi YAML manifest")?;
    let revision = digest(&bytes);
    let dir = local.home.join("manifests");
    fs::create_dir_all(&dir)?;
    let tmp = tempfile::tempdir_in(&dir)?;
    let mut count = 0;
    for game in catalog.into_values() {
        if let Some(id) = game.steam.id
            && !game.files.is_empty()
        {
            fs::write(
                tmp.path().join(id.to_string()),
                serde_json::to_vec(&game.files)?,
            )?;
            count += 1;
        }
    }
    let destination = dir.join(&revision);
    if !destination.exists() {
        match fs::rename(tmp.path(), &destination) {
            Ok(()) => (),
            Err(_) if destination.exists() => (),
            Err(e) => return Err(e.into()),
        }
    }
    let mut current = tempfile::NamedTempFile::new_in(&dir)?;
    write!(current, "{revision}")?;
    current.as_file().sync_all()?;
    current.persist(dir.join("current"))?;
    Ok(count)
}
pub fn candidates(local: &Local, game: &str) -> Result<Vec<Suggestion>> {
    let Some(id) = game
        .strip_prefix("steam-")
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return Ok(vec![]);
    };
    let dir = local.home.join("manifests");
    if !dir.join("current").is_file() {
        return Ok(vec![]);
    }
    let revision = fs::read_to_string(dir.join("current"))?;
    ensure!(crate::model::hash_ok(&revision), "invalid manifest index");
    let index = dir.join(&revision).join(id.to_string());
    if !index.is_file() {
        return Ok(vec![]);
    }
    let rules: BTreeMap<String, FileRule> = serde_json::from_slice(&fs::read(index)?)?;
    let roots = roots(game);
    let mut vars = BTreeMap::<&str, PathBuf>::new();
    for (a, b) in [
        ("<home>", "home"),
        ("<base>", "install"),
        ("<xdgData>", "data"),
        ("<xdgConfig>", "config"),
        ("<winDocuments>", "documents"),
    ] {
        if let Some(v) = roots.get(b) {
            vars.insert(a, v.clone());
        }
    }
    let platform = if roots.contains_key("pfx") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        std::env::consts::OS
    };
    if let Some(prefix) = roots.get("pfx") {
        let user = prefix.join("drive_c/users/steamuser");
        for (k, p) in [
            ("<winDocuments>", "Documents"),
            ("<winAppData>", "AppData/Roaming"),
            ("<winLocalAppData>", "AppData/Local"),
            ("<winLocalAppDataLow>", "AppData/LocalLow"),
        ] {
            vars.insert(k, user.join(p));
        }
    } else if cfg!(windows) {
        for (k, n) in [
            ("<winAppData>", "APPDATA"),
            ("<winLocalAppData>", "LOCALAPPDATA"),
        ] {
            if let Some(p) = std::env::var_os(n) {
                vars.insert(k, p.into());
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut found = vec![];
    for (template, rule) in rules {
        if !rule.when.is_empty()
            && !rule.when.iter().any(|w| {
                w.os.as_deref().is_none_or(|s| s == platform)
                    && w.store.as_deref().is_none_or(|s| s == "steam")
            })
        {
            continue;
        }
        let Some((token, rest)) = template.split_once('/') else {
            continue;
        };
        let Some(root) = vars.get(token) else {
            continue;
        };
        // Only expand rooted, bounded-depth patterns. Unresolved placeholders stay available via Browse.
        if rest.contains(['<', '>', '\\', ':'])
            || rest.contains("**")
            || rest
                .split('/')
                .any(|p| p.is_empty() || p == ".." || p == ".")
        {
            continue;
        }
        let Ok(anchor) = root.canonicalize() else {
            continue;
        };
        let Some(root_text) = anchor.to_str() else {
            continue;
        };
        let pattern = format!("{}/{}", glob::Pattern::escape(root_text), rest);
        let Ok(paths) = glob::glob(&pattern) else {
            continue;
        };
        for path in paths.take(200).flatten() {
            let Ok(real) = path.canonicalize() else {
                continue;
            };
            if !real.starts_with(&anchor) || !seen.insert(real.clone()) {
                continue;
            }
            let kind = if real.is_dir() {
                Kind::Folder
            } else if real.is_file() {
                Kind::File
            } else {
                continue;
            };
            found.push(Suggestion {
                path: real,
                kind,
                settings: rule.tags.iter().any(|s| s == "config"),
                source: format!("Ludusavi manifest {revision}"),
            });
        }
    }
    Ok(found)
}
