//! Telegram notification settings, scoped by account. An absent peer-level
//! mute inherits the account's user/group/channel default; it is not an unmute.
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use grammers_session::types::PeerId;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Category { User, Group, Broadcast }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuteStatus { Muted, Unmuted, Unknown }

#[derive(Clone, Copy)]
struct Setting { until: Option<i32>, observed: Instant }

type Key = (String, PeerId);
#[derive(Default)]
pub struct Notifications {
    categories: HashMap<Key, Category>,
    peers: HashMap<Key, Setting>,
    defaults: HashMap<(String, Category), Setting>,
}

impl Notifications {
    pub fn set_peer(&mut self, account: &str, peer: PeerId, until: Option<i32>, observed: Instant) {
        let key = (account.to_owned(), peer);
        if self.peers.get(&key).is_some_and(|old| old.observed > observed) { return; }
        self.peers.insert(key, Setting { until, observed });
    }
    pub fn set_category(&mut self, account: &str, peer: PeerId, category: Category) {
        self.categories.insert((account.to_owned(), peer), category);
    }
    pub fn set_default(&mut self, account: &str, category: Category, until: Option<i32>, observed: Instant) {
        let key = (account.to_owned(), category);
        if self.defaults.get(&key).is_some_and(|old| old.observed > observed) { return; }
        self.defaults.insert(key, Setting { until, observed });
    }
    pub fn status(&self, account: &str, peer: PeerId, now: u64) -> MuteStatus {
        let key = (account.to_owned(), peer);
        let Some(peer_setting) = self.peers.get(&key) else { return MuteStatus::Unknown };
        let effective = match peer_setting.until {
            Some(value) => value,
            None => {
                let Some(category) = self.categories.get(&key) else { return MuteStatus::Unknown };
                let Some(default) = self.defaults.get(&(account.to_owned(), *category)) else { return MuteStatus::Unknown };
                default.until.unwrap_or(0)
            }
        };
        if i64::from(effective) > now.min(i64::MAX as u64) as i64 { MuteStatus::Muted } else { MuteStatus::Unmuted }
    }
    pub fn all_defaults_known(&self, account: &str) -> bool {
        [Category::User, Category::Group, Category::Broadcast]
            .iter().all(|c| self.defaults.contains_key(&(account.to_owned(), *c)))
    }
    /// Reconsider timed mutes even when no new network/UI event arrives.
    pub fn next_expiry(&self, now: u64) -> Option<u64> {
        self.peers.values().chain(self.defaults.values())
            .filter_map(|setting| setting.until)
            .filter_map(|until| u64::try_from(until).ok())
            .filter(|until| *until > now)
            .min()
    }
    pub fn retain_accounts(&mut self, known: &HashSet<String>) {
        self.categories.retain(|(a, _), _| known.contains(a));
        self.peers.retain(|(a, _), _| known.contains(a));
        self.defaults.retain(|(a, _), _| known.contains(a));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn peer_override_inherits_default_and_timed_mute_expires() {
        let mut n = Notifications::default(); let peer = PeerId::user(1); let now = Instant::now();
        n.set_category("a", peer, Category::User);
        n.set_peer("a", peer, None, now);
        assert_eq!(n.status("a", peer, 100), MuteStatus::Unknown);
        n.set_default("a", Category::User, Some(200), now);
        assert_eq!(n.status("a", peer, 100), MuteStatus::Muted);
        assert_eq!(n.next_expiry(100), Some(200));
        assert_eq!(n.status("a", peer, 200), MuteStatus::Unmuted);
        n.set_peer("a", peer, Some(0), now + Duration::from_secs(1));
        assert_eq!(n.status("a", peer, 100), MuteStatus::Unmuted);
        n.set_peer("a", peer, Some(999), now + Duration::from_secs(2));
        n.set_peer("a", peer, Some(0), now); // stale snapshot must not undo an update
        assert_eq!(n.status("a", peer, 100), MuteStatus::Muted);
        assert_eq!(n.status("b", peer, 100), MuteStatus::Unknown);
    }
    #[test]
    fn default_updates_do_not_override_peer_exceptions() {
        let mut n = Notifications::default(); let p = PeerId::user(1); let t = Instant::now();
        n.set_category("a", p, Category::User);
        n.set_peer("a", p, Some(0), t);
        n.set_default("a", Category::User, Some(i32::MAX), t);
        assert_eq!(n.status("a", p, 1), MuteStatus::Unmuted);
        n.set_peer("a", p, None, t + Duration::from_secs(1));
        assert_eq!(n.status("a", p, 1), MuteStatus::Muted);
    }
}
