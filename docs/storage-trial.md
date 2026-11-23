# Populated worlds and interrupted restores

The storage trial runs the release binary as a server and two clients. A local HTTP proxy limits chunk traffic and rejects new uploads after at least 32 MiB of chunks have been accepted. The test checks that the old revision stays current and playable data stays byte-identical on the second client. After reconnecting, the client must resume without sending any previously accepted chunk again.

To run the opt-in benchmark from the repository root on Linux, use a **closed** Minecraft world. The test copies it, checks the copy against the original, and keeps all writable state under a temporary directory in `target/`. You can omit `--world` for a completely generated workload.

```sh
cargo build --release --locked -p buddy
python3 tests/populated_sync.py target/release/buddy \
  --world "/path/to/closed-world" --gib 10 --mib-per-sec 48 \
  --report target/populated-storage-result.json
python3 tests/disk_full.py target/release/buddy
```

Budget roughly six times the input size on the volume containing `target/`. Both test clients maintain chunk caches, the server stores its own copy, and restore needs space for the staged files. The optional generated files use valid Anvil region headers and distinct deterministic NBT byte arrays. They sit alongside the copied Minecraft world. Their payloads model data volume and chunk changes. The smaller [sparse benchmark](sync.md#recovery-and-test-coverage) exercises deduplication against highly repetitive data.

For a before-and-after comparison, run the benchmark with each release binary and the same closed world. Compare `region_update` in the two JSON reports and check that the final `tree_sha256` agrees. On Linux with procfs available, `client_rchar` counts bytes returned by read calls, including reads served from page cache, and `client_read_bytes` counts bytes the kernel attributes to storage reads by the client. The proxy counts successful binary pack bodies separately.

The disk-full test uses an unprivileged Linux user and mount namespace with a dedicated 16 MiB tmpfs. A 13 MiB ballast file leaves too little space to stage a 6 MiB replacement. The client receives ENOSPC from the filesystem. The test requires the old files to remain intact, rollback to work while space is still constrained, and a second failed restore to finish after removing the ballast. It also checks the server revision, the local restore journal and staging-directory cleanup. The tmpfs exists only in the test’s private namespace.

## Desktop trial

On November 23, 2026 the larger test used a closed, populated Minecraft Java world from Prism Launcher and additional Anvil region fixtures. All transfers used the Buddy HTTP protocol over throttled loopback on this Linux desktop. Timing includes local filesystem and SQLite work. The configured bandwidth is a ceiling in each direction, and the run did not exercise a physical network.

The test covered **10,747,768,680 bytes across 1,410 files**. The real world contributed 2,371,850,067 bytes across 910 files, including 893 region containers. The test added 499 generated region containers alongside it and kept a 21-byte sentinel from the previous revision. The proxy limited each pack transfer to 48 MiB/s.

| Operation | Elapsed | Successful pack payload |
| --- | ---: | ---: |
| Cold scan and upload until interruption | 166.419 s | 39,778,234 bytes sent |
| Resume and publish the new revision | 325.656 s | 10,708,591,297 bytes sent |
| Restore to the second client | 445.528 s | 10,748,369,531 bytes received |
| Change one byte in an existing Minecraft region timestamp and sync | 5.502 s | 586,426 bytes sent |
| Apply that update on the second client | 71.418 s | 586,426 bytes received |

Upload plus retry took 492.075 seconds. The old head remained available after interruption. The retry sent zero chunks previously acknowledged by the server. The table counts successful binary pack bodies including their framing and excludes HTTP overhead and the deliberately rejected request. Independent SHA-256 checks matched every destination file after both restores. The aggregate digest of the final inventory was `bab7e7d0b0ba89118e5a63fd21a33538304d0b1daf59b707a36231712787ffec`. The original world matched its starting inventory at the end.

The 71.418-second update used the original restore implementation. It checked cached chunks across the entire target manifest, scanned the live tree before staging, repeated the scan before installation, then scanned the result. The section below compares that receiver with the optimized build.

On the same unchanged 10 GiB tree, five wrapped launches reached a small Python child in **0.232–0.598 seconds** (median **0.255 seconds**). The entire wrapper including post-exit checks took **5.876–7.819 seconds** (median **5.990 seconds**). The five runs transferred zero chunk bytes. The first timing attempt exposed a progress window that could survive its wrapper and hold output pipes open. These five replacement samples use the corrected release build on a private X11 display. Current versions of the benchmark record both measurements and run with desktop display variables cleared by default. To include the graphical progress window, run in an isolated Xvfb session with `BUDDY_TEST_X11=isolated`.

Minecraft Java 1.21.5 opened a disposable copy of the second client's received world. The game listed the 1.21.1 save, loaded it in single-player, rendered the player's terrain and inventory, then saved and quit through the game menu. The game used a private offline Prism profile and separate working directory. The generated stress fixtures sit outside the playable world.

## Incremental restore comparison

### Linux on Btrfs

The comparison repeated the one-byte timestamp edit in the same 11,304,960-byte Minecraft region. Both client releases used the same server, receiver, 48 MiB/s pack limit and 10,747,768,680-byte fixture as each other. Each pull started with the local state left by a full restore with the original release (`2001e36`). Between runs, the trial restored the private profile checkpoint and the one changed region. The other 1,409 files retained their identities. The original scan index contained 528 entries, and the receiver had received no intervening Buddy sync. The host page cache was left enabled.

| Receiver, in run order | End-to-end pull | Client logical reads | Pack bytes received |
| --- | ---: | ---: | ---: |
| Original | 67.674 s | 72.90 GiB | 586,426 |
| Optimized | 8.632 s | 3.10 GiB | 586,426 |
| Optimized | 4.449 s | 3.10 GiB | 586,426 |
| Original | 56.302 s | 72.92 GiB | 586,426 |

The mean fell from **61.988 s to 6.541 s** (9.48× faster). Logical reads fell **95.75%**. Every pull reproduced the historical SHA-256 inventory across all 1,410 files, and the personal world still matched its starting inventory across all 910 original files.

A second receiver started empty and downloaded the entire world using the optimized build. Its very next sync received the same one-byte edit in **1.510 s**, with **1.95 GiB** of logical reads. After reversing the edit, the trial removed the edited chunk from the disposable cache so it would need downloading again. Reapplying the edit on the now-indexed receiver took **0.660 s**, with **464 MiB** of logical reads. Both forward pulls received exactly **586,426 bytes** and matched the full expected inventory. Linux attributed zero storage-read bytes to the client in both forward pulls, consistent with a warm OS page cache.

### Windows 11 Pro on NTFS

The native Windows trial used two clients over loopback and a warm receiver with **512 MiB in 64 unique 8 MiB files**. Each sample changed one byte, synced, and compared all 64 destination files by SHA-256. In run order the original receiver took 6.1120 s, the optimized receiver took 3.3927 s and 1.9684 s, and the original took 5.4719 s. The means were **5.7920 s and 2.6806 s**, a **54% reduction** for this Windows workload.
