# Try Buddy from Windows Steam

Dockur is a good fit for this test. It runs Windows as a KVM guest and automates installation. The questions to answer are whether pressing Play starts Buddy, whether its dialogs are usable, and whether a second device can read the last save. A lightweight game is enough. I'd use virt-manager if the experiment grew to need frequent virtual hardware changes or a dedicated GPU. [Dockur](https://github.com/dockur/windows) · [virt-manager](https://virt-manager.org/)

## Start the guest when you're ready

The host check found a working Docker daemon and KVM API, an accessible TUN device, 62 GiB of RAM, and about 199 GiB free on the Docker filesystem. The supplied Compose file pins the x86-64 Dockur 6.06 image by digest. It assigns four virtual CPUs, 8 GiB of RAM, an 80 GB virtual disk in a named volume, and a browser console bound to host loopback. The guest shares the small test kit directory with the host. [Compose file](../tests/windows/compose.yaml) · [Upstream release](https://github.com/dockur/windows/releases/tag/v6.06)

From the repo root in Bash, choose a guest password you can enter again:

```sh
read -rs -p 'Windows test guest password: ' BUDDY_VM_PASSWORD
echo
export BUDDY_VM_PASSWORD
docker compose -f tests/windows/compose.yaml up -d
```

Open the [local VM console](http://127.0.0.1:8006) on the desktop. First start downloads and installs Windows. The guest user is `BuddyTester`. Once the desktop is up, copy `buddy.exe` and `steam-trial.exe` from the guest's **Shared** folder (or drive `Z:`) into a new local folder such as `C:\Buddy Test`. Open PowerShell in that folder. This tests Windows file replacement on the guest disk. Install Steam inside the guest and sign in there when ready. [Dockur file sharing](https://github.com/dockur/windows#how-do-i-share-files-with-the-host) · [Steam installer](https://store.steampowered.com/about/)

## Three checks

Run one at a time. Each command starts a real Buddy server on an ephemeral guest loopback port, creates two isolated client profiles, and seeds a fresh test world. The second profile checks the lease and reads the committed bytes through the API. A one-machine run can therefore qualify the Windows launch path separately from LAN or TLS setup. Keep the terminal open. Report files use a fresh filename each time.

1. **Check the Windows binary directly.** Run `.\steam-trial.exe --report win-direct.json direct`. The launcher exits with code 23 while its child is still waiting. Buddy must hold the lease until that child writes its final save, return 23, and publish exactly what the second profile expects. To check the graphical resolver independently of Steam, run `.\steam-trial.exe --report win-dialog.json direct --conflict` and choose **Use server copy**.
2. **Launch a disposable non-Steam shortcut.** Run `.\steam-trial.exe --report win-shortcut.json shortcut --conflict`. In Steam, add the local `buddy.exe` as a **Non-Steam Game**. Open its shortcut properties and paste the printed **Target**, **Start In**, and **Launch Options** values. Press **Play** from Steam and choose **Use server copy** in Buddy's graphical conflict window. The fixture checks the restored bytes before starting, a quoted Unicode argument, its working directory, and Steam's launch environment. The second profile checks the late save and both permanent conflict backups. The trial exits on its own. Remove the temporary shortcut afterward.
3. **Launch a real Steam store title.** Choose a lightweight game with empty Launch Options, preferably installed in a second Steam library. Run the commands below, paste the printed launch line into **Properties → General → Launch Options**, and press **Play**. The wrapper infers the game ID from Steam. For a GUI setup check, use a second PowerShell window and run `.\buddy.exe --profile-dir 'TRIAL_DIRECTORY\player' gui`, substituting the trial directory printed by the runner. Search for the installed game, select it, and use the install-folder shortcut to check the library path. Close the chooser with Cancel to keep the disposable binding. When the actual game is visibly running, press Enter in the trial terminal. It verifies the lease, verifies the initial pull, then changes a disposable checkpoint. Quit the game normally and press Enter again. A pass requires the second profile to acquire the released lease and restore that exact checkpoint. Restore the original Launch Options before closing the trial session. The only path Buddy manages in this trial is its temporary world. [Steam game launch options](https://help.steampowered.com/en/faqs/view/7D01-D2DD-D75E-2955)

```powershell
$appId = Read-Host "Steam app ID of the installed game"
.\steam-trial.exe --report win-store.json store $appId
```

**What to record.** Keep the JSON report, Windows and Steam versions, title/app ID, library location, and whether Play, the overlay, dialog focus and Quit worked. The direct case checks the numeric exit code. The Steam cases check what gets published after Play. The real-title checkpoint is synthetic. Use a disposable in-game save and a second machine for a later test of that specific game's file recipe. Keep GPU frame rate and anti-cheat compatibility as separate hardware checks.

Stop the guest with `docker compose -f tests/windows/compose.yaml stop` in the same host shell. The named volume retains Windows and Steam for the next run.

## Rebuild the kit

On a Windows Rust development machine, run `cargo build --locked --release -p buddy --bin buddy --example steam-trial` and put `target\release\buddy.exe` and `target\release\examples\steam-trial.exe` together. On this host the equivalent cross-built executables are in `target/x86_64-pc-windows-gnullvm/release/` and its `examples/` subdirectory. Stage them for Compose from the repo root:

```sh
mkdir -p target/windows-steam-kit
cp target/x86_64-pc-windows-gnullvm/release/buddy.exe target/windows-steam-kit/
cp target/x86_64-pc-windows-gnullvm/release/examples/steam-trial.exe target/windows-steam-kit/
```

Buddy pins the upstream Windows Job Object fix that waits for the job's active process count to reach zero. The previous dependency could return on an earlier completion notification. The direct fixture reproduced that early return under Wine and passed after the fix in both Wine and a native Windows 11 guest. On Windows, Buddy preserved exit code 23, held the lease through the child's final write, and delivered that save to the second profile. [Upstream fix](https://github.com/watchexec/process-wrap/pull/76) · [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)

The signed-in Windows 11 Steam shortcut test also passed, including the graphical conflict choice and both permanent backups. The Dockur guest’s default OpenGL implementation could not render the chooser. We placed Mesa’s x64 `opengl32.dll` and `libgallium_wgl.dll` beside the test copy of Buddy and started the guest Steam process with `GALLIUM_DRIVER=llvmpipe`. [Mesa Windows distribution](https://github.com/pal1000/mesa-dist-win/releases/tag/26.3.1)

Vanilla Terraria 1.4.5.8 passed the store-title trial (App ID 105600). After backing up existing saves, we temporarily disabled this title’s Steam Cloud setting and appended `-savedirectory "TRIAL_DIRECTORY\player-world"` to the generated launch line. The second profile restored all nine files from the session, including the new character, map and world, with matching SHA-256 hashes. We loaded the restored character and world in Terraria and saw the three collected dirt blocks and the excavation beside spawn. We restored the original launch options and Steam Cloud setting afterward.

### Native Windows and Linux round trip

A separate trial ran the Steam-installed Terraria 1.4.5.8 executables directly through `buddy run` on a Windows 11 VM and a Linux desktop. The Linux installation was in a secondary Steam library. Each client used its own disposable save directory, with `Players` and `Worlds` bound to the same two logical slots. The save data traveled through the Buddy server API.

Windows uploaded a player, map and world. Linux downloaded all three with matching SHA-256 hashes. In the Linux game we chopped a tree and built a three-block wooden post beside spawn. After Save & Exit and quitting Terraria, Windows downloaded five matching files, including Terraria’s player and world `.bak` files. Reopening the game on Windows showed the post and the remaining 10 wood in the player’s sixth inventory slot. On each platform, a competing sync was blocked while the other game held the lease. Steam Deck Game Mode still needs a separate exit test.
