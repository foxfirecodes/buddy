use super::*;
use crate::{host, server};
use std::{fs, net::SocketAddr, path::Path, sync::mpsc, thread, time::Duration};

struct Server {
    handle: axum_server::Handle<SocketAddr>,
    task: Option<thread::JoinHandle<Result<()>>>,
}

impl Server {
    fn start(config: &Path) -> Result<Self> {
        let handle = axum_server::Handle::new();
        let (send, recv) = mpsc::channel();
        let worker_handle = handle.clone();
        let config = config.to_owned();
        let task = thread::spawn(move || {
            server::serve_controlled(&config, worker_handle, |addr| {
                let _ = send.send(addr);
            })
        });
        recv.recv_timeout(Duration::from_secs(10))?;
        Ok(Self {
            handle,
            task: Some(task),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.shutdown();
        self.task.take().unwrap().join().unwrap().unwrap();
    }
}

fn review_base(l: &Local, c: &Credentials, rev: i64) -> Result<ReviewBase> {
    ReviewBase::new(
        l,
        c,
        "sample",
        l.maybe_setup(c, "sample")?.as_ref(),
        &l.named_roots(c, "sample")?,
        Some(rev),
    )
}

fn batch(paths: &[(&str, &Path)]) -> Result<ReviewedSetup> {
    let mut approved = vec![];
    for (slot, path) in paths {
        let preview = Binding::new(path, &Kind::Folder)?;
        approved.push((
            Selection {
                game: "sample".into(),
                title: "Sample game".into(),
                slot: (*slot).into(),
                path: path.to_path_buf(),
                kind: Kind::Folder,
                settings: false,
                source: None,
            },
            crate::files::ReviewedBinding::Existing(preview),
        ));
    }
    Ok(ReviewedSetup {
        ignores: Default::default(),
        title: "Sample game".into(),
        approved,
        removed: vec![],
        shared_recipe: None,
        root_changes: BTreeMap::new(),
    })
}

fn login(local: &Local, pair: &host::Pairing, config: &Path, name: &str) -> Result<Credentials> {
    let c = Api::login_paired(
        &pair.url,
        Login {
            account: name.into(),
            password: "sample-test-password".into(),
            device: "desktop".into(),
            invite: Some(server::invite(config)?),
        },
        Some(pair),
    )?;
    local.set_login(&c, true)?;
    Ok(c)
}

#[test]
fn reviewed_setup_has_one_commit_and_preserves_other_devices_slots() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("host");
    let addr = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let pair = host::initialize(&dir, addr)?;
    let cfg = dir.join("server.toml");
    let live = Server::start(&cfg)?;
    let local = Local::open(Some(tmp.path().join("client")))?;
    let c = login(&local, &pair, &cfg, "first")?;
    let api = Api::new(c.clone())?;
    let worlds = tmp.path().join("worlds");
    let nested = worlds.join("players");
    let players = tmp.path().join("players");
    fs::create_dir_all(&nested)?;
    fs::create_dir(&players)?;
    let base = review_base(&local, &c, 0)?;
    let mut bad = batch(&[("worlds", &worlds), ("players", &nested)])?;
    bad.root_changes
        .insert("library".into(), tmp.path().to_owned());
    assert!(
        save_review(&local, &base, bad)
            .unwrap_err()
            .to_string()
            .contains("overlapping")
    );
    assert!(local.maybe_setup(&c, "sample")?.is_none());
    assert!(local.named_roots(&c, "sample")?.is_empty());
    assert!(api.rpc::<Vec<GameInfo>>(Action::Games)?.is_empty());

    let good = batch(&[("worlds", &worlds), ("players", &players)])?;
    // Replace the second directory after its preview. Its old binding must not authorize the new one.
    fs::rename(&players, tmp.path().join("previous-players"))?;
    fs::create_dir(&players)?;
    assert!(
        save_review(&local, &base, good)
            .unwrap_err()
            .to_string()
            .contains("changed after preview")
    );
    assert!(local.maybe_setup(&c, "sample")?.is_none());
    assert!(api.rpc::<Vec<GameInfo>>(Action::Games)?.is_empty());

    // A process interrupted directly after staging its first enrollment has no approved setup.
    let initial = GameInfo {
        recipe: Recipe {
            game: "sample".into(),
            title: "Sample game".into(),
            retention: 3,
            slots: BTreeMap::new(),
        },
        shared: false,
        revision: 0,
        retired_slots: Default::default(),
    };
    let mut provisional = Setup::new(initial.clone());
    provisional.draft = Some(Draft::new(initial));
    local.save(&c, &provisional)?;
    review(&local, "sample", Interface::Unattended, true)?;
    assert!(local.maybe_setup(&c, "sample")?.is_none());

    let mut good = batch(&[("worlds", &worlds), ("players", &players)])?;
    good.root_changes
        .insert("instance".into(), tmp.path().to_owned());
    // Fail the local transaction after the server has accepted the complete recipe.
    // Restarting the operation must install both bindings without publishing revision 2.
    let db = rusqlite::Connection::open(local.home.join("client.sqlite3"))?;
    db.execute_batch(
        "CREATE TRIGGER fail_install BEFORE INSERT ON state WHEN NEW.k LIKE 'roots/%'
         BEGIN SELECT RAISE(ABORT, 'fixture disk failure'); END;",
    )?;
    assert!(save_review(&local, &base, good).is_err());
    let staged = local.setup(&c, "sample")?;
    assert!(staged.draft.is_some() && staged.bindings.is_empty());
    assert!(local.named_roots(&c, "sample")?.is_empty());
    assert_eq!(api.rpc::<Vec<GameInfo>>(Action::Games)?[0].revision, 1);
    db.execute_batch("DROP TRIGGER fail_install")?;
    review(&local, "sample", Interface::Unattended, false)?;
    let first = local.setup(&c, "sample")?;
    assert_eq!(first.bindings.len(), 2);
    assert!(first.draft.is_none());
    let server_info = api.rpc::<Vec<GameInfo>>(Action::Games)?.pop().unwrap();
    assert_eq!(server_info.revision, 1);
    assert_eq!(server_info.recipe.slots.len(), 2);
    assert_eq!(
        server_info.recipe.slots["worlds"].locations[&runtime("sample")].root,
        "instance"
    );
    assert!(
        save_review(&local, &base, batch(&[])?)
            .unwrap_err()
            .to_string()
            .contains("changed since review")
    );

    // A remote policy edit while the review screen is open must not be overwritten.
    let before_remote_change = review_base(&local, &c, 1)?;
    let mut shared = server_info.recipe;
    shared.retention = 7;
    let version: i64 = api.rpc(Action::SaveRecipe {
        recipe: shared.clone(),
        expected: 1,
        shared: true,
    })?;
    assert_eq!(version, 2);
    assert!(
        save_review(&local, &before_remote_change, batch(&[])?)
            .unwrap_err()
            .to_string()
            .contains("server recipe changed")
    );
    assert_eq!(local.setup(&c, "sample")?.revision, 1);

    let second = Local::open(Some(tmp.path().join("other")))?;
    let second_c = login(&second, &pair, &cfg, "second")?;
    let independent = Api::new(second_c.clone())?;
    let second_base = review_base(&second, &second_c, 0)?;
    let catalog: Vec<GameInfo> = independent.rpc(Action::Catalog)?;
    assert_eq!(catalog.len(), 1);
    assert!(independent.rpc::<Vec<GameInfo>>(Action::Games)?.is_empty());
    second.set_login(&c, true)?;
    assert!(
        save_review(&second, &second_base, batch(&[("worlds", &worlds)])?)
            .unwrap_err()
            .to_string()
            .contains("account or setup changed")
    );
    second.set_login(&second_c, true)?;
    let wrong = ReviewedSetup {
        shared_recipe: Some("obsolete".into()),
        ..batch(&[("worlds", &worlds)])?
    };
    assert!(save_review(&second, &second_base, wrong).is_err());
    assert!(independent.rpc::<Vec<GameInfo>>(Action::Games)?.is_empty());
    let selected = ReviewedSetup {
        shared_recipe: Some(recipe_hash(&shared)?),
        ..batch(&[("worlds", &worlds)])?
    };
    save_review(&second, &second_base, selected)?;
    let imported = second.setup(&second_c, "sample")?;
    assert_eq!(imported.recipe, shared);
    assert_eq!(imported.bindings.len(), 1);
    assert!(!imported.shared);

    // Change a local binding while offline. Players, roots and the last approved recipe
    // stay active until the entire draft can publish on reconnection.
    let base = review_base(&second, &second_c, 1)?;
    let mut change = batch(&[("players", &players)])?;
    change.removed.push("worlds".into());
    change
        .root_changes
        .insert("offline".into(), players.clone());
    drop(live);
    save_review(&second, &base, change)?;
    let queued = second.setup(&second_c, "sample")?;
    assert!(queued.draft.is_some() && queued.bindings.contains_key("worlds"));
    assert!(!queued.bindings.contains_key("players"));
    assert!(second.named_roots(&second_c, "sample")?.is_empty());
    let _resumed = Server::start(&cfg)?;
    review(&second, "sample", Interface::Unattended, false)?;
    let applied = second.setup(&second_c, "sample")?;
    assert!(applied.draft.is_none() && applied.bindings.contains_key("players"));
    assert!(!applied.bindings.contains_key("worlds"));
    assert_eq!(second.named_roots(&second_c, "sample")?["offline"], players);
    assert!(
        independent.rpc::<Vec<GameInfo>>(Action::Games)?[0]
            .recipe
            .slots
            .contains_key("worlds")
    );

    let base = review_base(&second, &second_c, applied.revision)?;
    let mut remove_all = batch(&[])?;
    remove_all.removed.push("players".into());
    save_review(&second, &base, remove_all)?;
    let disabled = second.setup(&second_c, "sample")?;
    assert!(disabled.bindings.is_empty() && disabled.bindings_cleared);
    assert_eq!(disabled.recipe.slots.len(), 2);
    assert!(worlds.is_dir() && players.is_dir());

    let Acquisition::Granted { lease, .. } = independent.rpc(Action::Acquire {
        game: "sample".into(),
        session: "live-game".into(),
        takeover: None,
    })?
    else {
        panic!("expected a free lease")
    };
    let base = review_base(&second, &second_c, disabled.revision)?;
    let edit = batch(&[("season", &worlds)])?;
    save_review(&second, &base, edit)?;
    let pending = second.setup(&second_c, "sample")?;
    assert!(pending.bindings.is_empty() && pending.draft.is_some());
    assert_eq!(
        independent.rpc::<Vec<GameInfo>>(Action::Games)?[0].revision,
        disabled.revision
    );
    independent.rpc::<()>(Action::Release { lease })?;
    review(&second, "sample", Interface::Unattended, false)?;
    let applied = second.setup(&second_c, "sample")?;
    assert_eq!(applied.revision, disabled.revision + 1);
    assert!(applied.draft.is_none() && applied.bindings.contains_key("season"));
    Ok(())
}
