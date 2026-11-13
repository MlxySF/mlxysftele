# Roadmap

Shaped in ZapFast's spirit: a native client that sips memory, ships fast, and
stays honest about what the server enforces.

## v0.2 — verification & packaging

- CI (GitHub Actions): `cargo fmt --check`, `clippy -D warnings`, `cargo test`,
  cross-builds for linux/x86_64, macOS, Windows
- `cargo dist` release artifacts (tarballs + deb/rpm)
- Optional feature `proxy` mapped to grammers' SOCKS5 support
- `download --resume` (partial + Range requests via `upload.getFile` offsets)
- `send --file` via `client.upload_file` + `send_message(...).document(...)`

## v0.3 — GUI polish (ZapFast parity)

- `gui` is now the default mode: accounts · chats · messages, all accounts
  connected simultaneously, live update streams, unread badges,
  search filter, server-premium `PRO` badge, honest local-premium toggles
- Local themes from `premium themes` flag; short animation timings from
  `fast_animations`
- Target: < 200 MB RSS idle with 1 connected account, like ZapFast
- Media viewer streamed to disk, thumbnails via `litemap`/`image` (opt-in)
- Per-account proxy + per-account device model controls

## v0.4 — account ergonomics

- Bot API login path (`bot_sign_in`) for automation accounts
- `accounts import-tdlib-db` migration helper
- Per-account proxy + device model spoofing controls
- Export of session inventory (audit: where are my codes logged in?)

## Non-goals (won't happen)

- Unlocking server-enforced premium features for non-premium accounts
  (see `docs/LOCAL_PREMIUM.md`)
- Cracked session trading, credential stores, account selling
- Scraping at scale / spam tooling (drives bans; not our use case)

## Memory discipline checklist (kept honest per release)

- [x] GUI: accounts stay connected simultaneously (supersedes daemon hot-set
      as the flagship mode; the daemon remains for headless/VPS use)
- [ ] no unbounded caches — dialog list ≤ 150/account, chat cache ≤ 100 messages
- [ ] media paths streamed, never buffered whole
- [ ] release binary size budget: < 8 MB headless CLI target; GUI will be larger
- [ ] RSS budget: measure GUI with `mlxysftele stats` (target ≤ 200 MB idle, 1 account)
