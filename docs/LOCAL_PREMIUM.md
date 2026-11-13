# Local Premium — the honest breakdown

Telegram Premium is a **server-side subscription**. Telegram's datacenters
check the account's premium flag on nearly every perk people actually want.
**No client — this one included — can flip those bits for a non-premium
account.** If someone tells you otherwise, they are describing a placebo or a
violation of Telegram's Terms of Service.

What this client *can* do — and deliberately does — is unlock **client-side
conveniences** locally. That is what `mlxysftele premium` toggles. This is the
same engineering model as ZapFast's "local" features: the app does more, locally,
out of the box, without touching anyone's servers.

## Unlocked locally (real, in this process)

| Flag | What it does | Where it applies |
|---|---|---|
| `enabled` | Master switch for the flags below. | All of them |
| `themes` | Custom local themes/accents rendered from local assets. | This client's UI only |
| `fast_animations` | Shortens local UI animation durations. | This client's UI only |
| `sticker_shortcuts` | Quick-key access to locally installed sticker packs. | This client's input path only |
| `hide_sponsored` | Best-effort dropping of raw `Sponsored*` update objects anywhere they surface. | This client's update pump |
| `hidden_premium_nags` | Suppress client-side "upgrade to premium" upsell surfaces. | This client only |

Notes on the two that need them:

- `hide_sponsored`: Telegram currently delivers sponsored messages to
  third-party MTProto clients via separate RPC calls (`messages.sponsoredMessages`),
  which this client never makes. If Telegram ever streams sponsored content as
  raw update objects in the normal update stream, this flag drops them. It is
  defense-in-depth, not a magic ad blocker, and we say so.
- `themes` / `fast_animations`: this binary is headless; these flags exist so a
  GUI frontend (see `docs/ROADMAP.md`) renders with local assets and short
  animation timings. No premium assets are fetched, copied, or impersonated.

## Never unlocked locally (server-enforced, unchanged)

- 4 GB uploads (Telegram upload DCs enforce the 2 GB / 4 GB cap by account)
- Server-side download speed boost for larger files
- Animated / video status, profile video, emoji status
- Boosts, extra reactions, custom emoji in messages
- Voice-to-text transcription and Telegram-side message translation
- Telegram Stars spend, paid media, gifts, monetization surfaces
- Premium sticker packs flagged as paid/limited by the server
- Any feature where Telegram's server checks the `premium` flag on the account

## What about an account that IS premium?

Then this client shows `server premium: yes` (see `whoami <name>`), renders
premium-marked content it receives normally, and the flags above are simply
orthogonal — they add local conveniences on top of the real thing. Nothing
here pretends a non-premium account is premium, because it isn't.

## Why be this explicit?

Because "local premium access" is a phrase that is usually abused to mean
"features you didn't pay for, verified by the server". A client can lie about
that in its UI. It cannot make Telegram honor it. We don't lie; we don't cheat
the server; and everything we *do* change is listed in the table above, under
your control, per machine, in `config.json`.

## FAQ

**Q: I set `premium on`. Do my uploads get bigger?**
No. Upload size is enforced by Telegram's datacenters.

**Q: Will Telegram ban me for using local premium?**
No — these flags change how this client renders and routes data you already
receive. Ban risk comes from abuse (spam, flooding), not from theme flags.

**Q: Can I use this to view paid media for free?**
No. Paid media and paid messages are enforced server-side.

**Q: An account I manage is genuinely premium. What do I get?**
The server honors its subscription. This client displays the truth and gets out
of the way.
