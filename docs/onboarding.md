# Set up Buddy on your home network

## Host the server

1. On the computer that will hold the saves, open Buddy and select **Host server…**. Pick its home-network IP and port. Use a stable address, such as one reserved for this computer in your router. The IP is part of the server certificate. A loopback address is suitable for a one-computer trial.
2. Select **Start HTTPS server**. Leave the host window open while devices sync. **Stop server** pauses hosting. Opening the host again with the same profile reuses the same identity, accounts, and save history.
3. Select **Export pairing file…** and move the exported JSON to your second device using a trusted transfer or USB drive. It contains the server address and public certificate. The private key stays in the server data directory. Keep that directory when backing up the host.
4. Select **Create account invite**, then **Copy invite**. Each invite creates one account and expires after 24 hours.

## Connect each device

On the host computer, select **Use my hosted server** in the Buddy sign-in window. On another device, select **Open server pairing file…** and choose the exported JSON. Compare the address and fingerprint shown in Buddy with the ones in the host window.

Choose **Create account with an invite** on the first device. Enter the invite, account name, password, and device name. On the second device, sign into the same account and give the device a different name. A second player can create a separate account using their own invite.

These clients must be able to reach the selected IP and TCP port. If your firewall prompts when the host starts, allow incoming access for the home network. The host window runs as your current user. For unattended hosting, run the same executable with `buddy serve --config PATH_TO_SERVER_TOML`; the host window shows the directory containing that TOML. The graphical host and the headless process share the same settings and cannot both listen on the same port.

The CLI supports `buddy login SERVER_URL ACCOUNT --pairing PATH_TO_PAIRING_FILE`. The TUI prompts for a pairing file at sign-in. Manually configured servers with a publicly or system-trusted HTTPS certificate can still use the normal server URL.

## Pick files for a game

Search the game library by title. The list includes games installed in secondary Steam libraries. Choose an existing account setup or a shared recipe when available. For vanilla Terraria, Buddy suggests the local player and world directories and the optional game configuration. For vanilla Starbound it suggests `storage/player`, `storage/universe`, and the optional `storage/starbound.config` when they exist. Select both save directories if you want to move characters and worlds together. A modded launcher can use a different storage path. Use **Browse…** to select the directories that launcher actually writes.

Select a suggested path or browse for a file or folder. Buddy counts the included files and bytes on a background thread. Expand **Included files** to inspect all relative filenames before choosing **Save selection**. Empty selections show zero files. Incompatible names, links inside a selected folder, and unavailable roots show an error. On Steam games, the setup view calls out existing Steam Cloud records when found. Check which files Cloud manages before enabling both services.

For broader path suggestions, **Path suggestions → Download from Ludusavi** fetches the upstream catalog on that device. Refreshing checks its ETag. **Import local YAML…** works with a catalog you already have. Imported suggestions show the source revision when selected. These operations update suggestions; each approved selection stays under your control. [Upstream manifest](https://github.com/mtkennerly/ludusavi-manifest)

## Trial results

On Linux, a fresh client started the HTTPS host through the graphical window, created an account with an invite and enrolled a disposable two-file selection after previewing both names and the 19-byte total. On native Windows 11, the host started from the client, exported the public pairing file through a native file picker, and displayed the matching fingerprint after import. A second Windows client profile signed in through HTTPS. The generated host directory had a protected NTFS access list, and only the current account and SYSTEM had access to the private key and database.
