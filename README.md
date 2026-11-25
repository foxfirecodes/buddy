# Buddy

Buddy syncs selected game saves and settings between devices. The native Rust app includes the graphical client, command-line and terminal clients, Steam wrapper, and control server. The desktop sync prototype is ready for a trial with a disposable world.

To build the sync app from source, run `cargo build --locked --release -p buddy` from the repository root. Open `target/release/buddy` on Linux or `target\release\buddy.exe` on Windows to start the graphical client. To install `buddy` into Cargo’s bin directory, run `cargo install --locked --path buddy`.

For headless hosting, run `buddy init-server` in the directory where you want to keep your server settings. Edit `listen` in `server.toml`, then run `buddy invite` and `buddy serve`. Plain HTTP on a non-loopback address prints a warning and starts normally.

- [Set up Buddy on your home network](docs/onboarding.md) to host an HTTPS server, pair a second device, and preview the files you want to sync.
- [Run the sync client and server](docs/sync.md) for setup, a two-device walkthrough, backups, and recovery.
- [Test from Windows Steam](docs/windows-steam.md) with a disposable game on Windows or in a Dockur guest.
- [Restore both sides of an offline Terraria conflict](docs/offline-terraria.md), [restore before a first Proton launch](docs/proton-first-play.md), or [reproduce the large-world and disk-full trials](docs/storage-trial.md).
- [Read the product and protocol spec](SPEC.md) for design decisions and planned features.
- [Phase design language](docs/design-language.md) describes Buddy’s visual style and links to the interactive reference.
- [Run the Steam Deck launch probe](#buddy-launch-probe) to measure when Game Mode lets post-game work finish. This is a separate executable and experiment.

## Development tasks

Run `mise trust` once in your checkout. If your Rust toolchain is missing the check tools, install them with `rustup component add rustfmt clippy`.

| Command | What it does |
| --- | --- |
| `mise run check` | Check formatting, run Clippy with warnings as errors, and test the workspace for the current platform. |
| `mise run test:sync` | Build Buddy and exercise two-device sync against a disposable server on Linux. Requires Python 3. |
| `mise run build:linux` | Build the portable x86-64 Linux and Steam Deck client targeting glibc 2.28. |
| `mise run stage` | Cross-build the portable Linux/Deck and Windows x86-64 binaries into `server-stage/` for copying to your server. |

## Install Buddy on another device

Run `mise run stage` on your Linux build machine, then copy `server-stage/` beside `server.toml` on your server. The same `buddy-linux-x86_64` executable runs the server and every Linux or Steam Deck client. From the directory containing `server.toml`, publish each platform you use.

```sh
./server-stage/buddy-linux-x86_64 publish-release --config server.toml linux-x86_64 ./server-stage/buddy-linux-x86_64
./server-stage/buddy-linux-x86_64 publish-release --config server.toml windows-x86_64 ./server-stage/buddy-windows-x86_64.exe
```

On a signed-in client, open **Settings → Pair a device**, choose the destination platform, and copy the command. Paste it into a shell on Linux or PowerShell on Windows. The command verifies and installs the build, links the device to your account, then offers **Add Buddy to Steam**. Anyone with the command can join your account once within ten minutes. With plain HTTP, use a trusted network, since the connection carries credentials.

The terminal equivalent is `buddy install-command linux-x86_64` or `buddy install-command windows-x86_64`. The first time an existing HTTP client pairs or updates, enter the release signing key printed on the server by `publish-release`. In the CLI, add `--trust-key KEY`. Paired HTTPS clients use their existing server certificate. The new device receives the trusted signing key in the copied command.

Linux installs to `~/.local/bin/buddy` by default. If that directory is outside your PATH and `~/bin` is already on it, the installer uses `~/bin`. You can set `XDG_BIN_HOME` to choose another location. If the chosen directory is outside your PATH, the installer prints a command to add it. Windows installs to `%LOCALAPPDATA%\Programs\Buddy\buddy.exe` and adds that directory to the user PATH for new terminals. The installer uses the OS credential store. A headless Linux device can explicitly opt into private-file credentials with `export BUDDY_ALLOW_FILE_CREDENTIAL=true` before running the command.

To roll out a new build, repeat `publish-release` on the host. On each client choose **Settings → Update from server…** or run `buddy update`. Buddy verifies the release and updates the installed executable in place. Back up the host database and its adjacent `<database filename>.releases` directory together. That directory contains the published binaries and the private signing key clients trust.

## Buddy launch probe

This prototype measures when Steam's launch command returns, whether the game outlives it, and whether an independent worker can still finish work after Steam stops the game. The worker captures a tiny test marker and writes a receipt three seconds later. Real games run with their original arguments and working directory. All probe writes go to the probe's own session directory. You can test with Steam Cloud left as it is.

## Get it onto both machines

**On an x86-64 Linux desktop**, build the probe from the repository root with Rust 1.89 or later. This produces one executable you can use on both the desktop and the Deck.

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --locked --release -p buddy-probe --target x86_64-unknown-linux-musl
install -Dm755 target/x86_64-unknown-linux-musl/release/buddy-probe ~/.local/bin/buddy-probe
~/.local/bin/buddy-probe libraries
```

`libraries` lists every registered Steam library, including unavailable drives. To find a particular game's install directory and any existing Proton prefix, run `~/.local/bin/buddy-probe libraries APPID`, replacing `APPID` with its numeric Steam app ID. A wrapped launch also records `STEAM_COMPAT_DATA_PATH/pfx` if Steam supplies that override.

**On the Deck**, use the same binary. The Deck needs native Steam, a running user systemd manager, and either Zenity or KDialog for the optional test window. It needs no Rust toolchain. In Desktop Mode, copy the binary to `~/Downloads/buddy-probe` using USB, KDE Connect, or your usual transfer method. In Konsole:

```sh
install -Dm755 ~/Downloads/buddy-probe ~/.local/bin/buddy-probe
~/.local/bin/buddy-probe libraries
```

If you already use SSH with the Deck, you can copy the probe directly from the desktop. Set `deck_host` to the SSH username and hostname or IP address you use for your Deck.

```sh
deck_host=deck@steamdeck
ssh "$deck_host" 'mkdir -p ~/.local/bin'
scp ~/.local/bin/buddy-probe "$deck_host:.local/bin/buddy-probe"
ssh "$deck_host" 'chmod 755 ~/.local/bin/buddy-probe && ~/.local/bin/buddy-probe libraries'
```

## Start with a disposable game

1. On each machine, use Steam's **Games → Add a Non-Steam Game → Browse** and select the installed `buddy-probe`. In the picker, Ctrl+H shows the hidden `.local` folder. Name the shortcut **Buddy test game**.
2. Under **Properties → Shortcut**, set **Launch Options** to just `demo-window`. Keep **Force the use of a specific Steam Play compatibility tool** unchecked for this native fixture.
3. On the desktop, press **Play**, wait until the test window appears, then press **OK**. About six seconds after exit, run `~/.local/bin/buddy-probe report`. Look for `fixture_saved`, `wrapper_return`, `boundary_observed`, `receipt_written`, and `fixture=saved-on-quit`.
4. Return the Deck to **Game Mode** and run the cases below. Use touch on the test window if Steam Input doesn't focus the OK button. Record any focus issue. If you set up SSH above, run `ssh "$deck_host" '~/.local/bin/buddy-probe report'` from the same desktop terminal to read the report while the Deck stays in Game Mode. Otherwise, allow the worker to finish before returning to Desktop Mode and use Konsole to read the reports.

| Case | Do this | What to look for |
| --- | --- | --- |
| Normal exit | Press OK in the Deck test window. | The receipt contains `saved-on-quit`. The source game saved before the worker captured it. |
| Steam overlay stop | Launch again, leave the window open for five seconds, then use Steam's **Exit game** or **Stop Game**. | A receipt should still appear. `wrapper_gone … exit hook missing` means the worker survived a wrapper that never returned. An `opened` marker means the fixture was stopped before its graceful save. That is a valid interrupted result. |
| Suspend and resume | Leave the window open, tap Power, wait at least 15 seconds, wake the Deck, and leave the window open another five seconds before pressing OK. | One session ID throughout. Look for a `sampling_gap` around sleep. No boundary or receipt should appear while the game is still open. The final receipt should contain `saved-on-quit`. A wall-clock gap alone cannot identify suspend. |
| Immediate Deck → desktop handoff | Stop or quit the Deck fixture and immediately press Play on the **desktop fixture**. If using SSH, check the Deck report as soon as the desktop window opens and again six seconds later. | Record both session IDs and whether the Deck receipt already existed at desktop launch. If it appeared later, you've measured the interval a future lease must cover. Both fixtures can run independently in this probe. |
| Detached launcher | In a terminal on either machine, run `~/.local/bin/buddy-probe demo-detached`, keep the terminal open, wait nine seconds, then run `~/.local/bin/buddy-probe report`. | `wrapper_return` precedes `fixture_saved`, which precedes `boundary_observed`. The receipt must contain `saved-on-quit`. |

## Repeat with a real game

Choose a game installed on both devices. A Proton game on a secondary drive or the Deck's microSD also exercises library and prefix discovery. Save the game's existing Steam Launch Options so you can restore them later.

On each device, run this in a terminal to print the Steam launch option with the full path to your installed probe. For an empty options field, paste the output into **Properties → General → Launch Options**.

```sh
printf '"%s" run -- %%command%%\n' "$HOME/.local/bin/buddy-probe"
```

If you already use `mangohud %command% -windowed`, replace just the `%command%` in the printed line with that entire expression. Keep any leading shell environment assignments before Buddy. If your existing options contain pipelines or other shell logic, use a different game with empty options for the baseline.

Play to the title screen first. Verify input, overlay, and the ability to exit normally. Repeat the normal exit, overlay stop, and suspend cases, comparing the game's visible lifetime with the trace. Use the disposable fixture for forced stops around writes. In the real-game report, check the app ID, matching install directory, and prefix path against **Steam → Settings → Storage**. A real-game receipt records only timing and process evidence. The probe never opens the game's saves.

**What counts as a failed boundary test?** A receipt while the game is visibly running is an early detection. No receipt ten seconds after the game and its helpers have closed is worth investigating. If a launcher hands off to an already-running process, or a child closes inherited descriptors and detaches between samples, the worker can miss it. If an unrelated helper inherits the descriptor, the worker can wait too long. Those are exactly the cases this experiment should uncover.

## Read or share a result

```sh
~/.local/bin/buddy-probe sessions
~/.local/bin/buddy-probe report
~/.local/bin/buddy-probe report SESSION-ID
```

Each run has a directory under `~/.local/state/buddy-probe/<session-id>/`. Timestamps are Unix milliseconds on the originating device. Compare event order within one trace, or check receipt presence over SSH to avoid assuming both device clocks agree. To share a result, capture `buddy-probe report SESSION-ID` and note the device, game, quit method, storage device, and whether the game was visibly still open at `boundary_observed`. Reports include local filesystem paths.

If no report appears, verify the executable path and its execute bit. If the report stops at `wrapper_started`, check `systemctl --user is-system-running` and `journalctl --user -u buddy-probe-SESSION-ID.service`. A working run shows different cgroups for `wrapper_started` and `observer_ready`. The service is transient. Native Steam is the target of this experiment. A Flatpak PID namespace or inaccessible host user bus can prevent the observer from arming.

When finished, restore the real game's original Launch Options and remove the disposable Steam shortcut. No persistent service was installed.

## Build and reproduce the desktop checks

After [building the probe](#get-it-onto-both-machines), run the smoke suite from the repository root on Linux.

```sh
python3 tests/boundary.py target/x86_64-unknown-linux-musl/release/buddy-probe
```

The smoke suite uses synthetic files and transient user services on the local machine. It checks a normal exit, a detached child, forced wrapper death, argument and exit-code preservation, and finding a game in a second library after an unavailable drive. It requires a real user service bus. Steam overlay behavior, Deck controller focus, and suspend still need the hardware cases above.
