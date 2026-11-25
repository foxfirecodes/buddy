//! Review and apply a single reversible Steam edit. Receipts belong to a local
//! Steam profile. Save the receipt before replacing the file so retries can undo
//! an install even if Buddy exits between the two writes.
use super::{LaunchFields, binary, profiles::Profile, quoted_path, text};
use crate::{local::Local, model::key};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub fn launched_by_steam() -> bool {
    ["SteamAppId", "SteamGameId", "STEAM_COMPAT_APP_ID"]
        .iter()
        .any(|k| std::env::var_os(k).is_some())
}

pub fn running() -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let own = fs::metadata("/proc/self")?.uid();
        for entry in fs::read_dir("/proc")?.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_none_or(|n| n.parse::<u32>().is_err())
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.uid() != own {
                continue;
            }
            if let Ok(name) = fs::read_to_string(entry.path().join("comm"))
                && ["steam", "steamwebhelper", "steam.exe"]
                    .iter()
                    .any(|s| name.trim().eq_ignore_ascii_case(s))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let win = std::env::var_os("SystemRoot").context("Windows system directory unavailable")?;
        let output = std::process::Command::new(Path::new(&win).join("System32/tasklist.exe"))
            .args(["/FO", "CSV", "/NH", "/FI", "IMAGENAME eq steam.exe"])
            .creation_flags(0x08000000)
            .output()?;
        ensure!(
            output.status.success(),
            "Couldn't check whether Steam has exited."
        );
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|s| s.to_ascii_lowercase().starts_with("\"steam.exe\",")))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        bail!("Assisted Steam edits currently support Linux and Windows.");
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Fields {
    Store(Option<String>),
    Shortcut {
        target: String,
        options: Option<String>,
    },
    Absent,
}
impl Fields {
    fn describe(&self) -> String {
        match self {
            Self::Store(options) => format!(
                "Launch options: {}",
                options
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("(empty)")
            ),
            Self::Shortcut { target, options } => format!(
                "Target: {target}\nLaunch options: {}",
                options
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("(empty)")
            ),
            Self::Absent => "No shortcut".into(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Created {
    name: String,
    directory: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Receipt {
    executable: PathBuf,
    app: u32,
    original: Fields,
    installed: Fields,
    created: Option<Created>,
}

pub struct Plan {
    pub summary: String,
    pub changed: bool,
    file: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    key: String,
    prior: Option<Receipt>,
    receipt: Receipt,
    install: bool,
}

fn read_receipt(local: &Local, key: &str) -> Result<Option<Receipt>> {
    Ok(local.read::<Option<Receipt>>(key)?.flatten())
}
fn record_key(profile: &Profile, game: Option<&str>) -> String {
    format!(
        "steam-install/{}/{}",
        profile.id,
        game.map_or_else(|| "buddy".into(), |id| format!("game/{id}"))
    )
}
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let meta = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => other.with_context(|| format!("inspect {}", path.display()))?,
    };
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "expected a regular Steam file at {}",
        path.display()
    );
    Ok(Some(
        fs::read(path).with_context(|| format!("read {}", path.display()))?,
    ))
}
fn wrapper(template: &str, old: Option<&str>) -> Result<String> {
    let old = old.unwrap_or_default();
    ensure!(
        !old.contains("run --game ") || !old.to_ascii_lowercase().contains("buddy"),
        "Launch options already contain a Buddy wrapper. Restore the original launch options in Steam first."
    );
    match old.matches("%command%").count() {
        0 if old.is_empty() => Ok(template.into()),
        0 => Ok(format!("{template} {old}")),
        1 => {
            let (left, right) = old.split_once("%command%").unwrap();
            ensure!(
                left.chars().next_back().is_none_or(char::is_whitespace)
                    && right.chars().next().is_none_or(char::is_whitespace)
                    && plain_command_position(left)
                    && plain_command_position(right),
                "This launch template embeds %command% in another command. Use the manual launch options for this game."
            );
            Ok(format!("{left}{template}{right}"))
        }
        _ => bail!("Multiple %command% placeholders. Use the manual launch options for this game."),
    }
}
// Inserting a quoted path inside someone else's quoted shell script would
// change its meaning. Only wrap a standalone invocation.
fn plain_command_position(prefix: &str) -> bool {
    let mut quote = None;
    let mut chars = prefix.chars();
    let mut word_start = true;
    while let Some(ch) = chars.next() {
        if quote.is_none() && (";&|<>\n\r()`".contains(ch) || (ch == '#' && word_start)) {
            return false;
        }
        word_start = ch.is_whitespace();
        if ch == '\\' && quote != Some('\'') {
            chars.next();
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
        }
    }
    quote.is_none()
}
fn profile_arg(local: &Local, shell: bool) -> Result<String> {
    if local.explicit_home {
        Ok(format!(
            "--profile-dir {} ",
            quoted_path(&local.home, shell)?
        ))
    } else {
        Ok(String::new())
    }
}
fn exe_path(target: &str) -> Result<&str> {
    let path = target.trim();
    let path = path
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(path);
    ensure!(
        !path.is_empty() && !path.contains(['"', '\0', '\r', '\n']),
        "Unrecognized Steam shortcut target. Use the manual launch fields."
    );
    Ok(path)
}
fn short_fields(input: &[u8], shorts: &binary::Shortcuts<'_>, id: u32) -> Result<Fields> {
    let Some(row) = shorts.by_id(input, id) else {
        return Ok(Fields::Absent);
    };
    Ok(Fields::Shortcut {
        target: row
            .string(input, "exe")?
            .context("Steam shortcut has no target")?
            .to_owned(),
        options: row.string(input, "LaunchOptions")?.map(str::to_owned),
    })
}
fn linux_uses_steam_command(profile: &Profile, id: u32, path: &str) -> Result<bool> {
    if !cfg!(target_os = "linux") {
        return Ok(false);
    }
    if path.to_ascii_lowercase().ends_with(".exe") {
        return Ok(true);
    }
    let Some(raw) = read(&profile.root.join("config/config.vdf"))? else {
        return Ok(false);
    };
    let root = keyvalues_parser::parse(std::str::from_utf8(&raw)?.trim_start_matches('\u{feff}'))?
        .into_vdf();
    let mut node = &root.value;
    let id = id.to_string();
    for key in [
        "Software",
        "Valve",
        "Steam",
        "CompatToolMapping",
        &id,
        "name",
    ] {
        let fields = node
            .get_obj()
            .context("Invalid Steam compatibility mapping")?;
        let mut found = fields.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key));
        let Some((_, values)) = found.next() else {
            return Ok(false);
        };
        ensure!(
            found.next().is_none() && values.len() == 1,
            "Ambiguous Steam compatibility mapping"
        );
        node = &values[0];
    }
    Ok(node.get_str().is_some_and(|name| !name.is_empty()))
}
// Steam reports Buddy as the executable of wrapped native shortcuts. Recover the
// original target from the managed invocation, even when Start In points elsewhere.
#[derive(Default)]
pub(crate) struct ShortcutLinks {
    pub original: std::collections::BTreeMap<u32, PathBuf>,
    pub buddy_entries: std::collections::BTreeSet<u32>,
}
pub(crate) fn shortcut_links(steam: &Path, buddy: &Path) -> ShortcutLinks {
    let mut links = ShortcutLinks::default();
    let Ok(users) = fs::read_dir(steam.join("userdata")) else {
        return links;
    };
    for user in users.flatten() {
        let Ok(Some(bytes)) = read(&user.path().join("config/shortcuts.vdf")) else {
            continue;
        };
        let Ok(shorts) = binary::Shortcuts::parse(&bytes) else {
            continue;
        };
        for row in shorts.rows {
            let Some(exe) = row.string(&bytes, "exe").ok().flatten() else {
                continue;
            };
            if !exe_path(exe).is_ok_and(|exe| Path::new(exe) == buddy) {
                continue;
            }
            let Ok(id) = row.id(&bytes) else { continue };
            let opts = row
                .string(&bytes, "LaunchOptions")
                .ok()
                .flatten()
                .unwrap_or_default()
                .trim();
            let cmd = opts
                .strip_prefix("--profile-dir \"")
                .and_then(|s| s.split_once("\" ").map(|(_, cmd)| cmd))
                .unwrap_or(opts);
            if cmd.is_empty() || cmd == "gui" || cmd.strip_prefix("play ").is_some_and(key) {
                links.buddy_entries.insert(id);
                continue;
            }
            let marker = format!("run --game shortcut-{id} -- \"");
            if let Some((_, cmd)) = opts.split_once(&marker)
                && let Some((original, _)) = cmd.split_once('"')
                && let Some(folder) = Path::new(original).parent().filter(|p| p.is_dir())
            {
                links.original.insert(id, folder.to_owned());
            }
        }
    }
    links
}
fn prepare_shortcut(
    local: &Local,
    profile: &Profile,
    game: Option<&str>,
    input: &[u8],
    shorts: &binary::Shortcuts<'_>,
    buddy: &Path,
) -> Result<Receipt> {
    let target = quoted_path(buddy, false)?;
    if let Some(game) = game
        && let Some(s) = game.strip_prefix("shortcut-")
    {
        let app = s.parse::<u32>().context("invalid shortcut ID")?;
        let original = short_fields(input, shorts, app)?;
        let Fields::Shortcut {
            target: old,
            options,
        } = &original
        else {
            bail!("This shortcut is absent from the selected Steam account.")
        };
        let old_path = exe_path(old)?;
        ensure!(
            Path::new(old_path) != buddy,
            "This shortcut already targets Buddy. Choose the game's original shortcut."
        );
        let installed = if linux_uses_steam_command(profile, app, old_path)? {
            let wrapped = format!(
                "{} {}run --game {game} -- %command%",
                quoted_path(buddy, true)?,
                profile_arg(local, true)?
            );
            Fields::Shortcut {
                target: old.clone(),
                options: Some(wrapper(&wrapped, options.as_deref())?),
            }
        } else {
            ensure!(
                !options.as_ref().is_some_and(|s| s.contains("%command%")),
                "This shortcut already uses a Steam command template. Use the manual launch fields."
            );
            let mut launch = format!(
                "{}run --game {game} -- {}",
                profile_arg(local, false)?,
                quoted_path(Path::new(old_path), false)?
            );
            if let Some(s) = options.as_deref().filter(|s| !s.is_empty()) {
                launch.push(' ');
                launch.push_str(s);
            }
            Fields::Shortcut {
                target,
                options: Some(launch),
            }
        };
        return Ok(Receipt {
            executable: buddy.to_owned(),
            app,
            original,
            installed,
            created: None,
        });
    }
    let (name, directory, cmd) = if let Some(game) = game {
        let c = local.credentials()?;
        let launch = local
            .launcher(&c, game)?
            .context("Save the game's executable and working directory in Buddy first.")?;
        let name = local.setup(&c, game)?.recipe.title;
        (
            name,
            launch.directory,
            format!("{}play {game}", profile_arg(local, false)?),
        )
    } else {
        (
            "Buddy".into(),
            buddy
                .parent()
                .context("Buddy has no install directory")?
                .to_owned(),
            format!("{}gui", profile_arg(local, false)?),
        )
    };
    let directory = quoted_path(&directory, false)?;
    for row in &shorts.rows {
        let same_target = row
            .string(input, "exe")?
            .is_some_and(|s| exe_path(s).is_ok_and(|p| Path::new(p) == buddy));
        let opts = row
            .string(input, "LaunchOptions")?
            .unwrap_or_default()
            .trim();
        if same_target && (opts == cmd.trim() || game.is_none() && opts.is_empty()) {
            let app = row.id(input)?;
            let current = short_fields(input, shorts, app)?;
            return Ok(Receipt {
                executable: buddy.to_owned(),
                app,
                original: current.clone(),
                installed: current,
                created: None,
            });
        }
    }
    let sc =
        steamlocate::shortcut::Shortcut::new(0, name.clone(), target.clone(), directory.clone());
    let app = (sc.steam_id() >> 32) as u32;
    ensure!(
        shorts.by_id(input, app).is_none(),
        "Another Steam shortcut uses the same ID. Give the game a different title."
    );
    Ok(Receipt {
        executable: buddy.to_owned(),
        app,
        original: Fields::Absent,
        installed: Fields::Shortcut {
            target,
            options: Some(cmd),
        },
        created: Some(Created { name, directory }),
    })
}

pub fn is_installed(local: &Local, profile: &Profile, game: Option<&str>) -> Result<bool> {
    let record = read_receipt(local, &record_key(profile, game))?;
    let Some(r) = record else { return Ok(false) };
    if matches!(r.original, Fields::Store(_)) {
        let raw = read(&profile.config.join("localconfig.vdf"))?
            .context("Steam localconfig.vdf is missing")?;
        let current = Fields::Store(text::field(std::str::from_utf8(&raw)?, r.app)?.value);
        return Ok(current != r.original);
    }
    let raw =
        read(&profile.config.join("shortcuts.vdf"))?.unwrap_or_else(|| binary::EMPTY.to_owned());
    let shorts = binary::Shortcuts::parse(&raw)?;
    Ok(short_fields(&raw, &shorts, r.app)? != r.original)
}

pub fn plan(local: &Local, profile: &Profile, game: Option<&str>, install: bool) -> Result<Plan> {
    ensure!(game.is_none_or(key), "invalid game ID");
    let key = record_key(profile, game);
    let prior = read_receipt(local, &key)?;
    ensure!(
        install || prior.is_some(),
        "Buddy has no managed integration for this game and Steam account."
    );
    let buddy = std::env::current_exe()?;
    ensure!(
        !install || prior.as_ref().is_none_or(|r| r.executable == buddy),
        "The managed Steam entry points to an earlier Buddy installation. Uninstall its integration, then install it from this copy."
    );
    let (file, before, after, current, receipt) = if let Some(id) =
        game.and_then(|g| g.strip_prefix("steam-"))
    {
        let app = id.parse::<u32>().context("invalid Steam app ID")?;
        let file = profile.config.join("localconfig.vdf");
        let before = read(&file)?.context(
            "Missing Steam localconfig.vdf. Sign in to the selected Steam account first.",
        )?;
        let source = std::str::from_utf8(&before)?;
        let field = text::field(source, app)?;
        let current = Fields::Store(field.value.clone());
        let receipt = if let Some(old) = &prior {
            old.clone()
        } else {
            let game = game.unwrap();
            ensure!(
                steamlocate::SteamDir::from_dir(&profile.root)?
                    .find_app(app)?
                    .is_some(),
                "This game is not installed in the selected Steam library."
            );
            let fields = LaunchFields::new(
                &buddy,
                local.explicit_home.then_some(local.home.as_path()),
                game,
            )?;
            Receipt {
                executable: buddy.clone(),
                app,
                original: current.clone(),
                installed: Fields::Store(Some(wrapper(&fields.options, field.value.as_deref())?)),
                created: None,
            }
        };
        let Fields::Store(options) = (if install {
            &receipt.installed
        } else {
            &receipt.original
        }) else {
            bail!("Steam receipt mismatch")
        };
        let after = field.replace(source, options.as_deref()).into_bytes();
        (file, Some(before), after, current, receipt)
    } else {
        let file = profile.config.join("shortcuts.vdf");
        let before = read(&file)?;
        let source = before.as_deref().unwrap_or(binary::EMPTY);
        let shorts = binary::Shortcuts::parse(source)?;
        let receipt = match &prior {
            Some(old) => old.clone(),
            None => prepare_shortcut(local, profile, game, source, &shorts, &buddy)?,
        };
        let current = short_fields(source, &shorts, receipt.app)?;
        let desired = if install {
            &receipt.installed
        } else {
            &receipt.original
        };
        let row = shorts.by_id(source, receipt.app);
        if !install && let (Some(created), Some(row)) = (&receipt.created, row) {
            ensure!(
                row.string(source, "appname")? == Some(&created.name)
                    && row.string(source, "startdir")? == Some(&created.directory)
                    && !row.has_customizations(source)?,
                "This shortcut has custom Steam settings. Review and remove the shortcut in Steam to keep control of those edits."
            );
        }
        let after = if &current == desired {
            source.to_vec()
        } else {
            match (desired, row) {
                (Fields::Shortcut { target, options }, Some(row)) => row.edit(
                    source,
                    &[
                        ("exe", Some(target.as_str())),
                        ("LaunchOptions", options.as_deref()),
                    ],
                )?,
                (Fields::Shortcut { target, options }, None) => {
                    let created = receipt.created.as_ref().context(
                        "Original shortcut is gone. Restore it in Steam before editing.",
                    )?;
                    shorts.add(
                        source,
                        receipt.app,
                        &created.name,
                        target,
                        options.as_deref().unwrap_or_default(),
                        &created.directory,
                    )?
                }
                (Fields::Absent, Some(row)) => shorts.remove(source, row),
                _ => bail!("Unexpected Steam shortcut state"),
            }
        };
        (file, before, after, current, receipt)
    };
    let desired = if install {
        &receipt.installed
    } else {
        &receipt.original
    };
    let changed = &current != desired || (!install && prior.is_some());
    let drift = prior.is_some() && current != receipt.original && current != receipt.installed;
    // Removing an entire generated shortcut after its target was repurposed could
    // delete another game's entry. Existing game shortcuts only restore two fields.
    ensure!(
        !(drift && receipt.created.is_some()),
        "This Buddy-created shortcut was edited in Steam. Review and remove that shortcut in Steam."
    );
    let summary = if !changed {
        "Already configured.".into()
    } else {
        format!(
            "{} · {}{}\n\nBefore\n{}\n\nAfter\n{}",
            profile.label,
            game.unwrap_or("Buddy"),
            if drift {
                "\nSteam fields changed since installation."
            } else {
                ""
            },
            current.describe(),
            desired.describe()
        )
    };
    Ok(Plan {
        summary,
        changed,
        file,
        before,
        after,
        key,
        prior,
        receipt,
        install,
    })
}

pub fn apply(local: &Local, plan: &Plan) -> Result<()> {
    apply_checked(local, plan, running)
}
fn apply_checked(
    local: &Local,
    plan: &Plan,
    steam_running: impl Fn() -> Result<bool>,
) -> Result<()> {
    ensure!(
        !steam_running()?,
        "Steam is still open. Exit Steam, then apply again."
    );
    if !plan.changed {
        return Ok(());
    }
    let directory = plan
        .file
        .parent()
        .context("missing Steam profile directory")?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("buddy-steam.lock"))?;
    lock.try_lock()
        .context("Another Buddy process is editing this Steam profile")?;
    let prior = read_receipt(local, &plan.key)?;
    ensure!(
        prior == plan.prior,
        "Buddy's Steam setup changed. Reopen the review."
    );
    ensure!(
        read(&plan.file)? == plan.before,
        "Steam configuration changed. Reopen the review."
    );
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    if let Ok(meta) = fs::metadata(&plan.file) {
        temp.as_file().set_permissions(meta.permissions())?;
    }
    temp.write_all(&plan.after)?;
    temp.as_file().sync_all()?;
    if plan.install {
        local.write(&plan.key, &plan.receipt)?;
    }
    ensure!(
        !steam_running()? && read(&plan.file)? == plan.before,
        "Steam or its configuration changed. Reopen the review."
    );
    temp.persist(&plan.file)
        .with_context(|| format!("replace {}", plan.file.display()))?;
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    if !plan.install {
        local.write(&plan.key, &Option::<Receipt>::None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(root: &Path) -> Result<Profile> {
        let config = root.join("userdata/12/config");
        fs::create_dir_all(&config)?;
        Ok(Profile {
            id: "one".into(),
            label: "test".into(),
            root: root.to_owned(),
            config,
        })
    }

    #[test]
    fn text_field_preserves_comments_unicode_unknown_fields_and_backslashes() -> Result<()> {
        let source = r#""UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" { "93" { "other" "café" "launchoptions" // keep my comment
                   "env MESA=\"A B\" %command% C:\\User" } "99" { "LastPlayed" "152" } } } } } }"#;
        let field = text::field(source, 93)?;
        let original = field.value.clone().unwrap();
        assert_eq!(original, r#"env MESA="A B" %command% C:\User"#);
        let after = field.replace(source, Some("wrapped"));
        assert!(after.contains("\"launchoptions\" // keep my comment"));
        assert_eq!(
            text::field(&after, 93)?.replace(&after, Some(&original)),
            source
        );
        let src = "\"UserLocalConfigStore\"\r\n{\r\n// no apps yet\r\n}";
        let added = text::field(src, 42)?.replace(src, Some("new"));
        assert!(added.contains("// no apps yet") && added.contains("\r\n"));
        assert_eq!(text::field(&added, 42)?.value.as_deref(), Some("new"));
        let removed = text::field(&added, 42)?.replace(&added, None);
        assert_eq!(text::field(&removed, 42)?.value, None);
        Ok(())
    }
    #[test]
    fn wrappers_keep_arguments_and_review_ambiguous_templates() -> Result<()> {
        assert_eq!(
            wrapper("buddy -- %command%", Some("mangohud %command% -x"))?,
            "mangohud buddy -- %command% -x"
        );
        assert_eq!(
            wrapper("buddy -- %command%", Some("-x \"two words\""))?,
            "buddy -- %command% -x \"two words\""
        );
        for old in [
            "sh -c 'go %command% fast'",
            "%command% && %command%",
            "\"%command%\"",
            "prefix%command%",
            "# developer note %command%",
            "gamescope -- %command% ; echo done",
            "echo one | %command%",
        ] {
            assert!(wrapper("buddy -- %command%", Some(old)).is_err());
        }
        Ok(())
    }
    #[test]
    fn store_install_roundtrips_with_second_library_and_preserves_later_edits() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let p = profile(&temp.path().join("Steam"))?;
        let local = Local::open(Some(temp.path().join("data")))?;
        let other = temp.path().join("another library");
        fs::create_dir_all(other.join("steamapps"))?;
        fs::create_dir_all(p.root.join("steamapps"))?;
        fs::write(
            p.root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                p.root.display(),
                other.display()
            ),
        )?;
        fs::write(
            other.join("steamapps/appmanifest_93.acf"),
            r#""AppState" { "appid" "93" "name" "Example" "installdir" "Example" }"#,
        )?;
        let path = p.config.join("localconfig.vdf");
        let initial = r#""UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" { "93" { "LaunchOptions" "mangohud %command% -mods" } } } } } }"#;
        fs::write(&path, initial)?;
        let g = Some("steam-93");
        let on = plan(&local, &p, g, true)?;
        assert!(apply_checked(&local, &on, || Ok(true)).is_err());
        assert_eq!(fs::read_to_string(&path)?, initial);
        apply_checked(&local, &on, || Ok(false))?;
        let wrapped = fs::read_to_string(&path)?;
        assert!(
            text::field(&wrapped, 93)?
                .value
                .unwrap()
                .contains("%command% -mods")
        );
        assert!(!plan(&local, &p, g, true)?.changed);
        let off = plan(&local, &p, g, false)?;
        fs::write(&path, format!("{wrapped}\n// an unrelated Steam update"))?;
        assert!(apply_checked(&local, &off, || Ok(false)).is_err());
        let off = plan(&local, &p, g, false)?;
        apply_checked(&local, &off, || Ok(false))?;
        assert_eq!(
            fs::read_to_string(&path)?,
            format!("{initial}\n// an unrelated Steam update")
        );
        assert!(!is_installed(&local, &p, g)?);
        Ok(())
    }
    #[test]
    fn native_shortcut_keeps_its_appid_tags_start_in_and_original_install_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let p = profile(&temp.path().join("Steam"))?;
        let local = Local::open(Some(temp.path().join("data")))?;
        let dir = temp.path().join("My game");
        fs::create_dir(&dir)?;
        let target = quoted_path(&dir.join("OpenStarbound"), false)?;
        let path = p.config.join("shortcuts.vdf");
        let raw = binary::Shortcuts::parse(binary::EMPTY)?.add(
            binary::EMPTY,
            52,
            "OpenStarbound",
            &target,
            "--mods \"my mods\"",
            "another cwd",
        )?;
        let blank = b"\x00tags\x00\x08";
        let at = raw.windows(blank.len()).position(|v| v == blank).unwrap() + blank.len() - 1;
        let mut initial = raw;
        initial.splice(at..at, b"\x010\x00Favorites\x00".iter().copied());
        fs::write(&path, &initial)?;
        let g = Some("shortcut-52");
        let mut on = plan(&local, &p, g, true)?;
        // Closing between writing the durable receipt and replacing the file must be retryable.
        local.write(&on.key, &on.receipt)?;
        on = plan(&local, &p, g, true)?;
        apply_checked(&local, &on, || Ok(false))?;
        let wrapped = fs::read(&path)?;
        let parsed = binary::Shortcuts::parse(&wrapped)?;
        let row = parsed.by_id(&wrapped, 52).unwrap();
        assert_eq!(row.string(&wrapped, "startdir")?, Some("another cwd"));
        assert!(
            row.string(&wrapped, "launchoptions")?
                .unwrap()
                .ends_with("--mods \"my mods\"")
        );
        assert!(wrapped.windows(9).any(|w| w == b"Favorites"));
        assert_eq!(
            shortcut_links(&p.root, &std::env::current_exe()?)
                .original
                .get(&52),
            Some(&dir)
        );
        let remove = plan(&local, &p, g, false)?;
        apply_checked(&local, &remove, || Ok(false))?;
        assert_eq!(fs::read(&path)?, initial);
        Ok(())
    }
    #[test]
    fn buddy_shortcut_is_idempotent_hides_from_game_list_and_keeps_user_customizations()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let p = profile(&temp.path().join("Steam"))?;
        let local = Local::open(Some(temp.path().join("data")))?;
        let on = plan(&local, &p, None, true)?;
        let app = on.receipt.app;
        apply_checked(&local, &on, || Ok(false))?;
        assert!(is_installed(&local, &p, None)?);
        assert!(
            shortcut_links(&p.root, &std::env::current_exe()?)
                .buddy_entries
                .contains(&app)
        );
        assert!(!plan(&local, &p, None, true)?.changed);
        let path = p.config.join("shortcuts.vdf");
        let raw = fs::read(&path)?;
        let parsed = binary::Shortcuts::parse(&raw)?;
        let changed = parsed
            .by_id(&raw, app)
            .unwrap()
            .edit(&raw, &[("icon", Some("my icon"))])?;
        fs::write(&path, &changed)?;
        assert!(plan(&local, &p, None, false).is_err());
        assert_eq!(fs::read(&path)?, changed);
        fs::write(&path, &raw)?;
        apply_checked(&local, &plan(&local, &p, None, false)?, || Ok(false))?;
        assert_eq!(fs::read(path)?, binary::EMPTY);
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn proton_shortcut_preserves_steam_runtime_and_prefix_identity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let p = profile(&temp.path().join("Steam"))?;
        let local = Local::open(Some(temp.path().join("data")))?;
        let input = binary::Shortcuts::parse(binary::EMPTY)?.add(
            binary::EMPTY,
            832,
            "Game",
            "\"/mnt/game.exe\"",
            "%command% -x",
            "/mnt",
        )?;
        fs::write(p.config.join("shortcuts.vdf"), input)?;
        let on = plan(&local, &p, Some("shortcut-832"), true)?;
        let shorts = binary::Shortcuts::parse(&on.after)?;
        let row = shorts.by_id(&on.after, 832).unwrap();
        assert_eq!(row.string(&on.after, "exe")?, Some("\"/mnt/game.exe\""));
        assert!(
            row.string(&on.after, "launchoptions")?
                .unwrap()
                .ends_with("run --game shortcut-832 -- %command% -x")
        );
        Ok(())
    }
}
