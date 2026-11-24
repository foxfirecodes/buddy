# Set up Buddy on your home network

## Host the server

1. On the computer that will hold the saves, open Buddy and select **Host a server**. Pick its home-network IP and port. Use a stable address, such as one reserved for this computer in your router. The IP is part of the server certificate. A loopback address is suitable for a one-computer trial.
2. Select **Start HTTPS server**. Leave the host window open while devices sync. **Stop server** pauses hosting. Opening the host again with the same profile reuses the same identity, accounts, and save history.
3. Select **Export pairing file…** and move the exported JSON to your second device using a trusted transfer or USB drive. It contains the server address and public certificate. The private key stays in the server data directory. Keep that directory when backing up the host.
4. Select **Create account invite**, then **Copy invite**. Each invite creates one account and expires after 24 hours.

## Connect each device

In Buddy, choose **Join a server**. On the host computer, choose **Use my hosted server**. On another device, choose **Open pairing file…** and select the exported JSON. Compare the address and certificate fingerprint with the ones in the host window. For a manually configured HTTP server or a server using a system-trusted HTTPS certificate, expand **Enter server address** and enter its URL. [Headless server setup](sync.md#start-a-server-and-sign-in)

Choose **Use an invite** on the first device. Enter the invite, account name, password, and device name and choose **Create account**. On the second device, sign into the same account and give the device a different name. A second player can create a separate account using their own invite.

These clients must be able to reach the selected IP and TCP port. If your firewall prompts when the host starts, allow incoming access for the home network. The host window runs as your current user. For unattended hosting, run the same executable with `buddy serve --config PATH_TO_SERVER_TOML`; expand **Server data folder** in the host window to find that TOML. The graphical host and the headless process share the same settings and cannot both listen on the same port.

The CLI supports `buddy login SERVER_URL ACCOUNT --pairing PATH_TO_PAIRING_FILE`. The TUI prompts for a pairing file at sign-in. Manually configured servers with a publicly or system-trusted HTTPS certificate can still use the normal server URL.

## Pick files for a game

Search **Game library** by title, including games installed in additional Steam libraries. **Server recipes** shows games with a file layout from your account or a recipe another player explicitly shared. Open the game or choose **+ Non-Steam game**. An enrolled game opens its overview. Choose **Edit files** to change this device’s selection.

For a new game, review **Files to sync**. Buddy uses your account’s existing layout when available. When other players have shared recipes, the **File recipe** menu lets you choose theirs or use **Detected locations**. A recipe identifies locations. Each player’s saves stay private.

Check the locations you want. Select a row to inspect its relative filenames in **Included files**. For vanilla Terraria or Starbound, select both player and world directories to sync characters and worlds together. Game settings are optional. A modded launcher may use different locations. Use **Add files…** to browse from Home, the Steam folder or an available Proton prefix. **Custom root directories** lets devices map the same root name to different folders.

Review the total files and bytes, then choose **Save setup** to apply the checked locations together. An empty directory shows zero files. Unavailable roots, incompatible names and links inside a selected folder surface during review. If **Steam Cloud records found** appears, check what Steam already manages before enabling both services for the same files.

For broader path suggestions, **Settings → Path suggestions → Download from Ludusavi** fetches the upstream catalog on that device. Refreshing checks its ETag. **Import local YAML…** works with a catalog you already have. [Upstream manifest](https://github.com/mtkennerly/ludusavi-manifest)

## Trial results

On Linux, a fresh client started the HTTPS host through the graphical window, created an account with an invite and enrolled a disposable two-file selection after previewing both names and the 19-byte total. In the Windows 11 trial that preceded the Phase redesign, the host started from the client, exported the public pairing file through a native file picker, and displayed the matching fingerprint after import. A second Windows client profile signed in through HTTPS. The generated host directory had a protected NTFS access list, and only the current account and SYSTEM had access to the private key and database.

The Phase client’s Linux trial paired two disposable profiles with the graphical HTTPS host. On Windows 11, Explorer opened and extracted the Phase ZIP. SmartScreen stopped the unsigned executable at the unknown-publisher prompt, so the final Windows GUI still needs runtime verification. Windows Defender also flagged an earlier build during testing.
