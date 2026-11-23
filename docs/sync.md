# Test Buddy sync

Use a disposable world for the first run. Buddy can replace selected files on a pull and propagate deletions on a push. A folder selection includes its whole tree. For a game with its own cloud sync, use a test copy or disable that game's other sync writer on both devices for the trial.

## Start a server and sign in

This workspace also contains ready-built x86-64 executables. Use `target/release/buddy` on this desktop, `target/x86_64-unknown-linux-gnu/release/buddy` for the portable Linux build, and `target/x86_64-pc-windows-gnullvm/release/buddy.exe` on Windows. For the Deck sync trial, copy the portable Linux file to `~/Downloads/buddy`, switch to Desktop Mode and run `install -Dm755 ~/Downloads/buddy ~/.local/bin/buddy` followed by `~/.local/bin/buddy`. It targets glibc 2.28. The Game Mode exit experiment uses the separate `buddy-probe` binary.

For graphical hosting on your home network, choose **Host server…** in Buddy and follow the [pairing walkthrough](onboarding.md). For the headless loopback setup, run these commands in a terminal. The invite printed by the second command is valid for one new account for 24 hours.

```sh
cargo build --release --locked -p buddy
target/release/buddy invite --config server.example.toml
target/release/buddy serve --config server.example.toml
```

Keep the server terminal open. In another terminal run `target/release/buddy`. Enter `http://127.0.0.1:7341`, an account name, a password, and a device name such as **Desktop**. Enable **Create account with an invite** and enter the invite once. Sign the second device into the same account with its own device name. Use another invite for another player's private saves.

The shipped TOML puts the database, including file chunks, under `buddy-data/` next to the settings file. To use another location, copy the TOML, edit `database`, and pass that file to both commands. Normal sign-in uses the OS credential vault. If the device lacks an unlocked vault, **Credential storage** offers an explicit private-file option. On a shared PC, use separate OS accounts for different players. A local OS user able to read the server database can read its saves.

With the headless loopback configuration, you can put a TLS reverse proxy in front of the listener or use an SSH tunnel. The graphical host serves HTTPS directly. With SSH access to the desktop, run this on the second machine and keep it open:

```sh
ssh -N -L 127.0.0.1:7341:127.0.0.1:7341 YOUR_DESKTOP_USER@YOUR_DESKTOP_HOST
```

Both clients can then use `http://127.0.0.1:7341`. Stick with the same URL on each device across sessions. Buddy scopes the local cache to the server URL and account. The server needs no web frontend. A reverse proxy must allow request bodies up to 32 MiB to accommodate large manifests.

## Try a handoff

1. On the desktop, create a folder named `Buddy test world` outside Buddy's data directory. Put a text file with some recognizable progress in it. In Buddy choose **Add non-Steam game**, give it a title, select **Folder**, browse to the folder and **Save selection**. Click **Sync now**.
2. On the second machine, create an empty test folder. Sign in, refresh the games list, select the **same server game**, and configure its existing `saves` selection to point at this folder. Click **Sync now** and check the file's contents. Creating a second non-Steam game would give it a different ID.
3. Edit the file on the second device, sync there, then sync the desktop. Try leaving both devices at the same baseline, making different edits on each, then syncing first one and then the other. The second sync shows both device names and capture/commit times. Both versions are already pinned in **Backups** when the choice appears.
4. For a Steam game, search by title or app ID, select the save files, then **Copy launch options** into Steam's **Properties → General → Launch Options**. A typical entry is `"/absolute/path/to/buddy" run --game steam-12345 -- %command%`. Keep any existing wrapper or game flags with the original command. On the other device copy that device's launch options. The file chooser offers home, Steam install and existing Proton prefix shortcuts. Discovery visits registered libraries across drives.
5. For a non-Steam shortcut inside Steam, expand **Launch as a non-Steam shortcut**. Copy the new Target and the Launch Options prefix. Append the original quoted executable and its arguments to the prefix, and preserve the original Start In directory. For a non-Steam game outside Steam, open **Advanced → Launch a game directly**, choose the executable, working directory, and arguments, then **Save launcher**. Use **Play** in Buddy. These launcher details stay on that device. A launch that hands off to an already-running process can use **Ask me when the game has closed**.
6. To test offline play, close the game, stop the server or tunnel and use the configured launch command. Choose **Play offline**, change the test save and quit. Restart the connection and launch or sync again. The client compares its queued session against the server. **Decide later** keeps the data available for another attempt.

For a freshly installed Proton title with saves already on the server, Buddy can initialize its prefix and restore your selections on the first Play. [First Play with Proton](proton-first-play.md)

Each selection has an ID such as `saves` or `settings` that maps to a path on each device. For a second selection, choose a new ID. A platform-specific settings file can have a separate ID that only the relevant devices bind. **Stop syncing here** detaches a selection on that device and preserves the files and server history. Custom named roots such as `instance` can point to unrelated absolute directories on each device. Define the root before selecting its files so the recipe can record the relative path. Directories must exist when bound. You can bind an absent single file if its parent exists.

**Advanced** contains recipe sharing and backup retention. Publishing a recipe lets others on that server discover its path rules. Save bytes, histories and lease ownership belong to the account. **Back up now** creates a non-expiring backup of the closed game's current files. **Keep** pins an existing revision. Restore and delete are available in **Backups**. The default rolling history keeps three previous distinct commits in addition to the current one. Conflict backups and manual pins survive that rotation.

## Terminal equivalents

Recipe edits to an enrolled game can be queued offline. Buddy continues offline play with its last approved setup. On reconnect it merges independent recipe fields and asks about competing edits. If a game session or restore is pending, finish that sync first. Use **Apply edits…** or `buddy apply-setup GAME --interactive` to review a queued draft. **Discard edits…** drops it.

Use `buddy --profile-dir DIR` to select a separate client profile for a test server. The graphical launcher and copied Steam options carry that choice through.

The same executable provides `buddy tui` and `buddy --help`. These shell examples assume it is on your PATH and you have already signed in.

```sh
buddy games
buddy root minecraft instance "/path/to/instance"
buddy setup minecraft --title Minecraft "/path/to/instance/saves"
buddy setup minecraft --slot settings --file --settings "/path/to/instance/options.txt"
buddy sync minecraft --interactive
buddy run --game minecraft -- "/path/to/launcher" "one argument with spaces"
buddy status minecraft
buddy backups minecraft
buddy backup minecraft
buddy policy minecraft --retention 3 --shared false
buddy unbind minecraft settings
```

Use `buddy sync GAME --interactive` for terminal conflict choices. The default CLI sync stops if a human decision is required. Steam's `run` always opens graphical decisions. Choose **Use shared recipe** in the game view to copy a published recipe. If several people have published different recipes for the same game, `buddy catalog` lists their hashes and `buddy import GAME HASH` copies the one you choose. `buddy load-manifest FILE.yaml` indexes a locally provided Ludusavi catalog. **Path suggestions** offers both local YAML import and an optional download directly from Ludusavi. Buddy includes its own vanilla Terraria and Starbound recipes. Select a candidate to preview and approve the files on this device. The [spec](../SPEC.md) records the format and data licensing sources.

## Recovery and test coverage

Buddy records a restore plan before changing a live file and retains chunks for both versions locally. After an interruption, the next sync finishes the restore before launching. To put the pre-restore tree back, close the game and run `buddy rollback GAME`. If an unrelated process edits a file mid-restore, Buddy stops and identifies the affected path. For an interrupted game, the next wrapped launch asks you to confirm it has closed before capturing the surviving files. An expired lease still requires the second device's explicit takeover because the first device might be asleep with the game open.

Run `buddy cache-gc` to reclaim locally unreferenced chunks. On the server, `buddy gc --config server.example.toml` reclaims unreferenced chunks after a 24-hour grace period. The same sweep runs every six hours. SQLite can reuse the freed space internally. Back up a live database using SQLite's online backup facility, or stop the server before taking a filesystem copy of its database directory.

The Linux integration suites use isolated profiles and temporary files. Run them against a release binary:

```sh
python3 tests/sync.py target/release/buddy
python3 tests/recovery.py target/release/buddy
python3 tests/transport.py target/release/buddy
python3 tests/recipes.py target/release/buddy
python3 tests/large_sync.py target/release/buddy
```

The large-world test needs 12 GiB free. Running the portable binary on this desktop's Btrfs volume, a sparse 10 GiB world plus 2,000 small files took 18.94 seconds on first sync and 0.09 seconds on a warm unchanged sync. The unchanged run transferred zero chunk bytes. Changing seven bytes took 13.73 seconds and uploaded 4,194,372 bytes including pack framing. Restoring to a fresh profile took 49.67 seconds. The highest client peak RSS was 40.2 MiB. The world contains repeated data and the server is on loopback. A separate 10 GiB trial included a populated Minecraft save, throttled the network, interrupted an upload and opened the restored world in Minecraft. It also measures warm launch delay and exercises recovery on a full disk. See the [storage trial](storage-trial.md). Eligible Linux volumes reuse stable large-file identities. Recent edits, unsupported filesystems and interrupted restores cause full reads.

For the Deck Game Mode overlay, suspend, controller focus and immediate handoff cases, use the [separate probe walkthrough](../README.md#buddy-launch-probe). The desktop sync wrapper recovers interrupted sessions at the next launch. Choosing its outside-Steam worker behavior still requires the Deck hardware trace. The Windows executable exchanged saves with the native Linux binary under Wine, including replacing an existing file and preserving a game exit code. The [Windows Steam trial](windows-steam.md) covers a real shortcut and a store title in a guest. The native Windows Steam shortcut and Terraria store-title trials passed. Deck Game Mode still needs a hardware test. Settings conflicts currently use the same whole-snapshot choice as saves. Per-key config merging and translation scripts are future work.

## Rebuild for another platform

For a portable x86-64 Linux build, install Zig and cargo-zigbuild and run `cargo zigbuild --release --locked -p buddy --target x86_64-unknown-linux-gnu.2.28`. The resulting binary is under `target/x86_64-unknown-linux-gnu/release/`. The packaged build here used Zig 0.14.1 and cargo-zigbuild 0.24.1. On a Windows development machine with the Rust MSVC toolchain and Visual Studio C++ build tools, the standard build command is `cargo build --locked --release -p buddy`. The included Windows x86-64 executable was cross-built with LLVM-MinGW and statically linked libunwind.

For the GUI decision tests on Linux, start an isolated Xvfb and a window manager with xdotool and ImageMagick available. With that test display selected, run `BUDDY_TEST_X11=isolated python3 tests/ui_sync.py target/release/buddy "$(mktemp -d)"`. The test drives the visible windows and captures screenshots for the offline, conflict, and takeover cases.
