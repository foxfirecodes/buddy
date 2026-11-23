# First Play with Proton

On a Linux device with an existing account recipe, select the installed Steam game in Buddy and use **Copy launch options**. Paste the text in the game’s Steam properties, preserving existing launch flags as described in the [sync walkthrough](sync.md). Press **Play** in Steam.

If Steam selected an empty prefix, Buddy shows its path and offers **Initialize prefix**. It runs the selected Steam Linux Runtime and Proton’s `getcompatpath` operation. This prepares the Windows profile before the game opens. A prefix on a secondary library or an explicit `STEAM_COMPAT_DATA_PATH` works the same way.

**Confirm Proton save locations** shows each slot and its destination. Existing Proton mappings rooted at `pfx` take precedence. Windows home, Documents and roaming-data locations can supply a fallback. The dialog shows the resolved Documents path if the prefix redirects it. **Use these locations** creates any missing game folders, binds them on this device and completes the first online reconciliation before starting the original Steam command. Both confirmation windows default to **Cancel**.

If a slot needs a custom root or a different location, cancel the dialog, open Buddy and map the slots you wish to sync. If the account has no recipe yet, initializing the prefix lets you browse it in Buddy to choose your first files. If the server becomes unreachable before this first restore completes, reconnect and press Play again. Subsequent enrolled sessions offer normal offline play.

## Desktop Steam trial

Noita (Steam App ID 881100) ran from a secondary library with a fresh `STEAM_COMPAT_DATA_PATH`. We backed up its existing saves, seeded a private account with a disposable copy of `save00` and added one test marker. The receiving profile started without local bindings. We temporarily disabled Noita’s Steam Cloud setting and pressed Play in the signed-in desktop Steam library.

We approved prefix initialization and the resolved `AppData/LocalLow/Nolla_Games_Noita/save00` path. A witness immediately before Steam’s game command checked all 87 files against the source SHA-256 digests and then executed the original launch wrapper, reaper, Steam Linux Runtime, Proton and game arguments. Noita opened a new game. **Save & Quit** published a second revision and released the lease. A sync back to the source profile reproduced the 144 resulting files with matching hashes. The pre-trial save directories on the host matched their backups.

The Linux graphical integration suite exercises both cancellation points and uses a fixture that checks the restored bytes at game entry. Run it on an isolated X display using the instructions in the [sync walkthrough](sync.md).
