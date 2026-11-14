# Neo — architecture

Neo sends files of any size to one or many people at once, directly between
devices, end-to-end encrypted, with no account and no personal data.

## Pieces

| Crate | Role |
|---|---|
| `neo-core` | Identity, contacts, pairing/linking, backup, discovery, QUIC transfer engine, resume. No UI. Also builds `neo-cli` (headless node). |
| `neo-app` | GPUI desktop client (`neo.exe`). Talks to the engine via commands/events. |

The engine runs on its own small tokio runtime (2–4 threads). The UI receives
events on an async channel and only re-renders when something happens.

## Identity (decided)

There is no sign-up and no personal data. Identity has two layers:

- **Device key:** each device generates an Ed25519 key on first run, wrapped
  in a self-signed certificate used for TLS (`identity.*.der`). Its
  **DeviceId** is the BLAKE3 hash of that certificate.
- **Account key** (one per person, `account.key.der`): an Ed25519 key that
  signs each of the person's devices (an `Attest`, sent in discovery
  announcements and transfer offers). The **AccountId** is the BLAKE3 hash of
  the public key, and its short code (`8Z7N-DZH3-BAKP`) is what people share.
  It's in the header and on the Settings page, also as a QR code
  (`neo://add/CODE`) ready for a future mobile app.
- An attestation only binds an account to a device id, and TLS proves the
  device id. So copying someone's attestation from the network doesn't let
  anyone impersonate them.

### Contacts (pairing)

- Type their code under **Add by code** (or scan the QR code in their
  Settings). Neo never lists strangers, even on the same network. They see
  "X wants to add you as a contact" and accept or decline. On acceptance both
  sides store each other under the names they chose (`contacts.json`).
- Contacts appear by name, even when they're offline (greyed). For each
  contact you can turn on **Accept files automatically**; nobody else's files
  skip the prompt.

### Your devices (linking)

- On the new device, **Settings → Link this device to my identity**, then
  enter your code. Both screens show the same 6-digit number (from both
  device ids). You accept on the existing device only if they match, which
  rules out a different device on the network racing the request.
- The existing device then sends the account key, your name and your contacts
  over the pinned, encrypted connection. Both devices now have the same code
  and contacts, and files between your own devices never prompt.
- Limits for now: contacts are copied once at link time, not kept in sync, and
  there's no "unlink" yet.

### Backup

- **Settings → Backup** writes your account key, name and contacts to a
  file. The file is encrypted with Argon2id (64 MiB, 3 passes) and
  XChaCha20-Poly1305, under a passphrase of 8 or more characters.
  **Restore** on any device makes it act as you again.

### Email alias (moved to M4)

- An optional email alias lets people find you by email. It needs a small
  server to send verification emails, so it ships with the M4 server. It
  remains optional.

## Transport

- **QUIC** (`quinn`) with TLS 1.3. Both sides present their identity
  certificate. The sender pins the recipient's DeviceId; the recipient learns
  the sender's DeviceId from the handshake. Every byte is end-to-end
  encrypted.
- **One UDP socket** (`net/socket.rs`), dual-stack IPv6 + IPv4, on a port
  that stays the same across restarts. QUIC, STUN and NAT keepalive "pokes"
  all share it; STUN replies and pokes are filtered out before QUIC sees them.
- **LAN, contacts only** (`discovery.rs`): devices multicast an
  announcement (`239.255.77.7:47321`) every 2 s, encrypted
  (XChaCha20-Poly1305, random nonce) with a key derived from the account
  code. Only people who know the code can read it: contacts, your linked
  devices, and someone typing your code right now (so adding works on a LAN
  with the internet down). Strangers see unlinkable blobs and Neo lists no
  one else. Contacts on the same network then connect directly. Chosen over
  relying on the DHT's private addresses because it keeps working offline.
  Tests and `neo-cli --discover-all` also send the old readable
  announcement; the app never does.
- Connecting tries every known address of a device at once (LAN, router
  mapping, IPv6, public) and keeps the first that answers.

## Internet, with no server, VPN or port forwarding (M4)

All of this runs automatically. Users never set anything up.

- **Where am I?** (`net/mod.rs`)
  - Global IPv6 addresses.
  - A router port mapping: UPnP first, then PCP (RFC 6887) and NAT-PMP
  (RFC 6886) to the default gateway on port 5351 (`net/portmap.rs`). The
  developer's router answers none of the three.
  - The public IPv4 address and port seen by up to three public STUN servers
  on different IPs (Cloudflare, Google), refreshed every 5 minutes. Comparing
  them gives the NAT type (`net/nat.rs`): **open** (no NAT), **easy** (same
  public port for every destination) or **hard** (a new port per destination,
  "symmetric"), plus the port step when a hard NAT counts up predictably. The
  type goes into the encrypted presence record and into knocks.
  - Private LAN addresses too, for networks that block multicast.
- **Presence** (`net/presence.rs`) lives in the public BitTorrent DHT
  (BEP 44 mutable items; nobody runs it, millions of nodes).
  - Each person has one record, signed by their account key. It lists their
    devices and current addresses, and linked devices merge into it.
  - It is encrypted with a key derived from the account's public key, which
    only contacts (or someone who saw your device on a LAN) know.
  - Neo republishes every 5 minutes and when addresses change.
  - Contacts' records are read every 90 s. Devices updated in the last
    12 minutes show as reachable "over the internet".
- **Codes over the internet:** a second record maps your code to your full
  key and, since 0.2, also carries your devices' sealed addresses, so one
  lookup finds both who and where (old 32-byte records still resolve, via a
  second lookup). "Add by code" and "Link" look on the LAN and over the
  internet at once and take whichever answers first. Anyone with the code
  could overwrite the record, but not forge it, because the answer must hash
  to the code. It's republished with every presence update, starting as soon
  as the DHT is bootstrapped (checked every 3 s); good DHT nodes are saved in
  `dht_nodes.json` to bootstrap faster next time.
- **Fast reads:** DHT reads stream and take the first valid signed answer,
  waiting at most 0.5 s more for a newer one, instead of the whole traversal.
- **Measured** on the public DHT, two fresh profiles on one PC
  (`--no-lan --no-private`; the final connect can't succeed here because the
  router has no hairpin NAT, so "B is knocked" is when the person being
  added learns about it and pokes back):

  | Step (from A typing the code) | 0.1 | 0.2 |
  |---|---|---|
  | Code resolved | 6.4 s (lookup 2.8 s) | 4.0 s (lookup 0.85 s; most of the rest is A's own DHT bootstrap) |
  | Devices' addresses known | 9.2 s (second lookup) | 4.0 s (in the code record) |
  | Knock sent | 18.4 s (after a 6 s failed direct try) | 8.0 s (in parallel) |
  | B is knocked | 29.0 s (15 s polling) | 10.6 s (2 s polling) |

  Time from B starting until its code was findable: 7.1 s vs 7.8 s (DHT
  bootstrap ~3 s plus a 3–5 s write; not faster on a fresh profile, since
  saved nodes only help from the second start).
- **Hole punching:**
  - Contacts send each other an 8-byte "poke" every 15 s, which keeps a path
    open through both routers.
  - For a first contact the other router drops packets from strangers. So the
    caller publishes a "knock" (its addresses, under a key derived from the
    callee's key). The caller knocks at the same time as its first direct
    attempt (for a device it hasn't reached in 10 minutes), not after it
    fails. The callee checks for knocks every 2 s for the first 3 minutes
    after start, then every 5 s, and pokes back for 90 s, while the caller
    keeps retrying for up to 75 s. Later transfers connect right away.
- **Strict routers** (`net/punch.rs`), only when the plain attempt fails.
  The knock carries the caller's NAT type and a random 8-byte session token;
  both sides then run the same planned punch session and probe with
  `NEOPUNCH`+token packets. These are answered and swallowed by `NeoSocket`
  before QUIC sees them, and the first answer wins:
  - Easy ↔ easy: probing each other's known addresses (every 500 ms) is
    enough.
  - Against a hard NAT that counts up: spray its next 256 predicted ports.
  - Against a hard random NAT, from an easy side: the "birthday" trick. The
    hard side opens 256 extra sockets (from 3 s, one probe every 2 s each) at
    the easy side's stable address, and the easy side sprays 4096 random ports
    every 5 ms. One collision opens a path. A win on an extra socket hands
    that socket to a QUIC endpoint of its own (`adopt_socket`, closed after
    about 2 idle minutes); all other extra sockets close as soon as the
    session ends (35 s for the caller, 20 s for the callee).
  - If nothing works, the error says plainly that both networks block direct
    connections and suggests IPv6, another network (a phone hotspot), or a
    router with UPnP/NAT-PMP.
  - No relay of any kind, by decision: no server and no other user's
    connection ever carries the data.
- **Limits:**
  - Both sides behind hard NATs that pick random ports (many carrier-grade
    NATs) still can't connect: the simulator gets through about 1% of the
    time, and hard-predictable against hard-random is about the same.
    IPv6, when both have it, sidesteps NAT entirely.
  - Simulated success rates (500 runs each, `punch_rates` test, port-
    restricted filtering): easy↔easy, easy↔counting, counting↔counting 100%
    at once; easy↔random 100%, median 3.3 s; counting↔random 0.8%;
    random↔random 0.6%.
  - Your IP address is published, encrypted, for your contacts. The existence
    of your record is public.
  - Verified here: the public DHT (code record published in about 5 s,
    looked up in about 3 s), STUN, and the knock arriving through the public
    DHT. NAT detection on the developer's network says "easy" (same public
    port from two STUN servers); its router ignores UPnP, PCP and NAT-PMP.
    Tested on one PC: packet formats, PCP→NAT-PMP fallback against a fake
    router on loopback, the punch runner on real loopback sockets (including
    QUIC over a winning birthday socket), and a NAT simulator running the
    same schedule.
  - Not verified: a real cross-network connection, real symmetric-NAT
    behaviour (only simulated), and PCP/NAT-PMP against a router that
    supports them. These need two networks, ideally one behind a
    carrier-grade NAT (a phone hotspot often is).

Settings shows a one-line connection status (green when contacts can reach
you). `NEO_DEBUG=1` prints diagnostics to stderr.

## Protocol (`neo/6`)

`neo/5` (Neo 0.1) added pausing; `neo/6` adds compressed and delta blocks.
Both sides still offer `neo/5` in the TLS handshake only so a mismatch can be
explained: a new device closes a `neo/5` connection with code `2` and the
reason "The other device has a newer version of Neo. Update Neo on this
computer to reach it." (Neo 0.1 shows that reason as the failure), and tells
its own user that device needs to update. A new device that reaches an old
one fails the send with "<name> has an older version of Neo. Ask them to
update Neo, then try again." No retries either way.

There is one connection per (sender, recipient). The first control frame says what
the connection is for: `Offer` (files), `Pair` (become contacts) or
`LinkRequest` (join an identity).

1. A control stream carries length-prefixed JSON. The sender sends `Offer`
   (each file's name, size and a fingerprint of its path, size and mtime, plus
   the sender's account attestation).
   The recipient replies `Decision`, saying where each file should start,
   which files it already has, and the chunk hashes of old versions it holds
   (see below), and later sends `Complete`.
2. There is one stream per file still needed: a `u32` index, a `u64` start
   offset, then the file in 1 MiB blocks. Each block is a tag (raw, zstd, or
   pieces), its payload, and the BLAKE3 hash of its original bytes
   (`wire.rs`). The receiver rebuilds and checks every block before counting
   it and writes into `name.neopart`, then renames the file. Peer-supplied
   paths are sanitized.
3. Error codes on close: `1` cancelled, `2` failed (with a reason, no retry),
   `3` superseded by a reconnect, `6` reconnect now (an old copy used for
   delta turned out to differ from its index).

## Compression, delta and congestion control

All automatic; the received file is always byte-identical to the original,
because every block's BLAKE3 hash is over the original bytes and is checked
after decompressing and rebuilding.

- **Compression** (`wire.rs`): each 1 MiB block is compressed with zstd
  level 1 on the sender's reader thread (one context per stream) and
  decompressed on the receiver's writer thread. A block that doesn't shrink
  by 8% goes raw; after two misses a file goes raw and re-tries one block in
  64. Names like `.zip`/`.mp4`/`.jpg` start raw but are still sampled. The
  reader measures itself every 16 blocks: if the network never held it back
  (CPU-bound, as on a fast LAN), it tries 16 blocks raw and keeps whichever
  way moved more bytes per second, checking again later. Speed limits count
  bytes on the wire; progress and speed shown are in original bytes, plus a
  subtle "saved X%". In a swarm the sender compresses once and recipients
  forward blocks still compressed. `NEO_COMPRESS=0` turns it off
  (benchmarks).
- **Send only what changed** (`delta.rs`), direct sends only: if the receiver
  already has a file at the same path in its downloads folder (usually the
  previous version), it cuts that file into FastCDC chunks (~64 KiB average,
  BLAKE3, cached by path, size and mtime) and puts the 16-byte hashes in its
  `Decision`. The sender chunks the new file the same way while streaming;
  known chunks travel as ~24-byte references, the rest as (compressed) data.
  Blocks stay 1 MiB, so resume is unchanged (a resumed file continues
  without delta). The old file is opened read-only and locked against writes
  while in use, is never modified, and the new version is saved next to it
  ("name (1)") like any re-send. Skipped below 4 MiB, without a base, in
  swarms (each recipient may hold a different old version) and above
  250,000 chunks (~16 GB) per batch. If an old copy changed while keeping
  its timestamp, a block fails its check, the cache entry is dropped and
  the transfer reconnects and finishes without it. The row then reads
  "Updated: sent only X of Y". `NEO_DELTA=0` turns it off.
- **Congestion control:** BBR for every connection (`tls::transport`);
  `NEO_CC=cubic` switches back. Measured through a simulated 50 Mbit/s
  bottleneck (1 BDP drop-tail buffer, `tests/cc.rs`), in Mbit/s:

  | Link | BBR | Cubic |
  |---|---|---|
  | RTT 100 ms, no loss | 48.8 | 48.9 |
  | RTT 100 ms, 1% loss | 48.3 | 1.2 |
  | RTT 100 ms, 3% loss | 48.7 | 0.8 |
  | RTT 200 ms, 1% loss | 47.5 | 0.6 |

  Cubic's 1.2 Mbit/s at 1% loss matches the textbook TCP estimate
  (MSS/RTT · 1.22/√p), so the simulation is sane. Two flows sharing the
  link: BBR+BBR split 33/19 (Jain fairness 0.93; 0.98 at 1% loss),
  Cubic+Cubic 40/9 (0.70). **BBR is not fair to Cubic:** a BBR flow next to a
  Cubic one took 53 vs 0.1. So on a shared home uplink, a big Neo send can
  crowd out other TCP traffic; the upload limit in Settings is the remedy
  (and `NEO_CC=cubic`). quinn's BBR is marked experimental; it carried a
  3 MB transfer byte-perfect through 3% loss and 100 ms RTT in the test.

Measured on this PC (loopback; `tests/wire_delta.rs`, ignored benchmarks):

- 28 MB of Rust source over a 3.5 MB/s upload: 8.0 s raw, 1.3 s compressed
  (16% of the bytes, 21 MB/s effective).
- 200 MB of text with no cap: 0.67 s raw, 0.59 s with adaptive compression,
  so compression doesn't slow a fast link down.
- A 300 MB file edited (20 KB inserted, 300 KB deleted, 100 KB appended) and
  sent again: 0.6 MB on the wire. From Send to done: 1.14 s for the first
  full send, 0.72 s for the re-send including the receiver chunking its old
  copy, 0.46 s once cached, and 0.70 s at a 3.5 MB/s upload (a full send
  would take ~86 s).

## Swarm fan-out (several recipients)

Whenever one send goes to 2 or more devices, recipients pass blocks on to
each other, so the sender uploads each block about once instead of once per
recipient. It is fully automatic, with no settings or prompts (`swarm.rs`).

- **Sender:**
  - Reads and hashes each 1 MiB block once.
  - Gives each block to one recipient: whichever recipient's connection has
    room next, so fast recipients take more.
  - Sends every block's BLAKE3 hash to *every* recipient on a small hash
    stream.
- **Recipients:**
  - Forward each block they got from the sender to the co-recipients they're
    linked with.
  - Accept a block from a peer only if it matches the sender's hash, and
    never forward peer blocks again, so blocks can't loop.
  - Write blocks in any order. Resume still keeps each file's contiguous
    verified prefix.
- **Links between recipients:**
  - Ordinary pinned QUIC/TLS connections, so everything stays end-to-end
    encrypted.
  - The sender gives each recipient a roster (device ids and addresses only:
    no names or accounts) and a per-batch secret. A link is accepted only with
    a proof derived from that secret, for that batch, and only after that
    recipient accepted the send.
  - No contact relationship is created.
- **Fallbacks (no errors):**
  - Recipients tell the sender which peers they're linked with. For pairs that
    aren't linked, the sender also sends the block directly.
  - A push to a slow peer is dropped instead of waited for, so a slow
    recipient never holds up the others.
  - At the end ("endgame") recipients ask the sender for anything they still
    miss.
  - A recipient that reconnects (or restarts) gets every already-distributed
    block it lacks straight from the sender.
- **Measured** on one PC: a 512 MB file, sender upload capped at 40 MB/s with
  `NEO_SEND_LIMIT_MBPS`.

  | Recipients | Direct | Swarm |
  |---|---|---|
  | 3 | 40.3 s (13 MB/s each) | **13.5 s (40 MB/s each)**, 3.0× faster |
  | 5 | 67.1 s (8 MB/s each) | **13.6 s (39 MB/s each)**, 4.9× faster |
  | 3, one can't link to anyone | n/a | the other two 20.2 s, the isolated one 26.9 s (gets its own copy) |
  | 3, one killed at 40% and restarted | n/a | others 14.4 s; it resumed at 40.8% |

  - All copies were hash-identical.
  - Peak memory: sender 28 MB, each receiver about 13 MB.
  - Uncapped on one PC (4.6 s direct vs 5.7 s swarm) swarming doesn't help,
    because sender and recipients share one CPU and loopback; the gain comes
    when the sender's upload is the bottleneck, which is the normal case.
- Test knobs: `NEO_SWARM=0` forces direct sends; `NEO_SWARM_NO_LINKS=1`
  refuses links (simulates unreachable pairs); `NEO_SEND_LIMIT_MBPS` caps the
  sender's upload.

## Resume

- The receiver keeps one small record per incoming batch in
  `%APPDATA%\Neo\partials\`. For each file it holds where the `.neopart` is,
  how many bytes are verified, and whether the file is finished. The record is
  saved at most twice a second while something changes, and deleted when the
  batch completes. Records untouched for a week are pruned along with their
  partial files.
- **Dead peers** are detected within about 6 s (keep-alive every 2 s). The
  UI marks a transfer "stalled" once no bytes have moved for 2.5 s.
- **Connection drops:** the sender shows "reconnecting…" and retries with
  backoff (1 s up to 15 s) for 5 minutes after the last progress. It follows
  the recipient if its address changes. The recipient keeps the card, marked
  *Interrupted*.
- **Either app restarts:** sending the same files again matches the record.
  The recipient accepts automatically because it already agreed to this
  batch, and the transfer continues from the last verified block. If the same
  sender reconnects before the old connection has timed out, the stale one is
  closed first, so two writers never touch one file.
- Data isn't fsynced during a transfer, because that stalled the pipeline. If
  the PC rebooted since the record was saved, unfinished files restart from
  zero, since unsynced data may be gone. App crashes and network drops lose
  nothing.
- Files under 8 MiB restart rather than resume mid-file, but finished files
  in a batch are never sent again.

## Speed and resources

- Files are streamed in 1 MiB blocks and never loaded whole. Memory stays flat
  whatever the file size.
- **Pipelined:** a reader thread reads and hashes blocks two ahead of the
  network. On the receiving side, a writer thread verifies and writes them.
  Disk, hashing and encryption overlap instead of taking turns.
- **Parallel files:** up to 3 files per recipient are in flight at once, which
  helps folders of small files.
- **Fan-out:** each recipient gets its own connection and task, so a slow
  recipient never stalls the others. The OS page cache serves the file from
  disk once for all of them.
- **Measurements** (one PC, loopback; sender and two receivers all on a Ryzen
  5600X):
  - 2.2 GB to 2 recipients at the same time took about 8.2 s, about 269 MB/s
    each. M1 took 8.6 s (259 MB/s).
  - 2000 small files (192 MB) to 2 recipients took 6.6 s, against 8.8 s for
    M1. That M1/M2 pair was measured back to back with a game running.
  - Peak memory: sender about 55 MB, each receiver about 23 MB.
- Idle: the engine uses 0% CPU. The progress ticker and the resume-record
  saver only run while a transfer is active.
- The GPUI window idles at about 0–0.3% of a core (about 60 MB working set).
  GPUI is vendored with a small patch so its Windows vsync thread sleeps
  unless something needs drawing; stock GPUI used about 1.4%. See
  `vendor/gpui/NEO-PATCH.md`.

## Gofile tab

A separate tab (**Send | Gofile** in the header) uploads to the user's *own*
Gofile account, so anyone can download from Gofile's normal web page
(`neo-core/src/gofile`, `neo-app/src/ui/gofile.rs`).

- **Account:**
  - A real (email) Gofile account is required; guest tokens are refused.
  - To get one, the user either opens gofile.io, or asks Gofile (from the
    tab) to email a sign-in link.
  - The user pastes the API token from their gofile.io profile. Neo checks it
    with `/accounts/getid` and `/accounts/{id}`.
  - The token is stored encrypted with Windows DPAPI (`gofile.token`, only
    this Windows user can decrypt it); it's never stored in plain text.
  - The tab shows email, tier, storage, file and folder counts, and any
    limit/traffic fields Gofile returns. Disconnect forgets the token.
- **Uploads:**
  - Each upload becomes one folder in the account, because Gofile's
    password, expiry and description apply to folders. Subfolders are
    recreated with `createFolder`.
  - Files stream from disk as multipart with a known length (never
    buffered), with progress, speed, time left and cancel. Neo's MD5 must
    match Gofile's.
  - A cancelled or failed upload deletes its half-made folder.
  - The folder's download page is the link: Copy link, QR code, or send it
    to a Neo contact.
- **Sharing options:** public or private, a password (only its SHA-256 is
  sent; the text is never stored), an expiry date (on free accounts Gofile
  then deletes the folder), and a description.
- **My uploads:**
  - A local record in `gofile.json`, since free accounts can't list folders
    through the API.
  - On Premium, the real root-folder listing replaces it, keeping Neo's
    extra details.
  - Each item can be renamed, deleted (after a second "Delete permanently?"
    click), have its link copied or shown as a QR code, or be sent to a
    contact.
- **Sending a link to a contact:** a `ShareLink` message over the normal
  pinned connection. It's shown only if it comes from a contact or one of
  your own devices, and only for `https://gofile.io/` links.
- **Being a good API citizen:**
  - Requests are sequential.
  - The JSON `status` field is always checked.
  - `error-rateLimit` backs off (3 s, then 9 s) before giving up.
  - **No network while idle:** nothing is fetched until the user connects,
    uploads or edits something, or opens the tab (which refreshes account
    details at most every 5 minutes).
  - Idle with the tab open: about 0.08% CPU, 62 MB.
- **Testing:**
  - `tests/gofile.rs` runs the whole flow against a local mock of the API:
    guest refusal, connect, encrypted token, folder upload with options and
    subfolders, MD5, rename, delete, cancel with clean-up, disconnect.
  - `NEO_GOFILE_API` and `NEO_GOFILE_UPLOAD` point Neo at a mock.
  - `neo.exe` test flags: `--open-gofile`, `--demo-gofile-upload PATH`,
    `--demo-gofile-stage PATH`.
  - Not yet tried against a real Gofile account.

## Tray and notifications

- A notification-area icon, written directly against Win32 (`tray.rs`) on
  GPUI's own message loop, so there's no extra thread or event loop. Left
  click opens Neo. The right-click menu has "Open Neo" and "Quit Neo". The
  icon comes back if Explorer restarts.
- Closing the window hides Neo to the tray, so it keeps receiving. This is a
  setting, on by default. The first time, a notification explains where Neo
  went.
- Notifications use the tray's balloon API, which Windows 10/11 shows as
  toasts titled "Neo" (from the exe's version info), with no installer or app
  registration. They appear only when Neo isn't the window in front:
  incoming offers ("X wants to send you …"), received files, and failed
  receives. Clicking one opens Neo. Do Not Disturb hides them, like any
  app's notifications.
- `assets/neo.ico` is drawn by `assets/make_icon.py` and embedded by
  `crates/neo-app/build.rs` as resource 1, which GPUI uses for the window and
  taskbar icon.

## Settings

- `%APPDATA%\Neo\settings.json` stores the device name, the download folder
  and whether to keep running in the tray.
  Both are editable on the Settings page, and changes apply immediately; the
  new name is announced on the next discovery tick.
- **Speed limits** (`upload_limit` / `download_limit`, MB/s, empty = no
  limit) cap the total across everything Neo sends or receives: direct and
  swarm sends, blocks forwarded within a swarm, and Gofile uploads. There is
  one shared bucket per direction (`limit.rs`) that paces 16–256 KiB pieces on
  a virtual clock, so there are no saved-up bursts. Receiving is limited by
  reading QUIC streams more slowly, so flow control slows the sender down and
  nothing is dropped. Changes apply to running transfers at once. Measured on
  loopback: a 10 MB/s download cap and a 15 MB/s upload cap both hold within
  10%.

## Pause and resume

- Every transfer has a `Gate` with two flags, paused here and paused by the
  other side. Data waits at the gate between 256 KiB pieces, so a pause lands
  in milliseconds, not after a whole 1 MiB block. Changes travel as `Pause`
  frames on the control stream and wake waiting tasks through a `watch`
  channel, with no polling.
- The connection stays up on QUIC keepalives while paused. If it drops
  anyway, the sender doesn't redial until resumed, then continues from the
  last verified block.
- In a swarm, a paused recipient simply takes no new blocks, so the others
  carry on.
- Measured on loopback (`pause_and_resume_take_effect_quickly`), counting
  until the receiver's file stops or starts growing:
  - receiver pause: stops in 16 ms, at most one block in flight
  - sender pause: stops before the next block lands
  - resume from either side: data flows again in 30–60 ms
- **Gofile:** pausing stalls the HTTP body at once. Gofile can't resume a
  half-sent file, so if its server drops a long-stalled connection, that one
  file starts over (into the same folder, with the same link). Finished files
  are never sent again. The row says so while paused.

## Updates

- **Check** (`update.rs`): one HTTPS request to GitHub's latest-release API
  3 s after start and every 12 hours; nothing else. Failures (offline,
  rate-limited) are silent. `NEO_UPDATE_URL` points it at a mock for tests.
- **Prompt:** "Neo vX.Y.Z is available" with the first line of the release
  notes, **Update now** and **Later**. Later hides it until the next start
  (a newer version brings it back) and leaves an **Update to vX** pill in the
  header. With transfers running, Update now offers to wait for them
  ("Update when they finish"); interrupted transfers resume anyway.
- **Trust:** each release ships `neo.exe.sig`, an Ed25519 signature over
  `"neo-release-v1" || BLAKE3(neo.exe)`. The public key is built in
  (`update::RELEASE_KEY`); the private key lives only on the developer's PC
  (`%APPDATA%\Neo-release\release.key`, never in the repo). A download that
  doesn't verify is deleted, never run. So a hijacked GitHub account or a bad
  download can't push malware.
- **Install:** download to `neo.update.part` next to the exe, verify, rename
  `neo.exe` to `neo.old.exe` (Windows allows renaming a running exe), move the
  new file in, start it with `--updated` and quit. The new copy waits for the
  old one to exit and deletes `neo.old.exe`.

### Releasing a version

1. Bump `version` in the workspace `Cargo.toml` (e.g. `0.2.0`).
2. `./build.ps1 build -Release` (or `cargo build --release`).
3. `target/release/neo-cli.exe sign-release target/release/neo.exe` writes
   `neo.exe.sig`. The first run creates the key; it prints the public key,
   which must equal `update::RELEASE_KEY` (and warns if not). Back the key up
   somewhere safe: losing it means users must download the next version by
   hand; leaking it means someone could sign builds.
4. `gh release create v0.2.0 target/release/neo.exe target/release/neo.exe.sig --notes-file NOTES.md`
   The first line of the notes shows in the prompt.

## Offline recipients (decided)

- **Direct sends** need both devices online at the same time. The sender
  queues the send and delivers it when the recipient appears (M5).
- **File uploads** (M5) are the exception. The file is stored encrypted on a
  server so the recipient can pick it up later.

## Milestones

- **M1 (done):** workspace, GPUI window, LAN discovery, multi-recipient QUIC
  transfer with per-recipient progress, accept/decline, cancel, integrity
  check, `neo-cli`.
- **M2 (done):** resumable transfers (verified per 1 MiB block, survive
  network drops and restarts on either side), automatic reconnect, pipelined
  disk I/O, parallel small files, and settings (device name, download
  folder).
- **M2.5 (done):** tray icon with menu, close-to-tray, Windows
  notifications for incoming offers and received files, an app icon, and the
  GPUI idle-CPU patch.
- **M3 (done):** account identity (one per person, signing each device),
  contacts with mutual consent (one click on a nearby device, or by code),
  per-contact auto-accept, linking your own devices with a confirmation
  number, an encrypted identity backup, and a QR code of your code.
- **M4 (done):** internet reach with no server: DHT presence and code
  records, STUN, UPnP, IPv6, hole punching with keepalives and knocks, and a
  connection status in Settings.
- **M4.5 (done, no relay):** strict-router traversal: NAT type detection,
  PCP/NAT-PMP, predicted-port spraying and the bounded birthday trick. A
  relay was ruled out. The optional email alias needs a server to send
  emails, so it is parked unless you want to host one.
- **M5:** queued sends to offline contacts, uploads with encrypted server-side
  pickup, installer, auto-update.

## Follow-ups noted

- Offer the GPUI vsync patch upstream to Zed, so we can stop vendoring.
- Start with Windows (an opt-in setting) so Neo can receive files without
  being opened first.
- Real two-PC LAN measurements; so far every number is from loopback on one
  machine.
- Keep contacts in sync between linked devices, and add "unlink this
  device".
