//! Account-scoped unread state. Server snapshots and read acknowledgements can
//! race live messages; an old read must never clear a newer message.
use std::collections::HashMap;
use grammers_session::types::PeerId;

type Key = (String, PeerId);
#[derive(Clone, Copy)]
struct State { count: u32, newest: i32 }

#[derive(Default)]
pub struct Unread {
    entries: HashMap<Key, State>,
    read_through: HashMap<Key, i32>,
}

impl Unread {
    pub fn incoming(&mut self, account: &str, peer: PeerId, id: i32) {
        let key = (account.to_owned(), peer);
        if id <= self.read_through.get(&key).copied().unwrap_or(0) { return; }
        match self.entries.get_mut(&key) {
            Some(state) if id > state.newest => {
                state.newest = id;
                state.count = state.count.saturating_add(1);
            }
            Some(_) => {} // duplicate or already included in a server snapshot
            None => { self.entries.insert(key, State { count: 1, newest: id }); }
        }
    }

    pub fn snapshot(&mut self, account: &str, peer: PeerId, count: u32, top: i32, read: i32) {
        let key = (account.to_owned(), peer);
        let floor = self.read_through.entry(key.clone()).or_default();
        *floor = (*floor).max(read);
        let floor = *floor;
        if let Some(existing) = self.entries.get(&key) {
            if existing.newest > top && existing.newest > floor { return; }
        }
        if count == 0 || top <= floor {
            self.entries.remove(&key);
        } else {
            self.entries.insert(key, State { count, newest: top });
        }
    }

    pub fn read(&mut self, account: &str, peer: PeerId, max_id: i32, remaining: Option<u32>) {
        let key = (account.to_owned(), peer);
        let floor = self.read_through.entry(key.clone()).or_default();
        *floor = (*floor).max(max_id);
        if let Some(state) = self.entries.get_mut(&key) {
            if state.newest <= *floor { self.entries.remove(&key); }
            else if let Some(count) = remaining { state.count = count.max(1); }
        }
    }

    pub fn count(&self, account: &str, peer: PeerId) -> u32 {
        self.entries.get(&(account.to_owned(), peer)).map_or(0, |s| s.count)
    }
    pub fn newest(&self, account: &str, peer: PeerId) -> Option<i32> {
        self.entries.get(&(account.to_owned(), peer)).map(|s| s.newest)
    }
    pub fn any(&self) -> bool { !self.entries.is_empty() }
    /// Keep all badges/counts, but alarm only for explicitly unmuted chats.
    pub fn any_alertable(&self, notifications: &crate::notifications::Notifications, now: u64) -> bool {
        self.entries.keys().any(|(account, peer)|
            notifications.status(account, *peer, now) == crate::notifications::MuteStatus::Unmuted)
    }
    pub fn retain_accounts(&mut self, known: &std::collections::HashSet<String>) {
        self.entries.retain(|(a, _), _| known.contains(a));
        self.read_through.retain(|(a, _), _| known.contains(a));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_and_remote_read_track_each_account_and_chat() {
        let p = PeerId::user(1); let q = PeerId::user(2);
        let mut u = Unread::default();
        u.snapshot("a", p, 3, 20, 10);
        u.incoming("a", q, 21);
        u.incoming("b", p, 22);
        assert_eq!(u.count("a", p), 3);
        u.read("a", p, 20, Some(0));
        assert_eq!(u.count("a", p), 0);
        assert!(u.any());
        u.read("a", q, 21, Some(0)); u.read("b", p, 22, Some(0));
        assert!(!u.any());
        u.snapshot("a", p, 3, 20, 10); // delayed stale server snapshot
        assert!(!u.any());
    }
    #[test]
    fn muted_unread_is_counted_but_does_not_sound_alarm() {
        use crate::notifications::{Category, Notifications};
        use std::time::{Duration, Instant};
        let p = PeerId::user(1); let q = PeerId::user(2);
        let t = Instant::now(); let mut u = Unread::default(); let mut settings = Notifications::default();
        u.incoming("a", p, 1);
        settings.set_category("a", p, Category::User);
        settings.set_peer("a", p, Some(500), t);
        assert_eq!(u.count("a", p), 1);
        assert!(!u.any_alertable(&settings, 100));
        u.incoming("a", q, 2);
        assert!(!u.any_alertable(&settings, 100)); // unknown is not assumed unmuted
        settings.set_category("a", q, Category::User);
        settings.set_peer("a", q, Some(0), t);
        assert!(u.any_alertable(&settings, 100));
        u.read("a", q, 2, Some(0));
        assert!(!u.any_alertable(&settings, 100));
        assert!(u.any_alertable(&settings, 500)); // timed mute expired
        settings.set_peer("a", p, Some(0), t + Duration::from_secs(1));
        assert!(u.any_alertable(&settings, 100)); // live unmute wakes the alarm
        settings.set_peer("a", p, Some(i32::MAX), t + Duration::from_secs(2));
        assert!(!u.any_alertable(&settings, 100));
    }

    #[test]
    fn old_ack_cannot_clear_newer_incoming_or_snapshot() {
        let p = PeerId::user(1); let mut u = Unread::default();
        u.snapshot("a", p, 1, 10, 9);
        u.incoming("a", p, 11);
        u.incoming("a", p, 11);
        u.read("a", p, 10, Some(0));
        assert_eq!(u.count("a", p), 1);
        u.snapshot("a", p, 0, 10, 10);
        assert!(u.any());
        u.read("a", p, 11, Some(0));
        assert!(!u.any());
    }
}
