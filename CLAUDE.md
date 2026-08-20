# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Cadence (`cadence.exe`, crate name `cadence`, formerly "Ranify2") is a Windows-only Rust input-automation
tool for RAN Online: it records/plays back mouse+keyboard sequences and runs unattended on a fleet of
~22 farming VMs that report to a dashboard server (sibling repo `../cadence-server`). Everything is raw
Win32 via the `winapi` crate — no GUI framework, no async runtime; background work is plain threads
plus atomics/`Mutex` statics.

## Build / test

The crate does not compile on Linux (winapi). From WSL, use the Windows toolchain directly:

```
cd /mnt/d/Dev/Ran_services/ranitask
/mnt/c/Users/carlo/.cargo/bin/cargo.exe test                 # unit tests (pure helpers only)
/mnt/c/Users/carlo/.cargo/bin/cargo.exe test storage::       # one module / one test by name filter
/mnt/c/Users/carlo/.cargo/bin/cargo.exe build --release      # -> target/release/cadence.exe
```

Two `dead_code` warnings (`hwnd_main`, `low_since_ms`) are pre-existing. Don't launch the exe from
WSL to "verify" — it may already be running for the game; GUI checks are done by the user.

Tests are `#[cfg(test)] mod tests` inside each module and cover pure helpers only; the house style is
to split the decision out of the I/O so it's testable without `%APPDATA%`/Win32 (`copy_name(exists)`,
`resume::parse_marker`, `network::parse_name_list`, `RemoteBinding::command(resolve)`) — follow
that for new logic rather than skipping tests.

`.gitattributes` is `text=auto eol=lf`, but many working-copy files are CRLF (`storage.rs`,
`network.rs`, `settings.rs`, `remote.rs`, …). Preserve each file's existing line endings when
editing; the "CRLF will be replaced by LF" warnings on commit are harmless.

`build.rs` embeds `assets/cadence.manifest` (Per-Monitor-V2 DPI — required, or pixel sampling breaks
on >100% scaling) and derives the exe's FILEVERSION from `Cargo.toml`.

## Releasing

Releases are separate commits, `Release vX.Y.Z - <summary>`, that change only `Cargo.toml` +
`Cargo.lock`, followed by an annotated tag `vX.Y.Z` pushed to `origin main`. The tag triggers
`.github/workflows/release.yml`, which **refuses to build if the tag ≠ Cargo.toml version** (clients
self-report the version to the updater/dashboard), builds on `windows-latest`, runs Inno Setup on
`installer/cadence.iss`, and publishes `cadence.exe` + `CadenceSetup.exe`. Never bump the version
inside a feature commit. Since v3.9.0 clients do **not** auto-install: the periodic poll only shows
"vX available" in the toolbar title; installing is Settings › Update (a human action). Never
reintroduce a modal prompt or an unattended restart — both got fleet characters killed.

The GitHub repo must stay **public**: `update.rs` reads the releases API and downloads assets
unauthenticated, so a private repo silently blinds every VM's updater (and they can't fetch a fix).
If it ever has to go private, ship a different update channel first.

Commit trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. `NEXT_TASK_PLAN.md` is an
untracked scratch plan; leave it alone unless asked. `README.md` is download-only user text.

## Architecture (the parts that span files)

**Process shape.** `main.rs` raises timer resolution, loads `config`, creates the toolbar
(`gui/toolbar.rs`, window class `CadenceMain`), installs a low-level keyboard hook (`hotkeys.rs`),
arms the background subsystems from config, then runs the Win32 message loop. Hotkeys are delivered
as `WM_APP_HOTKEY` thread messages and dispatched to `gui::handle_*` in that loop. Subsystems each
own a thread + statics and expose `start/stop/set_*` + `is_*`/`snapshot()` accessors:
`monitor` (pixel bars), `pet_cycle`, `burst`, `proximity` (Npcap packet sniffer), `network` (TCP
remote control), `report` (dashboard heartbeat), `update` (GitHub poll).

**Data on disk** (`%APPDATA%\ranify2\`, path still uses the old name): `config.json` (`AppConfig`,
every field `#[serde(default)]`-tolerant so old files load), `sequences/<sanitized>.json` (one file
per `Sequence`; the *sanitized filename stem* is the identity used everywhere — see
`storage::sanitize_filename`), `queues/<stem>.json` (`SavedQueue`: a named, ordered list of
sequence names — repeats allowed, a sequence may be in many), `last_played.json`,
`detected_players.json`, plus a resume marker written across update restarts (`resume.rs`).
`storage::data_dir()` is the one place the base path is built.

**Config copies — the main footgun.** `config::save_config` writes the whole struct. The toolbar keeps
a long-lived copy in `ToolbarControls.config` (behind `GWLP_USERDATA`); Settings/Remote/Add-binding
dialogs write *through that same copy* (`GetParent` → `GWLP_USERDATA`) and save it. Anything else must
do `load_config()`/`cached_config()` → mutate → `save_config`, never hold its own copy across UI time,
or it will clobber fields another dialog changed. Per-Play state (`last_played`) is deliberately in its
own file for this reason.

**Playback model.** `player::run_playback_opts` is the single runner for one sequence or a queue:
absolute-timeline scheduling from a `PrecisionTimer`, `CANCEL`/`LOOP_MODE`/`SHUFFLE_MODE` atomics,
and it **zeroes the leading delay of every item after the first** in a pass (duration math in
`sequence::queue_pass_micros` mirrors this). All synthesized input funnels through
`player::dispatch` under `INPUT_LOCK` so monitor/burst/pet presses can't interleave a key-down/up
pair. Keys are injected as scancodes; `sequence::fix_extended` strips the extended flag Windows
mis-reports for Right Shift/NumLock (else Shift+`-` plays as `-`). `PlaybackSource` (Sequence /
Queue / Adhoc) is set right before `play_*` and drives last-played, the status line, and resume.

**Queue.** `gui::SEQUENCE_QUEUE` is an in-memory `Vec<String>` of sequence names consumed by the
Items window, the queue hotkey, `network` `PLAY_QUEUE`/`PLAY_LIST`, and `resume`. Mutate it only via
`gui::edit_queue` (clears `QUEUE_LABEL`, the saved queue it was loaded from) or `gui::set_queue`
(replace + label + posts `WM_APP_QUEUE_CHANGED` to the Items window — safe from any thread).
Groups are a per-sequence `group: Option<String>` + `group_order` (folders: unique, one per
sequence); "Group >>" expands a group into individual queue entries at add time — there is no group
object in the queue. Saved queues (`Saved Queues…` in Items) are the persisted, repeatable form.

**Bar monitor** (`monitor.rs`): one thread samples up to four pixels on the game window (found by
class/title via `find_game_window`, shared anchor from HP's config); HP/MP/SP press Q/W/E on
off-colour, `Skill` is observe-only for Idle Guard, Pet Guard rides on MP/SP. `report.rs` sends raw
bar states + `pet`/`skill` words; the server makes the alerting decisions.

**GUI conventions.** Each dialog is its own file under `src/gui/`: `register_and_create_dialog`, an
`AtomicIsize` singleton HWND, control IDs as `pub(crate) const IDC_*` in `gui/mod.rs` (numbered by
dialog: 1xx toolbar, 2xx settings, 3xx save/rename/dup, 4xx sequence manager, 6xx queue (62x saved
queues dialog), 7xx remote, 8xx bindings, 9xx players). Layout literals are 96-dpi and scaled inside `create_control`. Listbox
rows keep display text free-form and store the real name via `LB_SETITEMDATA` → `ROW_NAMES`
(item data 0 = group header). Shared state between windows is `pub(crate) static Mutex<..>` in
`gui/mod.rs`, always locked with `win32_helpers::lock_or_recover`.
The `w/h` given to `register_and_create_dialog` are **outer** sizes (and it scales them by DPI): derive
them from the client layout with `AdjustWindowRectEx` (see `settings::outer_size`) and keep the
`WM_GETMINMAXINFO` min-track in step, or a bottom-pinned OK ends up below the client edge (v3.8.0
shipped Settings with an invisible OK — every change was lost to the X button).

**Hotkeys — two storage models.** Local play hotkeys live *per sequence file* (`Sequence.hotkey`,
rebuilt by `gui::refresh_bindings` → `hotkeys::set_sequence_bindings`, no modifiers, ignored for
injected input). Remote hotkeys live in `config.remote_bindings` (`RemoteBinding`, modifier+key,
fire on injected input too so a played sequence can trigger the fleet) and are index-aligned with
the hook's list (`hotkeys::set_remote_bindings` after any change; the hook posts the index).

**Remote protocol** (`network.rs`): newline commands over TCP with optional password —
`PLAY <name>`, `PLAY_QUEUE` (host's own queue), `PLAY_SAVED <name> [seqs…]` / `PLAY_GROUP <name>
[seqs…]` (the **host's own** saved queue / group of that name wins; the trailing names are the
sender's expansion, used only if the host lacks the name; the list replaces the host's queue,
labelled, and plays), `PLAY_LIST <label> <seqs…>` (v3.10.0 sender-expansion form, kept), `STOP` →
`OK`/`ERR <reason>`. Names are file stems, so whitespace-split is safe. Remote hotkey bindings
(`RemoteBinding.target`: sequence / queue / group) build these in `RemoteBinding::command`.

**Packet detection.** `proximity.rs` (dynamic `wpcap.dll`, LZO envelope decode, opcode calibration)
is documented in `PLAYBOOK.md`; read that before touching opcodes/offsets. `NET_MSG_BASE` is
server-build-dependent (RAN Portal: 988 until mid-2026, **994** since the 2026-08 patch); when
`DROP_PC=0` with game data flowing, the console's "Paste me this line to recalibrate" candidates
line is the calibration source — DROP_PC is the candidate whose opcode+1 is frequent, base =
opcode − 2023.
