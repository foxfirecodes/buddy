# Test Buddy sync

Use a disposable world for the first run. Buddy can replace selected files on a pull and propagate deletions on a push. A folder selection includes its whole tree. For a game with its own cloud sync, use a test copy or disable that game's other sync writer on both devices for the trial.

## Start a server and sign in

This workspace also contains ready-built x86-64 executables. Use `target/release/buddy` on this desktop, `target/x86_64-unknown-linux-gnu/release/buddy` for the portable Linux build, and `target/x86_64-pc-windows-gnullvm/release/buddy.exe` on Windows. For the Deck sync trial, copy the portable Linux file to `~/Downloads/buddy`, switch to Desktop Mode and run `install -Dm755 ~/Downloads/buddy ~/.local/bin/buddy` followed by `~/.local/bin/buddy`. It targets glibc 2.28. The Game Mode exit experiment uses the separate `buddy-probe` binary.

For graphical hosting on your home network, choose **Host a server** when opening Buddy, or **Settings → Host a server** from the library. [Pairing walkthrough](onboarding.md)

For headless hosting, run the installed binary in the directory where you want to keep the server settings and data.

```sh
buddy init-server
# Edit listen in server.toml before starting the server.
buddy invite
buddy serve
```

`init-server` copies the bundled example to `./server.toml` and preserves any existing file. The default listener is `127.0.0.1:7341`. To reach Buddy from another device, set `listen` to the server's address on the desired interface, for example `"192.168.1.20:7341"`, or use `"0.0.0.0:7341"` for all IPv4 interfaces. An IPv6 address takes brackets, as in `"[fd00::20]:7341"`. Plain HTTP on a non-loopback address starts with a warning because Buddy sends credentials and saves without TLS.

Keep the server terminal open. On each client choose **Join a server → Enter server address** and enter the reachable URL, for example `http://192.168.1.20:7341`. Choose **Use an invite**, then enter the invite, an account name, a password, and a device name such as **Desktop**. An invite is valid for one new account for 24 hours. Sign the second device into the same account with its own device name.

The shipped TOML puts the database, including file chunks, under `buddy-data/` next to the settings file. To use another location, copy the TOML, edit `database`, and pass that file to both commands. Normal sign-in uses the OS credential vault. If the device lacks an unlocked vault, **Credential storage** offers an explicit private-file option. On a shared PC, use separate OS accounts for different players. A local OS user able to read the server database can read its saves.

With the headless loopback configuration, you can put a TLS reverse proxy in front of the listener or use an SSH tunnel. The graphical host serves HTTPS directly. With SSH access to the desktop, run this on the second machine and keep it open:

```sh
ssh -N -L 127.0.0.1:7341:127.0.0.1:7341 YOUR_DESKTOP_USER@YOUR_DESKTOP_HOST
```

Both clients can then use `http://127.0.0.1:7341`. Stick with the same URL on each device across sessions. Buddy scopes the local cache to the server URL and account. The server needs no web frontend. A reverse proxy must allow request bodies up to 32 MiB to accommodate large manifests.

## Try a handoff

1. On the desktop, create a folder named `Buddy test world` outside Buddy's data directory. Put a text file with some recognizable progress in it. In Buddy choose **+ Non-Steam game**, give it a title, and use **Add files… → Choose folder…** to select the folder. Check the filenames under **Included files**, then choose **Save setup**. Click **Sync now** on the overview.
2. On the second machine, create an empty test folder. Sign in, open **Settings → Refresh**, then use **Server recipes** to find that same game. Select the existing save location and use **Choose location…** or **Location options → Change location…** to point it at the empty test folder. Choose **Save setup**. Click **Sync now** and check the file's contents. Creating a second non-Steam game would give it a different ID.
3. Edit the file on the second device, sync there, then sync the desktop. Try leaving both devices at the same baseline, making different edits on each, then syncing first one and then the other. The second sync shows both device names and capture/commit times. Both versions are already pinned in **Backups** when the choice appears.
4. For a Steam game, search by title or app ID, select the save files, then **Copy launch options** into Steam's **Properties → General → Launch Options**. A typical entry is `"/absolute/path/to/buddy" run --game steam-12345 -- %command%`. Keep any existing wrapper or game flags with the original command. On the other device copy that device's launch options. The file chooser offers home, Steam install and existing Proton prefix shortcuts. Discovery visits registered libraries across drives.
5. For a non-Steam shortcut inside Steam, expand **Launch as a non-Steam shortcut**. Copy the new Target and the Launch Options prefix. Append the original quoted executable and its arguments to the prefix, and preserve the original Start In directory. For a non-Steam game outside Steam, open the game’s **Settings → Launch a game directly**, choose the executable, working directory, and arguments, then **Save launcher**. Use **Play** in Buddy. These launcher details stay on that device. A launch that hands off to an already-running process can use **Ask me when the game has closed**.
6. To test offline play, close the game, stop the server or tunnel and use the configured launch command. Choose **Play offline**, change the test save and quit. Restart the connection and launch or sync again. The client compares its queued session against the server. **Decide later** keeps the data available for another attempt.

For a freshly installed Proton title with saves already on the server, Buddy can initialize its prefix and restore your selections on the first Play. [First Play with Proton](proton-first-play.md)

Each location has an ID such as `saves` or `settings` that maps to a path on each device. Choose a row to inspect its files. New settings and ambiguous alternatives start unchecked. **Edit files** opens all selections together. Clear a checkbox and choose **Save setup** to stop syncing that location on this device. Buddy asks for confirmation and preserves the files and server history. Custom root names such as `instance` can point to different directories on each device. Recipe roots appear above the location list; map or change each root there. When a recipe points to a missing folder or a file with missing parent folders under a mapped root, review the destination and opt in to creating those directories when saving. Buddy does not create an empty file. **Remove from recipe** removes a location account-wide after confirmation; unchecking it only stops syncing it on this device. Upgrade the server before using account-wide removal. In the CLI, use `buddy remove-slot GAME SLOT` for account-wide removal and `buddy unbind GAME SLOT` for this device. Older backups remain subject to the existing pin and retention rules.

The game’s **Settings** tab contains recipe sharing and backup retention. Publishing a recipe lets others on that server discover its path rules. Save bytes, histories and lease ownership belong to the account. **Back up now** creates a non-expiring backup of the closed game's current files. **Keep** pins an existing revision. Restore and delete are available in **Backups**. The default rolling history keeps three previous distinct commits in addition to the current one. Conflict backups and manual pins survive that rotation.

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

Use `buddy sync GAME --interactive` for terminal conflict choices. The default CLI sync stops if a human decision is required. Steam's `run` always opens graphical decisions. In **Files to sync**, use the **File recipe** menu to select a shared recipe. Inspecting a recipe leaves the server untouched. **Save setup** copies the selected recipe and binds your approved locations. If several people have published different recipes for the same game, `buddy catalog` lists their hashes and `buddy import GAME HASH` copies the one you choose. `buddy load-manifest FILE.yaml` indexes a locally provided Ludusavi catalog. **Path suggestions** offers both local YAML import and an optional download directly from Ludusavi. Buddy includes its own vanilla Terraria and Starbound recipes. Select a candidate to preview and approve the files on this device. The [spec](../SPEC.md) records the format and data licensing sources.

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

The large-world test needs 12 GiB free. Running the portable binary on this desktop's Btrfs volume, a sparse 10 GiB world plus 2,000 small files took 18.94 seconds on first sync and 0.09 seconds on a warm unchanged sync. The unchanged run transferred zero chunk bytes. Changing seven bytes took 13.73 seconds and uploaded 4,194,372 bytes including pack framing. Restoring to a fresh profile took 49.67 seconds. The highest client peak RSS was 40.2 MiB. The world contains repeated data and the server is on loopback. A separate 10 GiB trial included a populated Minecraft save, throttled the network, interrupted an upload and opened the restored world in Minecraft. It also measures warm launch delay and exercises recovery on a full disk. See the [storage trial](storage-trial.md).

For ordinary restores on Linux ext-family, Btrfs and XFS volumes, Buddy can reuse a large file’s verified digest when its device, inode, change time, size and modification time still match. If a file was too recently written to trust that identity, Buddy confirms its digest from the live bytes before using the hint. Windows and other filesystems stream a whole-file digest to verify each file against the restore plan. Every restore checks the full directory inventory, verifies the chunks it installs and checks the resulting files. Replaying an interrupted restore starts by reading the surviving files in full to identify either journaled version and repair the local cache.

For the Deck Game Mode overlay, suspend, controller focus and immediate handoff cases, use the [separate probe walkthrough](../README.md#buddy-launch-probe). The desktop sync wrapper recovers interrupted sessions at the next launch. Choosing its outside-Steam worker behavior still requires the Deck hardware trace. The Windows executable exchanged saves with the native Linux binary under Wine, including replacing an existing file and preserving a game exit code. The [Windows Steam trial](windows-steam.md) covers a real shortcut and a store title in a guest. The native Windows Steam shortcut and Terraria store-title trials passed. Normal exits and three-device sync succeeded on Arch, Steam Deck Game Mode, and Bazzite; one game crash was recovered. An intermittent white screen on launch remains unresolved. Overlay force-stop and suspend still require separate validation. Settings conflicts currently use the same whole-snapshot choice as saves. Per-key config merging and translation scripts are future work.

## Rebuild for another platform

For a portable x86-64 Linux build, install Zig and cargo-zigbuild and run `cargo zigbuild --release --locked -p buddy --target x86_64-unknown-linux-gnu.2.28`. The resulting binary is under `target/x86_64-unknown-linux-gnu/release/`. The packaged build here used Zig 0.14.1 and cargo-zigbuild 0.24.1. On a Windows development machine with the Rust MSVC toolchain and Visual Studio C++ build tools, the standard build command is `cargo build --locked --release -p buddy`. The included Windows x86-64 executable was cross-built with LLVM-MinGW and statically linked libunwind.

For the GUI decision tests on Linux, start an isolated Xvfb and a window manager with xdotool and ImageMagick available. With that test display selected, run `BUDDY_TEST_X11=isolated python3 tests/ui_sync.py target/release/buddy "$(mktemp -d)"`. The test drives the visible windows and captures screenshots for the offline, conflict, and takeover cases.
