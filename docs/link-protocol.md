# The app link protocol, version 1

The wire format between the desktop app and the optional phone app ("the app route"). It is written so that `crates/link` can be implemented, tested and fuzzed from this page alone. The design reasons are in the vault's `design/companion-app.md` §3–§4. Status: **spec for Phase 1, nothing implemented yet** (17 Oct 2026).

The words MUST, MUST NOT and SHOULD are used in their RFC 2119 sense.

## 1. Overview

- **Roles.** The phone is the server: it listens on TCP and advertises over mDNS. The PC is the client: it finds the phone and opens the connection, so Windows needs no inbound firewall rule for the link.
- **One TCP connection per phone.** It carries one of two exchanges, chosen by the PC's first bytes:
  - **pairing** (§4), run once, which ends with both sides holding the other's long-term X25519 public key;
  - **session** (§5), every connection after that: a Noise `KK` handshake with those pinned keys, then framed messages (§6).
- **The content is today's protocol.** Control frames carry the same JSON objects as the adb route's `tpl_ctl` lines. Audio frames carry the same s16le mono 48 kHz samples as `tpl_mic`, in 10 ms frames with a sequence number. `AppTransport` on the desktop turns them back into the two byte streams that `phone.rs` and `mic.rs` read today.

### Conventions
- Integers are **unsigned, big-endian** unless stated. Audio samples are s16 **little-endian**, as on the adb route.
- `||` is concatenation. `"text"` is the ASCII bytes of the text, with no terminator.
- `phone_id` and `pc_id` are **16 random bytes** each, made with a CSPRNG at first launch (phone: per app install; PC: per Windows user). Where they appear as text they are 32 lowercase hex characters.
- Hash: SHA-256. MAC: HMAC-SHA-256. KDF: HKDF-SHA-256 (RFC 5869). AEAD: ChaCha20-Poly1305 (RFC 8439), 32-byte key, 16-byte tag. An AEAD nonce is 4 zero bytes || the message counter as a u64 **little-endian** (the Noise convention).
- `PROTO = 1`.

## 2. Discovery

### mDNS / DNS-SD
- Service type: **`_tpl._tcp.local.`**
- Instance name: **`TPL-` followed by the first 8 hex characters of `phone_id`**, for example `TPL-1a2b3c4d`. The device name is never advertised.
- SRV: the phone's host name and its listening port. The phone binds **TCP 27185** when it is free and otherwise any free port; **the SRV port is authoritative**, and the PC never assumes 27185.
- TXT keys (each `key=value`, as DNS-SD requires):

  | Key | Value | When |
  | --- | --- | --- |
  | `id` | `phone_id`, 32 hex characters | always |
  | `proto` | `1` | always |
  | `pair` | `1` | **only while the phone's pairing window is open** (§4.1); the key is absent otherwise |

- The phone registers the service (Android `NsdManager`) only while its link service runs, and re-registers after any network change.
- The PC browses with the `mdns-sd` crate. A TXT record without a valid `id`, or with a `proto` the PC does not support, is listed as "needs a newer app" and never connected to.

### Address order, for a phone that is already paired
The PC tries these in order and uses the first that completes a Noise handshake (§5), with a 3 s connect timeout each:
1. the address and port of the **last successful session** with this `phone_id`;
2. the address mDNS currently reports for this `phone_id`;
3. **when a Bluetooth PAN adapter is up, its IPv4 default gateway**, on the last known port, then on 27185 (the 5G-only case: the phone is the PAN's gateway, and no mDNS is needed);
4. an address typed by hand in Settings.

The identity check is the handshake, never the address: a different phone at a remembered address fails the Noise prologue (§5.2) and is skipped.

## 3. Wire framing and the preamble

### 3.1 Records
After the preamble, **every message in both directions is a record**: a **u16 length** followed by that many bytes. A length of 0 is invalid. This holds for pairing messages, Noise handshake messages and Noise transport messages. The u16 caps every record at 65 535 bytes, the Noise message limit.

A receiver MUST read the full record before acting on it, MUST close the connection on EOF part-way through a record, and MUST close it when a record arrives that is larger than the current state allows (each state below gives its limit).

### 3.2 Preamble (cleartext, PC first)
The PC's first 8 bytes:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | magic `"TPLK"` |
| 4 | 1 | `PROTO` (1) |
| 5 | 1 | mode: `1` = pairing, `2` = session |
| 6 | 2 | reserved, MUST be 0 |

In **session** mode the preamble is followed directly by **`pc_id`** as 16 raw bytes (no record length), so the PC's cleartext opening is **24 bytes** and the phone can select the pinned key. Pairing mode sends no id here: the opening is the 8 bytes alone, and the id arrives in `PairInit` (§4.2). Records (§3.1) start after the phone's status.

The phone answers with 8 bytes: `"TPLK"`, `PROTO`, a **status** byte, and 2 reserved zero bytes. On any status other than 0 it closes the connection after sending.

| Status | Meaning | PC shows |
| --- | --- | --- |
| 0 | OK, continue | |
| 1 | unsupported `PROTO` or a bad magic/reserved field | "Update the phone app" |
| 2 | busy: a pairing is already in progress | "The phone is pairing with another PC" |
| 3 | session mode, `pc_id` not paired | "This phone was unpaired. Pair again." |
| 4 | rate-limited (§7) | "Too many attempts; wait a minute" |
| 5 | pairing mode, no pairing window open | "Open Pair with a PC on the phone first" |
| 6 | pairing mode, this window's code is used up (3 wrong tries or expired) | "Get a new code on the phone" |

The status byte is **unauthenticated**. The PC treats it as a hint for the message it shows and nothing else: it never deletes a pin because of status 3. Deleting a pin only happens through Revoke (§8). A LAN attacker who forges a status can only deny service, which an on-path attacker can always do.

The phone MUST receive the complete preamble within **2 s** of accepting the connection, or close it.

## 4. Pairing

### 4.1 The pairing window (phone side)
- The user opens **Pair with a PC** in the app. The phone makes:
  - a **6-digit code**: a uniformly random integer in 0–999 999 from a CSPRNG (rejection sampling, no modulo bias), zero-padded to 6 ASCII digits;
  - a **16-byte `window_nonce`**, fresh for every window.
- It shows the code as `123 456` and, under it, **"Phone ID 1a2b-3c4d"** (the instance-name suffix), so the user can match the phone in the PC's list.
- It advertises `pair=1` and accepts pairing-mode connections.
- **The window closes**, and its code and nonce are erased, at the first of:
  - **120 s** after it opened;
  - a successful pairing;
  - the **third try** (a try is defined below);
  - the user leaving the screen.

  A new window always has a new code and a new nonce.
- **One pairing at a time.** While one pairing connection is between `PairInit` and its end, every other pairing-mode connection gets status 2.
- **A try is each `SpakeA` message the phone accepts**, whatever happens after it. The counter is per window. Counting at `SpakeA`, not at a failed confirmation, means an attacker who aborts after seeing `SpakeB` has still spent a try. (SPAKE2 gives that attacker nothing to test a guess against offline; this rule stops them guessing online for free.)

### 4.2 Messages
Mode 1. After the preamble and status 0, the eight messages below are exchanged as records (§3.1), strictly in this order. **Each message MUST arrive within 10 s** of the previous one, and **the whole exchange within 30 s**; otherwise the side waiting closes the connection. Any malformed message (wrong length, a bad field) closes the connection; on the phone it still counts as a try if `SpakeA` was accepted.

| # | Direction | Name | Content (exact layout) | Size |
| --- | --- | --- | --- | --- |
| P1 | PC → phone | `PairInit` | `pc_id` (16) | 16 |
| P2 | phone → PC | `PairHello` | `phone_id` (16) \|\| `window_nonce` (16) | 32 |
| P3 | PC → phone | `SpakeA` | SPAKE2 message of side A: the side byte `0x41` \|\| a 32-byte Ed25519 point, exactly as the crate emits it | 33 |
| P4 | phone → PC | `SpakeB` | SPAKE2 message of side B: `0x42` \|\| a 32-byte point | 33 |
| P5 | PC → phone | `PcKey` | AEAD(`k_pc`, counter 0, plaintext = `pc_static` (32) \|\| `pc_name`, AD = `TH4`) | 48 + name, ≤ 128 |
| P6 | phone → PC | `PhoneKey` | AEAD(`k_phone`, counter 0, plaintext = `phone_static` (32) \|\| `phone_name`, AD = `TH5`) | 48 + name, ≤ 128 |
| P7 | PC → phone | `PcConfirm` | AEAD(`k_pc`, counter 1, empty plaintext, AD = `TH6`) | 16 |
| P8 | phone → PC | `PhoneConfirm` | AEAD(`k_phone`, counter 1, empty plaintext, AD = `TH7`) | 16 |

- `pc_static` and `phone_static` are each side's long-term **X25519 public key** (§4.4).
- `pc_name` and `phone_name` are UTF-8 display names of **at most 80 bytes**, for example `ETHAN-LAPTOP` and `Ethan's Galaxy S24 Ultra`. They are shown in the paired-devices lists only; nothing trusts them.
- The PC checks in P2 that `phone_id` equals the `id` it saw in the TXT record of the phone the user picked, and aborts otherwise.

### 4.3 SPAKE2, the transcript and the keys
**SPAKE2.** The RustCrypto `spake2` crate (0.4.x), Ed25519 group, **asymmetric mode**: the PC is side A, the phone side B. The same crate runs on both sides, because `crates/link` is compiled into the phone's `.so` too.
- `password` = the 6 ASCII digits.
- `id_A` = `"TPL-PAIR-v1/pc/"` \|\| hex(`pc_id`) \|\| `"/"` \|\| hex(`phone_id`)
- `id_B` = `"TPL-PAIR-v1/phone/"` \|\| hex(`phone_id`) \|\| `"/"` \|\| hex(`pc_id`)

  Both identities name **both roles, both ids and the protocol version**, so a key from this exchange can't be reused in another role, between other devices or in another version.
- **Both sides pass the identities in the same order, A first**: the PC calls `Spake2::<Ed25519Group>::start_a(&Password::new(code), &Identity::new(id_A), &Identity::new(id_B))` and the phone calls `start_b` with the **same three arguments in the same order**. Swapping them on one side yields different keys and every pairing fails.
- Each side's output is the 32-byte key `K`.
- **Why the PC may send P5 first.** A rogue device that impersonates the phone receives `SpakeA` and then P5, but it can't test code guesses against P5 offline: computing `K` for a guessed code needs the discrete log of the crate's `N` constant (SPAKE2's security argument). Each impersonation is one online guess, and the user, who matches the "Phone ID" on both screens, is the check against picking a rogue phone in the first place.

**Transcript hash.** `TH0` = SHA-256(`"TPL-PAIR-v1 transcript"` \|\| the PC's 8-byte preamble \|\| the phone's 8-byte status). Then for n = 1 … 8, over message `Pn`'s record payload (without its length prefix): `THn` = SHA-256(`TH(n-1)` \|\| u16 length of `Pn` \|\| `Pn`). So `TH4` covers the preamble, the status and P1–P4; `TH5` adds P5's ciphertext; and so on.

**Keys.** `HKDF-SHA-256(salt = window_nonce, IKM = K, info = "TPL-PAIR-v1 keys" || TH4, L = 64)`, split into `k_pc` (first 32 bytes, PC → phone) and `k_phone` (last 32 bytes, phone → PC). Binding `TH4` into the expansion means the keys belong to this exact exchange.

**What each check proves:**
- **P5 decrypts** on the phone: the PC derived the same `K`, so it knew the code. If it fails, the phone closes without sending anything more, and the try already counted.
- **P6 decrypts** on the PC: the phone knew the code too. Its AD, `TH5`, includes P5, so it also authenticates `pc_static` as the phone received it, and the plaintext carries `phone_static`.
- **P7** (AD = `TH6`) proves the PC holds a transcript containing **both** static keys. On it, **the phone pins** `pc_id` → `pc_static` and closes the pairing window.
- **P8** (AD = `TH7`) tells the PC the phone pinned. On it, **the PC pins** `phone_id` → `phone_static`.

**A mismatch anywhere aborts with no detail**: the side that fails closes the connection. The PC tells the user only "That code didn't work". It never says which check failed.

If the connection breaks after P7 but before P8, the phone has pinned and the PC does not know it. The PC keeps `phone_static` from P6 as a **provisional pin** and tries one session (§5) at once. If that handshake succeeds, the phone had pinned, and the PC makes the pin permanent. If it fails, the PC discards the provisional pin and asks the user to pair again. A phone that sees a pairing from an already-paired `pc_id` replaces the old pin when the new pairing succeeds.

### 4.4 Long-term keys and storage
- **Phone.** One X25519 key pair, made on first launch. The private key is kept in app-private storage, encrypted with an AES-256-GCM key that never leaves Android Keystore (no user-authentication requirement, so the link works before the user opens the app). Pins (`pc_id`, `pc_static`, `pc_name`, paired-at) are stored the same way.
- **PC.** One X25519 key pair per Windows user, and the pins (`phone_id`, `phone_static`, `phone_name`, paired-at, last address), in app data protected with **DPAPI** (`CryptProtectData`, current user).
- Keys are never logged and never leave the device. Clearing the phone app's data makes a new key pair and a new `phone_id`, so every PC needs to pair again; the PC sees status 3 or a failed handshake and says "This phone was unpaired".

## 5. Session

### 5.1 Handshake
Mode 2. Preamble \|\| `pc_id`; the phone looks up the pin for `pc_id`:
- not found: status 3, close (and it counts as a failed handshake for §7);
- found: status 0, then the Noise handshake.

**`Noise_KK_25519_ChaChaPoly_SHA256`** (Noise rev 34), the `snow` crate, PC = initiator, phone = responder. Each side uses its own static key pair and the pinned remote static key. KK's pattern:

```
KK:
  -> s
  <- s
  ...
  -> e, es, ss
  <- e, ee, se
```

- Message 1 (PC → phone) and message 2 (phone → PC) are records (§3.1). **Both handshake payloads MUST be empty**, so each message is **exactly 48 bytes** (a 32-byte ephemeral key and a 16-byte tag over the empty payload); any other length is a handshake failure.
- **The session counts as authenticated only when the phone has decrypted the PC's first transport message**, which MUST be a `ping` (§6.4) sent straight after message 2 is processed, within 5 s. Until then the phone treats the connection as unauthenticated (§7) and acts on nothing. This matters because the responder finishes KK before it knows the initiator is live: a replayed message 1 gets a message 2 back, but the replayer can't produce a valid transport message, so it can never trigger the replacement rule below.
- Fresh ephemeral keys on both sides mean **every session has new transport keys**. Stealing a static private key later does not decrypt a recorded session: KK's transport keys depend on the `ee` exchange, so they have forward secrecy once message 2 is processed.
- The phone MUST finish the handshake within **5 s** of status 0, or close.
- A handshake failure (a bad MAC, a wrong key, a bad prologue) closes the connection with nothing sent, and counts for §7.

### 5.2 Prologue
`prologue` = `"TPL-LINK-v1"` \|\| u8 `PROTO` \|\| `phone_id` (16) \|\| `pc_id` (16)

The PC builds it from the phone id it **meant** to reach (from its pin) and the phone from its own id. So a handshake meant for another phone, another PC or another protocol version fails, even if the static keys matched by accident.

### 5.3 After the handshake
- Split into two `CipherState`s as Noise specifies. Each transport message is one record, decrypted before anything in it is parsed.
- Nonces are Noise's 64-bit counters. At 100 audio frames a second they cannot wrap in practice; a sender that reaches 2^64 − 1 MUST close the session instead.
- **One session per phone at a time.**
  - A new session from the **same** `pc_id` replaces the old one: once the new session is authenticated (§5.1), the phone sends `bye` (reason 1) on the old one and closes it. This is the "one owner" rule for a laptop that reconnects.
  - A session from a **different** paired PC while one is live is authenticated and then gets `bye` (reason 2, busy). The first PC keeps the phone.

## 6. Frames (inside the session)

Every decrypted transport message is exactly one frame: **a type byte, then the payload**.

| Type | Name | Direction | Payload | Payload size |
| --- | --- | --- | --- | --- |
| 1 | control | both | one JSON object, UTF-8 | 2 – 16 384 bytes |
| 2 | audio | phone → PC | `seq` u32 \|\| `capture_us` u64 \|\| 480 samples s16le | **exactly 972** |
| 3 | blob | phone → PC | `blob_id` u32 \|\| `index` u16 \|\| `count` u16 \|\| `total` u32 \|\| data | 13 – 32 780 (12-byte header + 1 – 32 768 bytes of data) |
| 4 | ping | PC → phone | `nonce` u32 \|\| `pc_us` u64 | exactly 12 |
| 5 | pong | phone → PC | `nonce` u32 \|\| `pc_us` u64 \|\| `phone_us` u64 | exactly 20 |
| 6 | bye | both | `reason` u8 | exactly 1 |

**Any other type, a payload outside its size, or a frame sent in the wrong direction is a protocol error** (§9).

Types 4–6 are additions to the design's table in §3 of the brief. Liveness and closing belong to the transport, so `transport.rs` can judge a route without parsing the control stream. Today's `{"c":"ping"}` / `{"t":"pong"}` control messages still pass through unchanged for `phone.rs`.

### 6.1 Control (type 1)
- Exactly the objects the adb route sends as lines on `tpl_ctl`, **without the newline**. The sender serialises compactly. **The payload MUST NOT contain the byte 0x0A**, because `AppTransport` re-adds a newline and hands `phone.rs` a line stream, as today. A payload that is not valid UTF-8, not a single JSON object, or contains 0x0A is a protocol error.
- Unknown `"t"` or `"c"` values are **ignored**, not errors, so either side can add messages within `PROTO = 1`.
- Additions on the app route (and, where marked, on both routes):
  - `hello` gains `"route":"app"` (both routes: `"adb"` there), `"proto":1`, `"phone_id":"<32 hex>"`, and `"caps"`, a list from `mic`, `media.state`, `media.meta`, `media.control`, `volume`, `battery`, `wallpaper`, `tile`.
  - PC → phone **`{"c":"mic","want":true|false}`**. `AppTransport.open_mic()` sends `want:true`; dropping the mic stream sends `want:false`. (On the adb route, opening and closing `tpl_mic` means the same.)
  - Phone → PC **`{"t":"mic","live":true}`** before the first audio frame of a mic session, and **`{"t":"mic","live":false,"why":"<reason>"}`** after the last one. `why` is one of `released` (the PC asked), `needs-tap` (Android refused background capture: §5 of the brief), `permission` (`RECORD_AUDIO` revoked), `busy` (another app holds the mic), `error`. The route switch (brief §7) waits on `live:false`.
  - Phone → PC **`{"t":"tile","want":"laptop"|"phone"}`** on a Quick Settings tile tap. The PC answers with its normal state messages; the phone never decides.
  - Phone → PC **`{"t":"wallpaper","id":<n>,"blob":<blob_id>}`** replaces the adb route's inline `"jpeg"`. The phone sends the blob's frames first, then this message. `AppTransport` rebuilds the adb-route line (`"jpeg"` = base64 of the blob) before `phone.rs` sees it, so `phone.rs` is unchanged.

### 6.2 Audio (type 2)
- Only between `{"t":"mic","live":true}` and `{"t":"mic","live":false}`.
- `seq` starts at **0 for each mic session** and increases by exactly 1 per frame. `capture_us` is the phone's `CLOCK_BOOTTIME` (`SystemClock.elapsedRealtimeNanos() / 1000`) at the frame's first sample.
- 480 samples = 10 ms at 48 kHz, mono, s16le, **after** the phone's voice chain, exactly what `tpl_mic` carries today.
- **The phone never blocks capture on the network.** Frames go into a **200 ms ring buffer (20 frames)**; when it is full, the oldest is dropped. Audio frames are sent **before** any queued control or blob frame.
- **The PC** treats a `seq` jump of *n* as *n* × 10 ms lost, writes that much silence to CABLE Input, and logs it (as `mic.rs` does for an underrun). A `seq` that goes backwards or repeats is a protocol error.

### 6.3 Blob (type 3)
- For payloads too big for a control frame: in v1, only the wallpaper picture.
- `total` ≤ **1 048 576** bytes; `count` = ceil(`total` / 32 768) and ≤ 32; `index` runs 0 … `count` − 1 **in order**; every chunk with `index` < `count` − 1 carries exactly 32 768 bytes of data, and the last carries `total` − (`count` − 1) × 32 768 bytes (1 – 32 768).
- **Blobs are not interleaved**: once index 0 of a blob is sent, its other chunks come before any other blob's. Control and audio frames may come in between.
- The PC holds at most **one incomplete blob**. A gap, a repeat, a size mismatch or a second blob starting early is a protocol error. A completed blob that no control message references within 10 s is discarded.

### 6.4 Liveness (types 4, 5)
- The PC sends `ping` every **10 s** while the mic is off and every **1 s** while it is live (Phase 5 tunes the idle interval against the idle-battery budget). `pc_us` is the PC's monotonic clock.
- The phone answers each `ping` with a `pong` at once, echoing `nonce` and `pc_us` and adding `phone_us` (`CLOCK_BOOTTIME`). The PC estimates the clock offset from these, to turn `capture_us` into mic latency.
- **Linked** = the last ping was answered within **2 s**. While the mic is live, any 2 s with no frame at all also counts as a miss. After **3 consecutive misses** the PC closes the connection and starts the address order again (§2). This is the "advertised but not linked" state of brief §5 when mDNS still sees the phone.
- Both sides also turn on TCP keepalive (idle 30 s, interval 10 s, 3 probes) as a backstop.

### 6.5 Bye (type 6)

| Reason | Meaning |
| --- | --- |
| 0 | normal close (the user stopped the link, or the route switch's step 3) |
| 1 | replaced by a newer session from the same PC |
| 2 | busy: linked to another PC |
| 3 | unpaired: the sender just revoked this pairing |
| 4 | protocol error (§9) |
| 5 | shutting down (app stopped, phone powering off) |

After sending `bye`, the sender sends nothing more and closes the connection within 1 s. A receiver closes on receipt.

## 7. Rate limits and resource limits (phone side)

| What | Limit |
| --- | --- |
| Unauthenticated connections (§5.1: until the first transport message decrypts) | at most **8** at once; a new connection beyond that **evicts the oldest** unauthenticated one, so a few idle sockets can't lock a paired PC out |
| Preamble | complete within 2 s |
| Handshake and first transport message | complete within 5 s of status 0 |
| Failed handshakes or unknown `pc_id`, per source IP | after **5 in 60 s**, every connection from that IP gets status 4 for 60 s; each further lockout doubles, up to **10 min**, and the counter resets after 10 min clean |
| Pairing | one at a time; 3 tries per window; 120 s window; 10 s per message, 30 s per exchange (§4) |
| Authenticated sessions | one (§5.3) |
| Blob in flight, PC side | one, ≤ 1 MiB |
| Stalled writer | if a write to the socket makes no progress for **5 s** (the TCP send buffer is full), the writer closes the connection; audio was already being dropped by the ring buffer |

## 8. Revoke
- Either side's paired-devices list has **Revoke**. It deletes that pin at once.
- If a session with that device is live, the revoking side sends `bye` (reason 3) and closes.
- **After the phone revokes,** the PC's next session gets status 3. **After the PC revokes,** the PC simply stops connecting, and the phone's list shows "last connected <date>" until the user removes it there too.
- The PC shows "This phone was unpaired" after `bye` reason 3, or after **3 consecutive** status-3 answers or handshake failures from a phone that mDNS still shows with the same `id`. One stray answer is not enough (§3.2: the status is unauthenticated).

## 9. Errors

| Event | Action |
| --- | --- |
| EOF or TCP reset mid-record | close; for the PC, a link gap and reconnect |
| Record longer than the state allows | close (no `bye`: it may not be a session yet) |
| Noise decryption fails in transport mode | close at once, no `bye` (nothing after it can be trusted) |
| Unknown frame type, wrong size, wrong direction, malformed control or blob, `seq` going backwards | send `bye` 4, close |
| Unknown control `"t"`/`"c"` | ignore |
| A control message that is valid but makes no sense now (for example `mic want:false` while off) | ignore, and log at debug level |

After any close, the PC re-runs the address order and, once linked, **re-reads the full state** (it waits for a fresh `hello`, `battery`, `media` and `volume`) before trusting anything it had before (brief §5).

## 10. What this protects, and what it does not

- **Protected:** the content of every frame (the mic, media titles, battery, the wallpaper), against anyone on the LAN, and the integrity of everything after the handshake. A device that never paired gets nothing. A recorded pairing can't be brute-forced offline (SPAKE2). An online attacker has 3 guesses per code: about 3 in a million.
- **Not protected:**
  - **Metadata.** A LAN observer sees `phone_id` (in mDNS and pairing), `pc_id` (in the session preamble), connection times, and traffic volume. **Mic live is visible from volume alone** (about 0.8 Mbit/s, against almost nothing idle). v1 adds no padding or cover traffic.
  - **Availability.** Anyone on the LAN can forge status bytes, reset TCP connections or flood the port. Rate limits stop this from becoming a guessing attack, not from becoming a nuisance. The desktop falls back to the laptop mic, so nobody goes silent.
  - **A compromised phone or PC**, which holds the keys.

## 11. Tests and vectors (Phase 1, `crates/link`)

- **Known-answer vectors**, committed under `crates/link/tests/vectors/`: a fixed code, ids, nonce, static and ephemeral keys and SPAKE2 randomness, with the expected `TH0`–`TH7`, `k_pc`, `k_phone`, every pairing message, the Noise handshake messages and the first transport frame of each type. The phone's Kotlin/Java never re-implements any of this; it calls the same Rust over JNI, and the vectors run on both targets.
- **Behaviour tests** (from brief §4): wrong code; expired code; three wrong codes then the right one (refused); two simultaneous pairings; a replayed pairing message from an old window; a replayed session handshake; a frame moved from one session into another; oversized and truncated records; a TCP reset mid-frame; each §9 row; each §7 limit; revoke on either end.
- **Fuzz targets** (`cargo fuzz`): the record reader, the frame parser (all six types), the control JSON parser, blob reassembly, and the pairing state machine driven by arbitrary peer messages. Phase 1's gate is 1 h of each clean.

## 12. Versioning
- `PROTO` appears in the TXT record, the preamble, the SPAKE2 identities and the Noise prologue. A change to anything on this page that an older peer would misread **bumps `PROTO`**. Adding a control message does not (§6.1).
- A phone app that supports several versions advertises the highest in TXT and answers status 1 to any version it does not support. A PC that sees a higher `proto` than it supports says "Update Sidemic on this PC".
