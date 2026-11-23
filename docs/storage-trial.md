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

The one-byte change illustrates a remaining cost. Applying a remote change on this machine took over a minute despite transferring under 0.6 MB. The receiver had just restored the full world and had its chunks cached. During restore Buddy deliberately reads the live tree with file-identity shortcuts disabled to check for unexpected edits, then reads the final tree again to verify the result. This trial measured end-to-end time without profiling those steps separately.

On the same unchanged 10 GiB tree, five wrapped launches reached a small Python child in **0.232–0.598 seconds** (median **0.255 seconds**). The entire wrapper including post-exit checks took **5.876–7.819 seconds** (median **5.990 seconds**). The five runs transferred zero chunk bytes. The first timing attempt exposed a progress window that could survive its wrapper and hold output pipes open. These five replacement samples use the corrected release build on a private X11 display. Current versions of the benchmark record both measurements and run with desktop display variables cleared by default. To include the graphical progress window, run in an isolated Xvfb session with `BUDDY_TEST_X11=isolated`.

Minecraft Java 1.21.5 opened a disposable copy of the second client's received world. The game listed the 1.21.1 save, loaded it in single-player, rendered the player's terrain and inventory, then saved and quit through the game menu. The game used a private offline Prism profile and separate working directory. The generated stress fixtures sit outside the playable world.
