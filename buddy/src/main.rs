mod files;
mod index;
mod local;
mod manifest;
mod model;
mod pack;
mod server;
mod session;
mod setup;
mod steam_launch;
mod ui;
use anyhow::Result;
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
    /// Open the interactive terminal client.
    Tui,
    #[command(hide = true, name = "_decision")]
    Decision,
    #[command(hide = true, name = "_progress")]
    Progress,
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
    /// Reclaim unreferenced server chunks after the grace period.
    Gc {
        #[arg(long, default_value = "server.toml")]
        config: PathBuf,
    },
    /// Register or sign this device in.
    Login {
        url: String,
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
    /// List explicitly shared file recipes.
    Catalog,
    /// Index a local Ludusavi YAML catalog for path suggestions.
    LoadManifest { path: PathBuf },
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
fn options() -> RunOptions {
    RunOptions {
        command: vec![],
        manual: false,
        restore: None,
        pin: false,
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
    let window = matches!(&args.command, None | Some(Cmd::Gui | Cmd::Run { .. }));
    let code = match dispatch(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Buddy: {e:#}");
            if window {
                let _ = ui::choose("Buddy needs attention", &format!("{e:#}"), &["Close"]);
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
        Cmd::Serve { config } => {
            server::serve(&config)?;
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
                Cmd::Tui => ui::tui(&l)?,
                Cmd::Login {
                    url,
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
                    l.set_login(
                        &Api::login(
                            &url,
                            Login {
                                account,
                                password,
                                device: device.unwrap_or_else(local::default_device),
                                invite,
                            },
                        )?,
                        private_file_credential,
                    )?;
                }
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
                            r.id, r.title, r.installed, r.local, r.shared
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
