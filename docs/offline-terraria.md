# Recover both versions of an offline Terraria world

Vanilla Terraria 1.4.5.8 ran on native Linux and in a Windows 11 VM. Each device used a separate Buddy profile, save directory and copy of the same disposable player and world. Both began at server revision 3, with a three-block wooden post beside spawn. The save folders exchanged data through Buddy’s API.

1. Disconnect only the Linux client. Launch Terraria through Buddy, choose **Play offline**, and place three dirt blocks on top of the post. Save and quit. Buddy reported one queued snapshot and a closed session.
2. On Windows, open the common baseline, extend the post three blocks sideways with wood, and save and quit. Windows committed revision 4. The player kept three dirt and seven wood.
3. Reconnect Linux and launch through Buddy. The graphical choice showed the device, capture time, size and backup number for each version. A separate history check while the dialog was open confirmed both revisions already had conflict pins. Choose **Use server copy**. All six Linux files then matched the Windows branch.
4. Run `buddy restore steam-105600 ID` for each pin separately. Compare every relative path and SHA-256 digest against those captured after the corresponding device quit, then open the restored player and world in native Terraria.

| Conflict pin | Files verified before reopening | Landmark beside spawn |
| --- | --- | --- |
| 5, native Linux | 6 of 6 | Three dirt blocks atop the post |
| 4, native Windows | 6 of 6 | A three-block horizontal wooden extension |

The six files were the player, map, world and Terraria’s own backup files. The hash checks precede reopening because normal gameplay updates the saves. Each platform used its Steam-installed game executable through `buddy run` with a disposable `-savedirectory`. The Windows guest stayed online while Linux was disconnected.
