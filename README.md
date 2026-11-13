# mlxysftele

[![ci](https://github.com/MlxySF/mlxysftele/actions/workflows/ci.yml/badge.svg?branch=arena/275d9362-mlxysftele)](https://github.com/MlxySF/mlxysftele/actions/workflows/ci.yml)

**A ZapFast-style, ultra low-RAM Telegram client in Rust.** Unlimited account
login, per-account sessions, a headless multi-account daemon, and honest
local client-side "premium" conveniences — all in one small static binary
with a current-thread async runtime.

> **Status:** v0.1.0 · headless core (CLI + daemon). GUI shell tracked in
> [`docs/ROADMAP.md`](docs/ROADMAP.md). Not affiliated with Telegram.

---

## Why

[ZapFast](https://github.com/crmne/zapfast) proved for WhatsApp that a native
Rust client can replace Chromium-class memory hogs: ~150 MB idle PSS vs
~1.13 GB for Chromium/WhatsApp Web across four paired runs
([ZapFast benchmarks][zapfast-bench]; see also the
[third-party comparison][sysfix]). Telegram Desktop sits around a few hundred
MB and Electron wrappers sit at the top.

`mlxysftele` applies the same discipline to Telegram via the MTProto protocol
directly ([grammers](https://github.com/Lonami/grammers), pure Rust, no TDLib
native binding):

| | mlxysftele | Electron wrapper | Telegram Desktop |
|---|---|---|---|
| Engine | tokio current_thread | Node + Chromium | Qt |
| Dormant accounts | **~0 RAM** (sqlite on disk) | n/a | per-instance |
| Downloads | streamed to disk | buffered | buffered |
| Binary | `strip`ed, `opt-level=z`, LTO | 100+ MB FS | large |
| Accounts | unlimited (software) | 1+ | multiple |

[zapfast-bench]: https://zapfast.rocks/benchmarks/
[sysfix]: https://yoursystemfixer.com/whatsapp-high-ram-usage-windows-11-alternatives/

## Features

- **Unlimited accounts** — register as many as you like. Each dormant account
  is a JSON registry entry (~200 bytes) plus one sqlite session file on disk.
  Connections are lazy, and the daemon evicts idle ones (see
  [Memory model](#memory-model)).
- **Real MTProto** — login with your phone via Telegram's official code flow
  (supports 2FA / SRP passwords). Uses your own `api_id`/`api_hash` from
  <https://my.telegram.org/apps>.
- **Send / tail / dialogs / whoami / streamed downloads** from the CLI.
- **`daemon`** — a headless multi-account hub: bounded hot set (`--max-hot`),
  idle eviction (`--idle-ttl`), one-line message stream per account, stdin
  command interface. Ideal on a home server or VPS.
- **Local premium** — client-side conveniences toggled locally in
  `config.json` (themes, fast animations, sticker shortcuts, sponsored-update
  filtering). **Server-enforced Premium is not and cannot be unlocked by any
  client** — see [`docs/LOCAL_PREMIUM.md`](docs/LOCAL_PREMIUM.md). This is the
  honest meaning of "local Telegram Premium access".
- **No framework soup** — one binary, single-threaded runtime, dependency
  list you can audit in one screen. Optional `--features mimalloc`.

## Quick start

Prerequisites: Rust **1.85+** and network access to `github.com` (git-pinned
grammers), `crates.io`, and Telegram's datacenters.

```sh
git clone https://github.com/MlxySF/mlxysftele.git
cd mlxysftele
cargo build --release          # binary: target/release/mlxysftele

# 1) One-time: your own Telegram API credentials (https://my.telegram.org/apps)
./target/release/mlxysftele init --api-id=123456 --api-hash=your_api_hash_here

# 2) Log in any number of accounts; each asks for your phone + the code Telegram sends
./target/release/mlxysftele login work    --phone=+6581234567
./target/release/mlxysftele login personal --phone=+6598765432

# 3) Use them
./target/release/mlxysftele whoami work
./target/release/mlxysftele dialogs work --limit=10
./target/release/mlxysftele send work me "hello from mlxysftele"
./target/release/mlxysftele tail work        # live stream, Ctrl-C to stop
./target/release/mlxysftele stats             # process memory footprint
```

Headless hub (prints incoming messages from all hot accounts, stdin commands):

```sh
./target/release/mlxysftele daemon --max-hot=4 --idle-ttl=300
# type `help` inside the daemon for commands
```

## Command reference

| Command | What it does |
|---|---|
| `init --api-id=N --api-hash=H` | Store API credentials (or set `MLXYSFTELE_API_ID` / `MLXYSFTELE_API_HASH`) |
| `login <name> [--phone=+..]` | Register/refresh an account (interactive code + 2FA) |
| `accounts [--check]` | List registered accounts; `--check` verifies authorization online |
| `whoami <name>` | Identity + **server** premium flag + **local** premium state |
| `dialogs <name> [--limit=N]` | Chat list |
| `send <name> <peer> <text…>` | Send text (`peer` = `@username` or `me`) |
| `tail <name> [--no-catch-up]` | Live updates until Ctrl-C |
| `download <name> <peer> <msg_id> [--out=PATH]` | Media streamed to disk |
| `daemon [--max-hot=4] [--idle-ttl=120]` | Multi-account hub (stdin commands) |
| `premium [status\|on\|off\|set <flag> on\|off\|info]` | Local premium conveniences |
| `stats` | RSS/VM/threads + on-disk footprint |
| `logout <name> [--force]` | Sign out on Telegram + delete local session |
| `remove <name>` | Delete local session & registry entry only |

## Memory model

Every number below is a design property of the code, enforced by construction:

- **Dormant account = 0 RAM.** `login` persists a sqlite session; until a
  command or daemon tick connects it, nothing is held in memory.
- **One connection per command.** `send`, `whoami`, `dialogs`, `download`
  connect, act, and disconnect.
- **Daemon "hot set" is bounded.** At most `--max-hot` connections stay open
  (default 4); eviction is least-recently-used, plus a `--idle-ttl` reaper.
- **No caches.** No message cache, no media cache, no peer cache beyond what
  Telegram returns in-flight. Downloads are chunked straight to disk.
- **Small allocator & binary.** current_thread runtime, `opt-level = "z"`,
  `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
  Optional `mimalloc` (`cargo build --release --features mimalloc`).
- **Atomic config.** `config.json` is written via tmp-file + rename; secrets
  (`config.json`, sessions) are forced to `0600` on Unix.

Measure it yourself — don't trust marketing (ours included):

```sh
/usr/bin/time -v ./target/release/mlxysftele stats
./target/release/mlxysftele daemon --max-hot=2
# then, in another shell: watch -n1 'grep -E "VmRSS|Threads" /proc/$(pgrep mlxysftele)/status'
```

Memory is **not** byte-equal across machines; the budget targets are < 32 MB
idle (0 hot connections) and < 48 MB with 1 hot connection — see
[`docs/ROADMAP.md`](docs/ROADMAP.md).

## Local premium, honestly

`mlxysftele premium on` enables client-side conveniences only. It does **not**
and **cannot** unlock Telegram Premium's server-enforced perks (4 GB uploads,
faster DCs, Stars, boosts, reactions, translation). No third-party client can.
The full matrix — what is unlocked locally, what never is, and why — is in
[`docs/LOCAL_PREMIUM.md`](docs/LOCAL_PREMIUM.md). An account that is genuinely
premium shows `server premium: yes` in `whoami`.

## Unlimited accounts, honestly

Software-wise there is no cap. Reality-wise:

- Each account needs its own phone number and a login code; Telegram
  rate-limits codes. Your account may be limited by Telegram for automation
  abuse or spam — that is a ToS matter, not a client feature.
- One IP sustaining many sessions is fine for normal use; flooding earns
  `FLOOD_WAIT`. Respect it.
- Dormant accounts cost disk only: ~1 KB config line + session file.

## Security

- Sessions are **bearer credentials**. Anyone with `sessions/<name>.sqlite`
  is that account. File modes are forced `0600`; back them up like passwords.
- `config.json` contains only your API hash (yours, not Telegram's) — still
  `0600`. Use `MLXYSFTELE_API_ID`/`MLXYSFTELE_API_HASH` env vars to avoid
  writing secrets to disk.
- `logout` deletes local state but your sessions list on Telegram is
  authoritative — revoke stale ones in Settings → Devices.
- Never commit `.mlxysftele/`, `*.sqlite`, or `.session` files (this repo's
  `.gitignore` already excludes them).

## Configuration

`~/.mlxysftele/config.json` (override root with `MLXYSFTELE_HOME`):

```json
{
  "api_id": 123456,
  "api_hash": "your_api_hash",
  "accounts": [
    {
      "name": "work",
      "phone": "+6581234567",
      "user_id": 123456789,
      "username": "someone",
      "created_unix": 1760000000,
      "last_login_unix": 1762900000
    }
  ],
  "local_premium": {
    "enabled": true,
    "themes": true,
    "fast_animations": true,
    "sticker_shortcuts": true,
    "hide_sponsored": true,
    "hidden_premium_nags": true
  }
}
```

## Development

```sh
cargo fmt           # formatting
cargo check         # types (needs github.com + crates.io access)
cargo test          # unit tests
cargo build --release
```

Grammers is pinned to an exact git commit (the crates.io v0.8.0 release
predates the module reorganization this crate uses), so building requires
network access to `github.com` and `crates.io`. Builds and tests are
verified on every push by GitHub Actions (`.github/workflows/ci.yml`) —
including a smoke test of the release binary. `Cargo.lock` is generated
on your first build; commit it for reproducible builds.

Layout: `src/main.rs` (CLI) · `telegram.rs` (engine) · `daemon.rs` (hub) ·
`config.rs` (atomic registry) · `premium.rs` (local premium flags) ·
`lowmem.rs` (`/proc` introspection) · `util.rs` · `log.rs` (zero-dep logger).

Design rule for contributors: **no new dependency without a written reason.**
Every crate we take on is compile time, audit surface, and attack surface.

## FAQ

**Can I log into 100 accounts at once?**
Yes — register 100 accounts; they cost ~0 RAM until connected. The daemon
keeps at most `--max-hot` connected; the rest stay on disk.

**Is this official?**
No. It is a third-party client using MTProto, like every MTProto client.
Telegram may change the API at any time; grammers tracks it.

**Does it replace the official app?**
For headless / multi-account / low-RAM use, yes. For calls, video, stories
and the full UI — not yet (see ROADMAP).

## License

MIT OR Apache-2.0 (same as grammers). See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
