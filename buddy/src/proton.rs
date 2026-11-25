//! Enroll an unbound Steam game at first Play, using the actual Proton launch context.
use crate::{
    files::{Binding, Files},
    local::{Api, Local, Setup},
    model::{Action, GameInfo, Kind, Slot},
    session::Interface,
};
use anyhow::{Context, Result, bail, ensure};
use cap_std::{ambient_authority, fs::Dir};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct Launch {
    compat: PathBuf,
    init: Vec<OsString>,
}

fn init_command(args: &[OsString]) -> Option<Vec<OsString>> {
    let mode = args.windows(3).position(|a| {
        let tool = Path::new(&a[0]);
        tool.is_absolute()
            && tool.file_name() == Some(OsStr::new("proton"))
            && (a[1] == "run" || a[1] == "waitforexitandrun")
    })?;
    let mut init = Vec::new();
    // Steam can prepend its reaper and the user can wrap the launch. Prefix setup
    // needs only Steam's selected runtime and Proton, never the executable or game args.
    if mode >= 3 && args[mode - 1] == "--" {
        let entry = args[..mode - 1]
            .iter()
            .rposition(|a| Path::new(a).file_name() == Some(OsStr::new("_v2-entry-point")))?;
        if !Path::new(&args[entry]).is_absolute() {
            return None;
        }
        init.extend_from_slice(&args[entry..mode]);
        for arg in &mut init {
            if arg == "--verb=waitforexitandrun" || arg == "--verb=run" {
                *arg = "--verb=getcompatpath".into();
            }
        }
    }
    init.extend([args[mode].clone(), "getcompatpath".into(), "/".into()]);
    Some(init)
}

impl Launch {
    fn from_steam(game: &str, command: &[OsString]) -> Option<Self> {
        let id = std::env::var("SteamAppId").ok()?;
        if id.parse::<u32>().is_err() || game.strip_prefix("steam-") != Some(&id) {
            return None;
        }
        let compat = PathBuf::from(std::env::var_os("STEAM_COMPAT_DATA_PATH")?);
        if !compat.is_absolute() {
            return None;
        }
        let init = init_command(command)?;
        let proton = Path::new(&init[init.len() - 3]);
        if let Some(paths) = std::env::var_os("STEAM_COMPAT_TOOL_PATHS").filter(|p| !p.is_empty()) {
            let directory = proton.parent()?.canonicalize().ok()?;
            if !std::env::split_paths(&paths)
                .any(|p| p.canonicalize().is_ok_and(|p| p == directory))
            {
                return None;
            }
        }
        Some(Self { compat, init })
    }
    fn prepare(&self, ui: Interface, game: &str) -> Result<PathBuf> {
        let prefix = self.compat.join("pfx");
        let user = prefix.join("drive_c/users/steamuser");
        if !user.is_dir() || !prefix.join("user.reg").is_file() {
            let msg = format!(
                "Steam selected {} for this game. Initialize it with Proton so Buddy can show where the saves will go?",
                self.compat.display()
            );
            ensure!(
                ui.choose(
                    "Set up the Proton prefix",
                    &msg,
                    &["Cancel", "Initialize prefix"]
                )? == 1,
                crate::ui::Cancelled
            );
            if !self.compat.exists() {
                fs::create_dir(&self.compat)
                    .context("the Steam library or prefix parent must be available")?;
            }
            let _progress = crate::ui::Progress::start(ui, game, "Initializing the Proton prefix…");
            let status = Command::new(&self.init[0])
                .args(&self.init[1..])
                .env_remove("BUDDY_INVITE")
                .env_remove("BUDDY_PASSWORD")
                .stdout(Stdio::null())
                .status()
                .context("start Steam's selected Proton tool")?;
            ensure!(
                status.success() && user.is_dir() && prefix.join("user.reg").is_file(),
                "Proton did not initialize {}. Check Steam’s compatibility tool and library, then try again.",
                prefix.display()
            );
        }
        prefix
            .canonicalize()
            .context("resolve the initialized Proton prefix")
    }
}

struct Destination {
    kind: Kind,
    path: PathBuf,
    anchor: PathBuf,
}

impl Destination {
    fn resolve(slot: &Slot, prefix: &Path) -> Result<Self> {
        let user = prefix.join("drive_c/users/steamuser");
        let (anchor, relative) = if let Some(loc) = slot.locations.get("proton") {
            ensure!(
                loc.root == "pfx",
                "configure the Proton root {} in Buddy",
                loc.root
            );
            let rel = Path::new(&loc.relative);
            let docs = Path::new("drive_c/users/steamuser/Documents");
            if let Ok(suffix) = rel.strip_prefix(docs) {
                (user.join("Documents"), suffix)
            } else {
                (prefix.to_owned(), rel)
            }
        } else if let Some(loc) = slot.locations.get("windows") {
            let anchor = match loc.root.as_str() {
                "documents" => user.join("Documents"),
                "data" | "config" => user.join("AppData/Roaming"),
                "home" => user,
                _ => bail!("select this slot manually in Buddy"),
            };
            (anchor, Path::new(&loc.relative))
        } else {
            bail!("select this slot manually in Buddy")
        };
        ensure!(
            relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
                && !relative.as_os_str().is_empty(),
            "choose a game-specific directory or file"
        );
        let anchor = anchor
            .canonicalize()
            .context("required prefix directory unavailable")?;
        let mut path = anchor.clone();
        for part in relative {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    path = path.canonicalize().context("unavailable Proton path")?;
                    ensure!(
                        path.starts_with(&anchor),
                        "redirected save path needs a manual binding"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Self {
            kind: slot.kind.clone(),
            path,
            anchor,
        })
    }
    fn bind(&self) -> Result<Binding> {
        let folder = if self.kind == Kind::File {
            self.path.parent().context("missing destination parent")?
        } else {
            self.path.as_path()
        };
        let relative = folder.strip_prefix(&self.anchor)?;
        let dir = Dir::open_ambient_dir(&self.anchor, ambient_authority())?;
        if !relative.as_os_str().is_empty() {
            dir.create_dir_all(relative)?;
        }
        let binding = Binding::new(&self.path, &self.kind)?;
        ensure!(
            binding.path == self.path,
            "destination changed since approval"
        );
        Ok(binding)
    }
}

// Called under the game lock. Never replace an existing selection or interrupted session.
pub fn first_launch(
    local: &Local,
    api: &Api,
    game: &str,
    command: &[OsString],
    ui: Interface,
    existing: Option<Setup>,
) -> Result<Setup> {
    let launch = Launch::from_steam(game, command);
    let existing = match existing {
        Some(setup)
            if !setup.bindings.is_empty()
                || setup.bindings_cleared
                || setup.base.is_some()
                || setup.draft.is_some()
                || !crate::setup::idle(&setup) =>
        {
            if let (Some(saved), Some(launch)) = (&setup.approved_proton_prefix, &launch) {
                let selected = launch.compat.join("pfx");
                if selected.canonicalize().ok().as_ref() != Some(saved) {
                    let ready = launch.prepare(ui, &setup.recipe.title)?;
                    bail!(
                        "Steam selected {}. Buddy’s approved prefix is {}. Reconnect the original drive or use Buddy to stop syncing the old selections and bind your saves in the new prefix.",
                        ready.display(),
                        saved.display()
                    );
                }
            }
            return Ok(setup);
        }
        other => other,
    };
    let Some(launch) = launch else {
        return existing.context("configure this game on this device first");
    };
    let prefix = launch.prepare(ui, game)?;
    let remote = api.rpc::<Vec<GameInfo>>(Action::Games)?.into_iter().find(|g| g.recipe.game==game)
        .context("Proton is ready. Choose the save paths for this game in Buddy before pressing Play again.")?;
    remote.recipe.validate()?;
    ensure!(
        !remote.recipe.slots.is_empty(),
        "choose a save location for this game in Buddy before playing"
    );
    let mut setup = Setup::new(remote);
    let mut selected = BTreeMap::new();
    for (id, slot) in &setup.recipe.slots {
        let dest = Destination::resolve(slot, &prefix).with_context(|| {
            format!(
                "Proton is ready at {}. Configure {id} in Buddy",
                prefix.display()
            )
        })?;
        ensure!(
            !local.home.starts_with(&dest.path) && !dest.path.starts_with(&local.home),
            "selection overlaps Buddy data"
        );
        ensure!(
            !selected
                .values()
                .any(|v: &Destination| v.path.starts_with(&dest.path)
                    || dest.path.starts_with(&v.path)),
            "overlapping suggestions; pick the paths in Buddy"
        );
        selected.insert(id.to_owned(), dest);
    }
    let paths = selected
        .iter()
        .map(|(id, d)| format!("{id}   {}", d.path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let msg = format!(
        "Use these locations for {} on this device? Buddy creates missing folders and syncs the selected files before starting the game.\n\n{paths}",
        setup.recipe.title
    );
    ensure!(
        ui.choose(
            "Confirm Proton save locations",
            &msg,
            &["Cancel", "Use these locations"]
        )? == 1,
        crate::ui::Cancelled
    );
    for (id, dest) in selected {
        setup.bindings.insert(id, dest.bind()?);
    }
    Files::open(&setup.recipe, &setup.bindings, &local.home)?;
    setup.needs_first_pull = true;
    setup.approved_proton_prefix = Some(prefix);
    local.save(&api.c, &setup)?;
    Ok(setup)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_documents_resolves_the_users_redirect_prior_to_approval() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let pfx = tmp.path().join("pfx");
        let user = pfx.join("drive_c/users/steamuser");
        fs::create_dir_all(&user).unwrap();
        let redirected = tmp.path().join("redirected-documents");
        fs::create_dir(&redirected).unwrap();
        symlink(&redirected, user.join("Documents")).unwrap();
        let mut slot = Slot {
            kind: Kind::Folder,
            settings: false,
            source: None,
            codec: crate::model::raw_codec(),
            ignores: Default::default(),
            locations: BTreeMap::new(),
        };
        slot.locations.insert(
            "windows".into(),
            crate::model::Location {
                root: "documents".into(),
                relative: "My Games/Worlds".into(),
            },
        );
        let destination = Destination::resolve(&slot, &pfx).unwrap();
        assert_eq!(destination.path, redirected.join("My Games/Worlds"));
        assert!(!destination.path.exists());
        symlink(tmp.path(), redirected.join("My Games")).unwrap();
        assert!(Destination::resolve(&slot, &pfx).is_err());
        slot.locations.insert(
            "proton".into(),
            crate::model::Location {
                root: "custom-drive".into(),
                relative: "Worlds".into(),
            },
        );
        assert!(Destination::resolve(&slot, &pfx).is_err());
    }
    #[test]
    fn initializer_excludes_game_and_outer_launchers() {
        let args = [
            "/bin/steam-launch-wrapper",
            "--",
            "/bin/reaper",
            "SteamLaunch",
            "AppId=42",
            "--",
            "/lib/runtime/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/lib/Proton 10/proton",
            "waitforexitandrun",
            "/games/A GAME.exe",
            "--profile",
            "a b",
        ]
        .map(OsString::from);
        assert_eq!(
            init_command(&args).unwrap(),
            [
                "/lib/runtime/_v2-entry-point",
                "--verb=getcompatpath",
                "--",
                "/lib/Proton 10/proton",
                "getcompatpath",
                "/"
            ]
            .map(OsString::from)
        );
        assert!(init_command(&["/games/native".into()]).is_none());
    }
}
