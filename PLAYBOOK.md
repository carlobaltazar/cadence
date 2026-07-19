# RAN packet-detection playbook (reusable)

How the player-proximity detector was built, generalized so the same **method** and **code** can
be reused for other in-game events (mob spawns, item drops, chat triggers, buffs…) on any RAN
server — or ported to another game with a similar batched/compressed TCP protocol.

Companion docs/code:
- `PROTOCOL.md` — the concrete RAN wire format (envelope, opcodes, calibration numbers).
- `lzo1x.py`, `ran_proximity.py` — Python reference implementation (fast to iterate).
- `D:\Dev\Ran_services\ranitask\src\proximity.rs` — the Rust engine used in Cadence.

---

## Part 1 — The method (server/game-agnostic)

This is the exact sequence that took us from "packets don't detect players" to a working detector.
Reuse it whenever you need to detect a network-visible game event.

1. **Get ground truth if you can.** If game source or a client build exists, read it first. For
   RAN, `C:\Users\carlo\Downloads\ran_ep7\Source Code VS2022` gave the opcode enum
   (`s_NetGlobal.h`) and packet structs (`GLCharData.h`), which told us *what* to look for
   (`DROP_PC` = player spawn) and the packet layout. Ground truth turns guessing into confirming.

2. **Find the right adapter + server automatically — don't hardcode.** Capture *all* TCP, count
   packets per interface, and report the busiest non-web endpoint (skip ports 443/80/53). That IP
   is the game server, even when the server remaps ports (RAN Portal uses 8104/8102, not
   7112/7101). If `pkts=0`, it's the wrong adapter (or, in a VM, Npcap isn't installed there).

3. **Decode the transport before the payload.** RAN wraps batched messages in a `NET_COMPRESS`
   envelope `[dwSize:u32][nType=170:u32][bCompress:u32]`, and large envelopes are **miniLZO
   (LZO1X) compressed**. You must: match the envelope opcode → read the compress flag →
   LZO-decompress if set → *then* walk inner messages. Skipping decompression is why earlier
   attempts only ever saw small uncompressed packets (mobs/pets) and never players.

4. **Enumerate opcodes, don't assume them.** Walk inner messages `[dwSize][nType][payload]` and
   build a histogram of `nType`. This shows what's actually flowing and how often.

5. **Identify the target by structural signature, not a guessed number.** Player-spawn signatures:
   the payload carries a **null-terminated player name near the start**, the packet is **large**,
   and it has a **frequent `DROP_CROW` (mobs) neighbor at opcode+1**. We scan every message for an
   embedded name and log "name-carrying opcodes" — that surfaces the spawn opcode directly.

6. **Calibrate the region/opcode base empirically.** RAN opcodes are `NET_MSG_BASE + 1900 + offset`
   and the base is region/build-specific (private servers pick a custom one for anti-sniffing).
   Solve the base from confirmed opcodes: e.g. `DROP_PC(3011)/DROP_CROW(3012)/DROP_OUT(3015)` line
   up only for base **988** on RAN Portal (public `143.14.88.19` used **977** → `DROP_PC=3000`).
   The `+1 = DROP_CROW must be frequent` rule disambiguates false candidates.

7. **Validate offline then live.** Replay saved captures (deterministic), then confirm live with a
   stats line. Success = the event fires with the right name and no false positives.

---

## Part 2 — The RAN mechanism (what to reuse)

- **Capture:** passive Npcap sniff, server→client TCP. Passive = invisible to GameGuard. Npcap must
  be installed **where the game+tool actually run** (inside the VM if applicable).
- **Direction:** a packet is server→client when its **source IP == server IP** (found by discovery).
- **Framing:** `NET_COMPRESS` envelope (nType 170, fixed) → optional LZO1X → concatenated
  `[dwSize][nType][payload]` inner messages. Resync by advancing 1 byte on any framing glitch.
- **Opcodes:** `DROP_PC` (player spawn, name-carrying, large), `DROP_CROW=DROP_PC+1` (mobs),
  `DROP_OUT=DROP_PC+4` (despawn). Base is per-server; see `PROTOCOL.md`.
- **Names:** null-terminated ASCII a few bytes into the spawn payload.

---

## Part 3 — Reusable code (lift these, they're event-agnostic)

**Rust (`ranitask/src/proximity.rs`)** — the transport engine is independent of *what* you detect:
- `struct Pcap` + `Pcap::load()` — loads `wpcap.dll` dynamically (no Npcap SDK needed to build);
  `list_devices`, `open_live`, `set_filter`, `next_packet`. Reuse verbatim.
- `parse_tcp(pkt) -> (src_ip, dst_ip, src_port, dst_port, payload)` — Ethernet/IPv4/TCP parse.
- `lzo1x_decompress(&[u8]) -> Option<Vec<u8>>` — pure-Rust LZO1X (unit-tested vs a real capture).
- `parse_flow(buf, on_msg)` — **the core decoder**: NET_COMPRESS envelope + LZO + inner-message
  walk, calling `on_msg(opcode, message)` per inner message. Feed it reassembled stream bytes.
- Server auto-discovery (busiest non-web endpoint) and the calibration diagnostics (opcode
  histogram + `scan_name` name-carrying scan) live in `capture_loop` — copy those blocks.
- Helpers: `parse_ipv4`, `fmt_ip`, `scan_name`.

**Event-specific (swap these):** `NET_MSG_BASE`/`DROP_PC`, and the reaction (`do_reaction`, key/
sequence, the "Det" UI). For a new event, keep the engine and change only the opcode you match in
the `parse_flow` callback + what you do on match.

**Python (`ran_sniff/`)** — fastest for R&D/calibration: `lzo1x.py`, `protocol.py`, and
`ran_proximity.py` (`--pcap` / `--iface` / `--textdump`). Use this to calibrate a new server before
touching Rust.

---

## Part 4 — Adapting it

- **New server / remapped ports:** nothing to change — leave Server IP blank; discovery finds it.
  If the event stops firing, the opcode base changed: read the console's `Opcodes(top)` +
  `Name-carrying opcodes` lines and recompute the base (Part 1, steps 5–6).
- **Detect a different event** (specific mob, item drop, guild/chat message): find its opcode via
  the histogram + a structural signature (size, a marker byte, an embedded string), then match that
  opcode in the `parse_flow` callback instead of `DROP_PC`.
- **New project/tool:** copy `Pcap`, `parse_tcp`, `parse_flow`, `lzo1x_decompress` (+ discovery) as
  a standalone module; they have no dependency on Cadence.

---

## Part 5 — Gotchas (all cost us time)

- **Npcap location:** must be installed where capture runs. On a VM, install it *in the VM*.
- **Compression is mandatory:** large/batched packets are LZO — decompress before parsing.
- **Opcode base is per-server:** never hardcode across servers; calibrate.
- **Interface/IP discovery beats hardcoding:** `pkts=0` almost always means wrong adapter, not a
  code bug. Give yourself a per-interface packet counter.
- **Anti-cheat:** passive capture is safe (no process/memory access). But the *tool's* exe/window
  name matters — RAN's GameGuard scans for names (that's why this app is "Cadence", not "ranitask").
- **Reproducibility:** keep saved captures and a unit test that decodes a known envelope, so a
  refactor can't silently break the decoder.
