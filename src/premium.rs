//! "Local Premium" — client-side conveniences toggled locally, ZapFast-style.
//!
//! ## The honest contract
//!
//! Telegram Premium is a *server-side* subscription. Everything worth having in
//! it is verified by Telegram's datacenters against the account's premium flag:
//!
//!   * 4 GB (vs 2 GB) upload limit        — enforced by upload DCs
//!   * Faster DC downloads                — enforced by download DCs
//!   * Animated profile picture upload    — enforced server-side
//!   * Boosts, reactions, emoji status    — enforced server-side
//!   * Voice-to-text, message translation — Telegram's servers
//!   * No ads                             — ads are delivered by the server
//!   * Stories for non-premium viewing UX — partly server-driven
//!
//! **No client can flip those bits for an account that is not premium.** Anyone
//! selling that is selling a placebo or breaking Telegram's Terms of Service.
//!
//! What a client *can* do is unlock its own local functionality. That is what
//! this module does. Every flag below is real, implemented locally, and affects
//! only what THIS process does with data it already lawfully receives. If
//! Telegram one day sends premium-marked content because the account genuinely
//! is premium, this client renders it; the flags here only unblock the local UI
//! affordances.
//!
//! See `docs/LOCAL_PREMIUM.md` for the full breakdown.

use serde::{Deserialize, Serialize};

/// Client-side premium conveniences. All default to `false`.
///
/// `enabled` is a master switch: when false, nothing is unlocked locally.
/// The granular flags let users cherry-pick even when the master switch is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PremiumToggles {
    /// Master switch. When false, all local premium conveniences are off.
    pub enabled: bool,
    /// Custom local themes and accent colors (rendered from this client's own
    /// assets, not from Telegram's premium theme packs).
    pub themes: bool,
    /// Reduce local animation durations / local micro-animations.
    pub fast_animations: bool,
    /// Unlock local sticker-pack shortcuts (quick-key mapping of locally
    /// installed packs). Does not unlock premium sticker packs server-side.
    pub sticker_shortcuts: bool,
    /// Best-effort dropping of raw `*Sponsored*` update objects if Telegram
    /// ever streams them to third-party MTProto clients (currently they are
    /// delivered via separate RPC calls this client never makes).
    pub hide_sponsored: bool,
    /// Suppress client-side "upgrade to premium" nags and upsell surfaces
    /// (this client ships none by default; the flag exists for UI parity
    /// with forks that add telemetry-free upsell widgets).
    pub hidden_premium_nags: bool,
}

impl Default for PremiumToggles {
    fn default() -> Self {
        Self {
            enabled: false,
            themes: false,
            fast_animations: false,
            sticker_shortcuts: false,
            hide_sponsored: false,
            hidden_premium_nags: false,
        }
    }
}

/// A single toggle together with human-readable documentation.
pub struct ToggleDoc {
    pub key: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
}

pub const TOGGLE_DOCS: &[ToggleDoc] = &[
    ToggleDoc {
        key: "themes",
        label: "Custom themes",
        detail: "Local themes/accents rendered by this client only.",
    },
    ToggleDoc {
        key: "fast_animations",
        label: "Fast animations",
        detail: "Shortens local UI animation durations.",
    },
    ToggleDoc {
        key: "sticker_shortcuts",
        label: "Sticker shortcuts",
        detail: "Quick-key access to locally installed sticker packs.",
    },
    ToggleDoc {
        key: "hide_sponsored",
        label: "Hide sponsored",
        detail: "Best-effort drop of raw Sponsored* updates (client-side only).",
    },
    ToggleDoc {
        key: "hidden_premium_nags",
        label: "Hidden premium nags",
        detail: "Suppress client-side premium upsell surfaces.",
    },
];

impl PremiumToggles {
    /// Master switch AND granular flag.
    pub fn active(&self, flag: bool) -> bool {
        self.enabled && flag
    }

    pub fn hide_sponsored_active(&self) -> bool {
        self.active(self.hide_sponsored)
    }

    /// Set every granular flag (used by `premium on` / `premium off`).
    pub fn set_all(&mut self, value: bool) {
        self.enabled = value;
        self.themes = value;
        self.fast_animations = value;
        self.sticker_shortcuts = value;
        self.hide_sponsored = value;
        self.hidden_premium_nags = value;
    }

    /// Apply one named flag; returns false if the key is unknown.
    pub fn set_named(&mut self, key: &str, value: bool) -> bool {
        match key {
            "enabled" => self.enabled = value,
            "themes" => self.themes = value,
            "fast_animations" => self.fast_animations = value,
            "sticker_shortcuts" => self.sticker_shortcuts = value,
            "hide_sponsored" => self.hide_sponsored = value,
            "hidden_premium_nags" => self.hidden_premium_nags = value,
            _ => return false,
        }
        true
    }

    /// Read one named flag.
    pub fn get_named(&self, key: &str) -> Option<bool> {
        match key {
            "enabled" => Some(self.enabled),
            "themes" => Some(self.themes),
            "fast_animations" => Some(self.fast_animations),
            "sticker_shortcuts" => Some(self.sticker_shortcuts),
            "hide_sponsored" => Some(self.hide_sponsored),
            "hidden_premium_nags" => Some(self.hidden_premium_nags),
            _ => None,
        }
    }

    /// Render the toggle table for `premium status`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "local premium: {}\n",
            if self.enabled { "ON" } else { "OFF" }
        ));
        for doc in TOGGLE_DOCS {
            let value = self.get_named(doc.key).unwrap_or(false);
            out.push_str(&format!(
                "  {:<22} {:>3}  — {}\n",
                doc.key,
                if value { "on" } else { "off" },
                doc.detail
            ));
        }
        out
    }
}

/// What Local Premium deliberately does NOT unlock (server-enforced).
pub const NEVER_UNLOCKED: &[&str] = &[
    "4 GB uploads (Telegram upload DCs enforce the 2/4 GB cap by account)",
    "Server-side download speed boost for larger files",
    "Animated / video status, profile video, emoji status",
    "Boosts, extra reactions, custom emoji in messages",
    "Voice-to-text transcription and Telegram-side message translation",
    "Telegram Stars spend, paid media, gifts, monetization features",
    "Premium sticker packs flagged as paid/limited by the server",
    "Any feature where Telegram's server checks the `premium` flag on the account",
];

/// Render the full honesty notice printed by `premium info`.
pub fn render_info() -> String {
    let mut out = String::new();
    out.push_str("LOCAL PREMIUM — what it is\n");
    out.push_str("===========================\n");
    out.push_str(
        "Flags stored in config.json unlock conveniences inside this client\n\
         only. They cost nothing and bypass nothing: anything the server\n\
         marks as premium stays premium-only.\n\n",
    );
    out.push_str("Unlocked locally (this process):\n");
    for doc in TOGGLE_DOCS {
        out.push_str(&format!("  • {} — {}\n", doc.label, doc.detail));
    }
    out.push_str("\nNEVER unlocked locally (enforced by Telegram servers):\n");
    for item in NEVER_UNLOCKED {
        out.push_str(&format!("  • {item}\n"));
    }
    out.push_str(
        "\nIf an account IS genuinely premium, this client shows its real\n\
         premium status (see `whoami`) and renders premium-marked content\n\
         normally — the flags above are orthogonal to that.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_all_flips_every_flag() {
        let mut t = PremiumToggles::default();
        t.set_all(true);
        assert!(t.enabled && t.themes && t.fast_animations && t.sticker_shortcuts
            && t.hide_sponsored && t.hidden_premium_nags);
        t.set_all(false);
        assert!(!t.enabled && !t.themes);
    }

    #[test]
    fn named_flags_roundtrip() {
        let mut t = PremiumToggles::default();
        assert!(t.set_named("hide_sponsored", true));
        assert_eq!(t.get_named("hide_sponsored"), Some(true));
        assert!(!t.set_named("definitely_not_a_flag", true));
        assert_eq!(t.get_named("definitely_not_a_flag"), None);
    }

    #[test]
    fn master_switch_gates_granular_flags() {
        let mut t = PremiumToggles::default();
        t.hide_sponsored = true;
        assert!(!t.hide_sponsored_active(), "granular flag without master switch stays inactive");
        t.enabled = true;
        assert!(t.hide_sponsored_active());
    }

    #[test]
    fn render_lists_every_flag() {
        let t = PremiumToggles::default();
        let out = t.render();
        for doc in TOGGLE_DOCS {
            assert!(out.contains(doc.key), "render should mention {}", doc.key);
        }
    }
}
