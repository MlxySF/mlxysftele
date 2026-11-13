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

## v0.3 — GUI shell (ZapFast parity)

- `feature = "gui"`: egui + egui_extras only (no web view, no Electron)
- Three panes: accounts (registry) · dialogs · messages
- Local themes from `premium themes` flag; short animation timings from
  `fast_animations`
- Target: < 200 MB RSS idle with 1 connected account, like ZapFast
  (< 100 MB for the headless daemon path)
- Media viewer streamed to disk, thumbnails via `litemap`/`image` (opt-in)

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

- [ ] current_thread runtime stays the default
- [ ] no unbounded caches; idle eviction stays under `--idle-ttl`
- [ ] media paths streamed, never buffered whole
- [ ] release binary size budget: < 8 MB (opt-level = "z", LTO fat, strip)
- [ ] RSS budget: < 32 MB daemon idle with 0 hot connections;
      < 48 MB with 1 hot connection (measured with `mlxysftele stats`)
