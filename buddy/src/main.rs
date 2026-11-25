mod distribution;
mod files;
mod host;
mod index;
mod local;
mod manifest;
mod model;
mod pack;
mod phase;
#[cfg(target_os = "linux")]
mod proton;
mod server;
mod session;
mod setup;
mod steam_launch;
mod ui;
mod update;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use local::{Api, Local};
use model::*;
use session::{Interface, RunOptions, synchronize};
use std::{
    ffi::OsString,
    io::{self, BufRead},
    path::PathBuf,
};

#[derive(Parser)]
#[command(name = "buddy", version, about = "Sync game saves across your devices")]
struct Args {
    /// Use a separate client profile (also available through BUDDY_HOME).
    #[arg(long, global = true, env = "BUDDY_HOME", value_name = "DIR")]
    profile_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}
#[derive(Subcommand)]
enum Cmd {
    /// Open the graphical client.
    Gui,
    /// Set up or run a private HTTPS Buddy server in a window.
    Host,
    /// Open the interactive terminal client.
    Tui,
    #[command(hide = true, name = "_decision")]
    Decision,
    #[command(hide = true, name = "_progress")]
    Progress,
    /// Copy the default server.toml to the current directory.
    InitServer,
    /// Start the control server.
    Serve {
        #[arg(long, default_value = "server.toml")]
        config: PathBuf,
    },
    /// Create a single-use registration invite.
    Invite {
        #[arg(long, default_value = "server.toml")]
        config: PathBuf,
    },
    /// Publish a native Buddy binary for devices installing or updating from this server.
    PublishRelease {
        #[arg(long, default_value = "server.toml")]
        config: PathBuf,
        #[arg(value_parser = ["linux-x86_64", "windows-x86_64"])]
        platform: String,
        binary: PathBuf,
    },
    /// Generate a one-time command to install Buddy and pair another device to your account.
    InstallCommand {
        #[arg(value_parser = ["linux-x86_64", "windows-x86_64"])]
        platform: String,
        /// First-time release key from the server operator.
        #[arg(long)]
        trust_key: Option<String>,
    },
    /// Update this executable to the build published by your server.
    Update {
        /// First-time release key from the server operator.
        #[arg(long)]
        trust_key: Option<String>,
    },
    #[command(hide = true)]
    RedeemDevice {
        #[arg(long, env = "BUDDY_ALLOW_FILE_CREDENTIAL", action = clap::ArgAction::SetTrue)]
        private_file_credential: bool,
    },
    /// Offer to add the installed Buddy executable to Steam.
    FinishInstall,
    /// Install or remove Buddy integration on this Steam account.
    Steam {
        #[command(subcommand)]
        action: SteamAction,
    },
    /// Reclaim unreferenced server chunks after the grace period.
    Gc {
        #[arg(long, default_value = "server.toml")]
        config: PathBuf,
    },
    /// Register or sign this device in.
    Login {
        url: String,
        /// Trust the host certificate in this server pairing file.
        #[arg(long)]
        pairing: Option<PathBuf>,
        account: String,
        #[arg(long)]
        device: Option<String>,
        #[arg(long, env = "BUDDY_INVITE", hide_env_values = true)]
        invite: Option<String>,
        #[arg(long)]
        password_stdin: bool,
        #[arg(long,env="BUDDY_ALLOW_FILE_CREDENTIAL",action=clap::ArgAction::SetTrue)]
        private_file_credential: bool,
    },
    /// Reclaim local chunks outside active or pending snapshots.
    CacheGc,
    /// Publish reviewed changes to a cached recipe.
    ApplySetup {
        game: String,
        #[arg(long)]
        interactive: bool,
    },
    /// Discard unpublished recipe edits on this device.
    DiscardSetup { game: String },
    /// Roll back an interrupted local restore.
    Rollback { game: String },
    /// Map a portable root name to a directory on this device.
    Root {
        game: String,
        name: String,
        path: PathBuf,
    },
    /// List installed and configured games.
    Games,
    /// Stop syncing one selection on this device.
    Unbind { game: String, slot: String },
    /// Remove a location from this account's recipe on every device. Keeps local files.
    RemoveSlot { game: String, slot: String },
    /// List explicitly shared file recipes.
    Catalog,
    /// Index a local Ludusavi YAML catalog for path suggestions.
    LoadManifest { path: PathBuf },
    /// Download or refresh path suggestions directly from the upstream Ludusavi manifest.
    FetchManifest,
    /// Show detected file candidates for a Steam game.
    Suggestions { game: String },
    /// Bind a file or directory to a logical selection.
    Setup {
        game: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value = "saves")]
        slot: String,
        #[arg(long)]
        file: bool,
        #[arg(long)]
        settings: bool,
        path: PathBuf,
    },
    /// Change retention and recipe sharing.
    Policy {
        game: String,
        #[arg(long)]
        retention: Option<u32>,
        #[arg(long)]
        shared: Option<bool>,
    },
    /// Copy a selected shared recipe to your account.
    Import { game: String, recipe_hash: String },
    /// Inspect local bindings and unfinished work.
    Status { game: String },
    /// Reconcile selected files with the server.
    Sync {
        game: String,
        #[arg(long)]
        interactive: bool,
    },
    /// Launch the saved command for a manually configured game through Buddy.
    Play { game: String },
    /// Sync, launch the supplied game command, and capture its session.
    Run {
        #[arg(long)]
        game: Option<String>,
        #[arg(long)]
        manual_exit: bool,
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
    /// List saved versions and permanent backups.
    Backups { game: String },
    /// Sync current files and pin a permanent backup.
    Backup { game: String },
    /// Keep an existing revision indefinitely.
    Pin { game: String, id: i64 },
    /// Restore a revision and publish it as current.
    Restore {
        game: String,
        id: i64,
        #[arg(long)]
        interactive: bool,
    },
    /// Delete a historical backup.
    DeleteBackup { game: String, id: i64 },
    /// Put the pinned current revision back on its rolling retention schedule.
    UnpinCurrent { game: String, id: i64 },
    /// List devices signed in to your account.
    Devices,
    /// Revoke a device token.
    Revoke { device: String },
}
#[derive(clap::Args)]
struct SteamEdit {
    /// Steam app, shortcut, or custom game ID. Omit to manage Buddy's own shortcut.
    game: Option<String>,
    /// Profile ID as shown by buddy steam profiles.
    #[arg(long)]
    steam_profile: Option<String>,
    /// Apply without interactive confirmation, after Steam exits.
    #[arg(long)]
    yes: bool,
}
#[derive(Subcommand)]
enum SteamAction {
    /// List Steam accounts on this device.
    Profiles,
    /// Add the integration and remember the previous launch fields.
    Install(SteamEdit),
    /// Restore the original launch fields or remove the Buddy-created shortcut.
    Uninstall(SteamEdit),
}
fn options() -> RunOptions {
    RunOptions {
        command: vec![],
        manual: false,
        restore: None,
        pin: false,
        expected_scope: None,
    }
}
fn cli_decisions(interactive: bool) -> Interface {
    if interactive {
        Interface::Terminal
    } else {
        Interface::Unattended
    }
}
fn main() {
    let args = Args::parse();
    let window = matches!(
        &args.command,
        None | Some(Cmd::Gui | Cmd::Host | Cmd::Play { .. } | Cmd::Run { .. })
    );
    let code = match dispatch(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Buddy: {e:#}");
            if window && !e.is::<ui::Cancelled>() {
                let _ = ui::choose_with_details(
                    "Buddy needs attention",
                    &ui::error_summary(&e),
                    &["Close"],
                    &format!("{e:#}"),
                );
            }
            e.downcast_ref::<session::PostGame>().map_or(1, |v| v.0)
        }
    };
    std::process::exit(code);
}
fn dispatch(args: Args) -> Result<i32> {
    match args.command.unwrap_or(Cmd::Gui) {
        Cmd::Decision => return ui::decision_window(),
        Cmd::Progress => {
            ui::progress_window()?;
            return Ok(0);
        }
        Cmd::InitServer => {
            server::init_config()?;
            println!("Created server.toml");
            return Ok(0);
        }
        Cmd::Serve { config } => {
            server::serve(&config)?;
            return Ok(0);
        }
        Cmd::PublishRelease {
            config,
            platform,
            binary,
        } => {
            println!("{}", server::publish_release(&config, &platform, &binary)?);
            return Ok(0);
        }
        Cmd::Invite { config } => {
            println!("{}", server::invite(&config)?);
            return Ok(0);
        }
        Cmd::Gc { config } => {
            println!("Removed {} unused chunks", server::gc(&config)?);
            return Ok(0);
        }
        cmd => {
            let l = Local::open(args.profile_dir)?;
            match cmd {
                Cmd::Gui => ui::gui(l)?,
                Cmd::Host => host::gui(l)?,
                Cmd::Tui => ui::tui(&l)?,
                Cmd::FinishInstall => ui::finish_install(l)?,
                Cmd::InstallCommand {
                    platform,
                    trust_key,
                } => {
                    println!(
                        "{}",
                        distribution::install_command(
                            &l,
                            &l.credentials()?,
                            &platform,
                            trust_key.as_deref()
                        )?
                    );
                }
                Cmd::RedeemDevice {
                    private_file_credential,
                } => {
                    distribution::redeem_stdin(&l, private_file_credential)?;
                }
                Cmd::Update { trust_key } => {
                    match update::run(&l, &l.credentials()?, trust_key.as_deref(), || Ok(true))? {
                        update::Outcome::Current => println!("Buddy is up to date"),
                        update::Outcome::Updated(path) => println!("Updated {}", path.display()),
                        update::Outcome::Cancelled => {}
                    }
                }
                Cmd::Steam { action } => {
                    let (edit, install) = match action {
                        SteamAction::Profiles => {
                            ui::list_profiles()?;
                            return Ok(0);
                        }
                        SteamAction::Install(edit) => (edit, true),
                        SteamAction::Uninstall(edit) => (edit, false),
                    };
                    println!(
                        "{}",
                        ui::steam(
                            &l,
                            edit.game.as_deref(),
                            Some(install),
                            edit.steam_profile.as_deref(),
                            Interface::Terminal,
                            edit.yes
                        )?
                    );
                }
                Cmd::Play { game } => {
                    let c = l.credentials()?;
                    let launcher = l.launcher(&c, &game)?.context(
                        "choose and save a local launcher in this game's Settings first",
                    )?;
                    // This process belongs to this launch; keep the GUI's working directory intact.
                    std::env::set_current_dir(&launcher.directory)
                        .context("open game working directory")?;
                    let mut command = vec![launcher.executable.into_os_string()];
                    command.extend(launcher.args.into_iter().map(OsString::from));
                    return synchronize(
                        &l,
                        &game,
                        RunOptions {
                            command,
                            manual: launcher.manual_exit,
                            expected_scope: Some(l.scope(&c)),
                            ..options()
                        },
                        Interface::Window,
                    );
                }

                Cmd::Login {
                    url,
                    pairing,
                    account,
                    device,
                    invite,
                    password_stdin,
                    private_file_credential,
                } => {
                    let password = if password_stdin {
                        let mut p = String::new();
                        io::stdin().lock().read_line(&mut p)?;
                        p.trim_end_matches(['\n', '\r']).to_owned()
                    } else {
                        inquire::Password::new("Password")
                            .without_confirmation()
                            .prompt()?
                    };
                    let pair = pairing.as_deref().map(host::Pairing::load).transpose()?;
                    l.set_login(
                        &Api::login_paired(
                            &url,
                            Login {
                                account,
                                password,
                                device: device.unwrap_or_else(local::default_device),
                                invite,
                            },
                            pair.as_ref(),
                        )?,
                        private_file_credential,
                    )?;
                }
                Cmd::FetchManifest => println!("{}", manifest::download(&l)?),
                Cmd::LoadManifest { path } => {
                    println!("Indexed {} games", manifest::import(&l, &path)?)
                }
                Cmd::Suggestions { game } => {
                    for s in manifest::candidates(&l, &game)? {
                        println!(
                            "{}\t{}\t{}",
                            if s.settings { "settings" } else { "save" },
                            s.path.display(),
                            s.source
                        );
                    }
                }
                Cmd::Root { game, name, path } => l.bind_root(&game, &name, &path)?,
                Cmd::Unbind { game, slot } => setup::unbind(&l, &game, &slot)?,
                Cmd::RemoveSlot { game, slot } => {
                    let c = l.credentials()?;
                    let games: Vec<GameInfo> = Api::new(c.clone())?.rpc(Action::Games)?;
                    let remote = games
                        .iter()
                        .find(|g| g.recipe.game == game)
                        .ok_or_else(|| anyhow::anyhow!("game has no recipe on this account"))?;
                    let base = setup::ReviewBase::new(
                        &l,
                        &c,
                        &game,
                        l.maybe_setup(&c, &game)?.as_ref(),
                        &l.named_roots(&c, &game)?,
                        Some(remote.revision),
                    )?;
                    setup::remove_slot(&l, &base, &slot)?;
                }
                Cmd::ApplySetup { game, interactive } => {
                    setup::review(&l, &game, cli_decisions(interactive), false)?
                }
                Cmd::DiscardSetup { game } => {
                    setup::review(&l, &game, Interface::Unattended, true)?
                }
                Cmd::Rollback { game } => session::rollback(&l, &game)?,
                Cmd::CacheGc => println!("Removed {} unused local chunks", l.clean_cache()?),
                Cmd::Games => {
                    for r in setup::games(&l)? {
                        println!(
                            "{}\t{}\tinstalled={} configured={} shared={}",
                            r.id,
                            r.title,
                            r.installed,
                            r.device == setup::DeviceState::Ready,
                            r.shared
                        );
                    }
                }
                Cmd::Catalog => {
                    let list: Vec<GameInfo> = Api::new(l.credentials()?)?.rpc(Action::Catalog)?;
                    for i in list {
                        println!(
                            "{}\t{}\t{}",
                            i.recipe.game,
                            i.recipe.title,
                            setup::recipe_hash(&i.recipe)?
                        );
                    }
                }
                Cmd::Setup {
                    game,
                    title,
                    slot,
                    file,
                    settings,
                    path,
                } => setup::bind(
                    &l,
                    setup::Selection {
                        title: title.unwrap_or_else(|| game.clone()),
                        game,
                        slot,
                        kind: if file { Kind::File } else { Kind::Folder },
                        settings,
                        path,
                        source: None,
                    },
                    None,
                )?,
                Cmd::Policy {
                    game,
                    retention,
                    shared,
                } => setup::policy(&l, &game, retention, shared)?,
                Cmd::Import { game, recipe_hash } => setup::import(&l, &game, &recipe_hash)?,
                Cmd::Status { game } => {
                    let s = l.setup(&l.credentials()?, &game)?;
                    if s.draft.is_some() {
                        println!("Recipe edits queued on this device");
                    }
                    println!(
                        "{} · {} queued snapshots · session open={} · restore pending={}",
                        s.recipe.title,
                        s.pending.len(),
                        s.active,
                        s.restore.is_some()
                    );
                    for (k, b) in s.bindings {
                        println!("{k}: {}", b.path.display());
                    }
                }
                Cmd::Run {
                    game,
                    manual_exit,
                    command,
                } => {
                    return synchronize(
                        &l,
                        &setup::inferred_game(game)?,
                        RunOptions {
                            command,
                            manual: manual_exit,
                            ..options()
                        },
                        Interface::Window,
                    );
                }
                Cmd::Sync { game, interactive } => {
                    return synchronize(&l, &game, options(), cli_decisions(interactive));
                }
                Cmd::Backup { game } => {
                    return synchronize(
                        &l,
                        &game,
                        RunOptions {
                            pin: true,
                            ..options()
                        },
                        Interface::Unattended,
                    );
                }
                Cmd::Restore {
                    game,
                    id,
                    interactive,
                } => {
                    return synchronize(
                        &l,
                        &game,
                        RunOptions {
                            restore: Some(id),
                            ..options()
                        },
                        cli_decisions(interactive),
                    );
                }
                Cmd::Backups { game } => {
                    let list: Vec<Backup> =
                        Api::new(l.credentials()?)?.rpc(Action::Backups { game })?;
                    for b in list {
                        println!(
                            "#{}\t{}\t{}\t{} {}",
                            b.id,
                            b.origin,
                            when(b.time),
                            if b.head { "current" } else { "" },
                            b.reason
                        );
                    }
                }
                Cmd::Pin { game, id } => {
                    Api::new(l.credentials()?)?.rpc::<()>(Action::Pin { game, id })?
                }
                Cmd::DeleteBackup { game, id } => {
                    Api::new(l.credentials()?)?.rpc::<()>(Action::Delete { game, id })?
                }
                Cmd::UnpinCurrent { game, id } => {
                    Api::new(l.credentials()?)?.rpc::<()>(Action::UnpinCurrent { game, id })?
                }
                Cmd::Devices => {
                    let devices: Vec<(String, String)> =
                        Api::new(l.credentials()?)?.rpc(Action::Devices)?;
                    for (id, name) in devices {
                        println!("{id}\t{name}");
                    }
                }
                Cmd::Revoke { device } => {
                    Api::new(l.credentials()?)?.rpc::<()>(Action::Revoke { device })?
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(0)
}
