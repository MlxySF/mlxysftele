//! Native egui GUI — the primary interface of mlxysftele.
//!
//! Architecture
//! ============
//! * egui (via eframe) runs on the main thread.
//! * All MTProto traffic lives on a dedicated background thread with its own
//!   multi-thread tokio runtime, behind an `UnboundedReceiver<Cmd>`.
//! * Events flow back over a std mpsc channel; the worker calls
//!   [`egui::Context::request_repaint`] after every event so the UI reacts
//!   instantly without any busy polling.
//! * **Every registered account keeps a live connection for the lifetime of
//!   the process** — "accounts stay active". There is no lazy connect, no
//!   idle reaper, no hot-set here; that was the superseded headless daemon
//!   model. An account that fails to authenticate is shown as signed-out in
//!   the sidebar instead of being dropped.
//!
//! RAM note: chat history is capped at [`MAX_CACHED_MESSAGES`] per dialog and
//! the dialog list at [`MAX_DIALOGS`] — a client that renders everything it
//! has ever seen would not be ZapFast-style.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use eframe::egui;

use grammers_client::client::{LoginToken, PasswordToken, UpdatesConfiguration};
use grammers_client::client::SignInError;
use grammers_client::update::Update;
use grammers_session::types::PeerId;
use grammers_session::types::PeerRef;
use grammers_client::Client;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::config::{AccountMeta, Config};
use crate::premium::{self, PremiumToggles};
use crate::telegram;
use crate::video::VideoPlayer;
use crate::util;
use crate::{log_err, log_warn};

/// Cap for cached dialogs per account.
const MAX_DIALOGS: usize = 150;
/// Cap for cached messages per open dialog.
const MAX_CACHED_MESSAGES: usize = 100;
/// Messages fetched when a dialog is opened.
const HISTORY_PAGE: usize = 40;

// ---------------------------------------------------------------------------
// Commands (GUI -> worker) and events (worker -> GUI)
// ---------------------------------------------------------------------------

enum Cmd {
    /// Save Telegram API credentials; afterwards any account without a live
    /// connection gets one.
    SaveCreds { api_id: i32, api_hash: String },
    RefreshDialogs { account: String },
    OpenChat { account: String, peer_id: PeerId },
    SendText { account: String, peer_id: PeerId, text: String },
    /// Send a local image file as a photo; `caption` may be empty.
    SendPhoto { account: String, peer_id: PeerId, path: PathBuf, caption: String },
    /// Download a video message for inline playback (user-tapped; never automatic).
    OpenVideo { account: String, peer_id: PeerId, msg_id: i32 },
    /// GUI → worker echo of `Evt::StashMedia`; stashes the raw Media handle.
    StashMedia { cache_key: String, media: grammers_client::media::Media },
    BeginLogin { name: String, phone: String },
    SubmitCode { name: String, code: String },
    SubmitPassword { name: String, password: String },
    /// Sign out on Telegram (best effort) and wipe local session + registry entry.
    Logout { name: String },
    /// Wipe local session + registry entry only.
    RemoveLocal { name: String },
    /// Flip a local premium flag (`None` = master switch).
    SetLocalPremium { flag: Option<String>, on: bool },
}

#[derive(Clone)]
struct DialogItem {
    peer_id: PeerId,
    name: String,
    preview: Option<String>,
}

/// Media carried by a chat message.
#[derive(Clone)]
enum MsgMedia {
    /// An image Telegram gave us (photo / image document / webp sticker);
    /// downloaded to the local cache for inline rendering.
    Photo,
    /// A video document; tapped to download & play inline (embedded libmpv).
    Video(String),
    /// Anything else — rendered as a muted caption label.
    Label(String),
}

#[derive(Clone)]
struct MsgItem {
    id: i32,
    outgoing: bool,
    unix: i64,
    text: String,
    media: Option<MsgMedia>,
}

/// Cache key used for an image attached to `msg_id` in `account`'s chat `peer_id`.
fn media_cache_key(account: &str, peer_id: PeerId, msg_id: i32) -> String {
    format!("{account}_{}_{msg_id}", peer_id)
}

/// Don't auto-download anything larger than this.
const MAX_MEDIA_BYTES: u64 = 24 * 1024 * 1024;

/// Full-screen image viewer state (ZapFast-style click-to-zoom).
struct ViewerState {
    /// Key into `textures`.
    key: String,
    /// Multiplier on top of the fit-to-screen scale (1.0 = fit).
    zoom: f32,
    /// Pan offset from the screen center, in points.
    pan: egui::Vec2,
}

#[derive(Clone)]
struct MeInfo {
    full_name: String,
    username: Option<String>,
    user_id: i64,
    server_premium: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum ConnStatus {
    Connecting,
    Online,
    Unauthorized,
    Error(String),
}

#[derive(Clone)]
enum LoginEvt {
    NeedCode,
    NeedPassword,
    Done,
    Failed(String),
}

enum Evt {
    Config(Config),
    Status { name: String, status: ConnStatus },
    Me { name: String, me: MeInfo },
    Dialogs { name: String, items: Vec<DialogItem> },
    History { account: String, peer_id: PeerId, messages: Vec<MsgItem> },
    Incoming { account: String, peer_id: PeerId, msg: MsgItem },
    Sent {
        account: String,
        peer_id: PeerId,
        result: Result<MsgItem, String>,
        /// Local source path when the sent message carries a photo we sent —
        /// the GUI can cache it for display without re-downloading.
        source_path: Option<PathBuf>,
    },
    /// Hand a video Media handle to the GUI, which echoes it back as
    /// `Cmd::StashMedia` (the worker updater cannot borrow `self`).
    StashMedia { cache_key: String, media: grammers_client::media::Media },
    /// An image finished downloading (or failed — empty `path`).
    MediaReady {
        account: String,
        peer_id: PeerId,
        msg_id: i32,
        cache_key: String,
        path: PathBuf,
        /// Photo vs video — decides whether the GUI decodes a texture or opens the player.
        kind: MsgMedia,
    },
    Login { name: String, state: LoginEvt },
    Toast(String),
}

// ---------------------------------------------------------------------------
// Worker side (background tokio thread). Owns the Config so the GUI never
// writes config.json behind its back — single writer, no races.
// ---------------------------------------------------------------------------

/// Classify a message's media. Returns `(kind, downloadable, size_hint)`.
fn split_media(
    message: &grammers_client::message::Message,
) -> Option<(MsgMedia, Option<grammers_client::media::Media>, Option<u64>)> {
    use grammers_client::media::Media;
    let media = message.media()?;
    let triple = match media {
        Media::Photo(p) => (MsgMedia::Photo, Some(Media::Photo(p.clone())), p.size().map(|s| s as u64)),
        Media::Document(d) => {
            let mime = d.mime_type().unwrap_or("").to_string();
            let name = d.name().map(str::to_string);
            let size = d.size().map(|s| s as u64);
            // We can only decode jpeg/png/bmp in-process; show the rest as captions.
            let decodable = matches!(mime.as_str(), "image/jpeg" | "image/png" | "image/bmp");
            if decodable {
                (MsgMedia::Photo, Some(Media::Document(d.clone())), size)
            } else if mime.starts_with("video/") {
                let label = match (&name, size) {
                    (Some(n), Some(s)) => format!("{n} ({})", util::human_bytes(s)),
                    (Some(n), None) => n.clone(),
                    (None, Some(s)) => format!("{} · {}", mime, util::human_bytes(s)),
                    (None, None) => "video".to_string(),
                };
                (MsgMedia::Video(label), Some(Media::Document(d.clone())), size)
            } else if mime.starts_with("image/") {
                let label = name.unwrap_or_else(|| "image".into());
                (MsgMedia::Label(label), None, None)
            } else {
                let label = match (&name, size) {
                    (Some(n), Some(s)) => format!("{n} ({})", util::human_bytes(s)),
                    (Some(n), None) => n.clone(),
                    (None, Some(s)) => format!("{} · {}", mime, util::human_bytes(s)),
                    (None, None) => {
                        if mime.is_empty() { "document".into() } else { mime.clone() }
                    }
                };
                (MsgMedia::Label(label), None, None)
            }
        }
        Media::Sticker(_) => (MsgMedia::Label("sticker".into()), None, None),
        Media::Contact(_) => (MsgMedia::Label("contact".into()), None, None),
        Media::Poll(_) => (MsgMedia::Label("poll".into()), None, None),
        Media::Geo(_) | Media::Venue(_) | Media::GeoLive(_) => {
            (MsgMedia::Label("location".into()), None, None)
        }
        Media::Dice(_) => (MsgMedia::Label("dice".into()), None, None),
        Media::WebPage(_) => (MsgMedia::Label("link preview".into()), None, None),
        // Media is #[non_exhaustive]; stay honest about unknown variants.
        _ => (MsgMedia::Label("media".into()), None, None),
    };
    Some(triple)
}

/// Format seconds as `m:ss` (or `h:mm:ss` past an hour) for the video HUD.
fn format_mmss(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".to_string();
    }
    let total = seconds as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn media_extension(media: &grammers_client::media::Media) -> String {
    use grammers_client::media::Media;
    match media {
        Media::Photo(_) => "jpg".into(),
        Media::Document(d) => {
            // Prefer the original filename extension (keeps mkv/mov/… honest).
            let from_name = d
                .name()
                .and_then(|n| std::path::Path::new(n).extension().map(|e| e.to_string_lossy().into_owned()))
                .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()));
            if let Some(ext) = from_name {
                return ext.to_ascii_lowercase();
            }
            match d.mime_type().unwrap_or("") {
                "image/png" => "png",
                "image/bmp" => "bmp",
                "video/webm" => "webm",
                "video/x-matroska" => "mkv",
                "video/quicktime" => "mov",
                "video/x-msvideo" => "avi",
                "video/mpeg" => "mpg",
                "video/3gpp" => "3gp",
                "video/ogg" => "ogv",
                _ => "mp4",
            }
            .into()
        }
        _ => "bin".into(),
    }
}

fn video_label_for(media: &grammers_client::media::Media) -> String {
    use grammers_client::media::Media;
    match media {
        Media::Document(d) => d
            .name()
            .map(str::to_string)
            .or_else(|| d.mime_type().map(|m| m.to_string()))
            .unwrap_or_else(|| "video".into()),
        _ => "video".into(),
    }
}

/// Largest video we will auto-start downloading (honest cap; bigger files toast instead).
const MAX_VIDEO_BYTES: u64 = 512 * 1024 * 1024;

/// Download an image to `<data_dir>/cache/media/<cache_key>.<ext>` off the
/// critical path; the GUI gets an `Evt::MediaReady` when it's on disk.
#[allow(clippy::too_many_arguments)]
fn spawn_media_download(
    client: &Client,
    data_dir: &std::path::Path,
    account: &str,
    peer_id: PeerId,
    msg_id: i32,
    media: grammers_client::media::Media,
    size_hint: Option<u64>,
    evt_tx: &std_mpsc::Sender<Evt>,
    ctx: &egui::Context,
    cap_bytes: u64,
) {
    if let Some(size) = size_hint {
        if size > cap_bytes {
            return;
        }
    }
    let account_s = account.to_string();
    let cache_key = media_cache_key(account, peer_id, msg_id);
    let dir = data_dir.join("cache").join("media");
    let _ = std::fs::create_dir_all(&dir);
    let kind: MsgMedia = match &media {
        grammers_client::media::Media::Document(d)
            if d.mime_type().unwrap_or("").starts_with("video/") =>
        {
            MsgMedia::Video(video_label_for(&media))
        }
        _ => MsgMedia::Photo,
    };
    let path = dir.join(format!("{cache_key}.{}", media_extension(&media)));
    if path.exists() {
        let _ = evt_tx.send(Evt::MediaReady { account: account_s, peer_id, msg_id, cache_key, path, kind });
        ctx.request_repaint();
        return;
    }
    let client = client.clone();
    let evt_tx = evt_tx.clone();
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let ok = client.download_media(&media, &path).await.is_ok();
        let _ = evt_tx.send(Evt::MediaReady {
            account: account_s,
            peer_id,
            msg_id,
            cache_key,
            // An empty path signals "download failed" to the GUI.
            path: if ok { path } else { PathBuf::new() },
            kind,
        });
        ctx.request_repaint();
    });
}


struct AccountConn {
    client: Client,
    pool: tokio::task::JoinHandle<()>,
    /// Latest known peers, refreshed with every dialog list fetch.
    peer_refs: HashMap<PeerId, PeerRef>,
}

enum PendingLogin {
    AwaitingCode {
        client: Client,
        pool: tokio::task::JoinHandle<()>,
        phone: String,
        token: LoginToken,
    },
    AwaitingPassword {
        client: Client,
        pool: tokio::task::JoinHandle<()>,
        phone: String,
        token: PasswordToken,
    },
}

struct Worker {
    data_dir: PathBuf,
    cfg: Config,
    evt_tx: std_mpsc::Sender<Evt>,
    ctx: egui::Context,
    conns: HashMap<String, AccountConn>,
    pending_logins: HashMap<String, PendingLogin>,
    /// Kept-alive raw video Media handles for on-demand downloads (capped).
    media_objects: HashMap<String, grammers_client::media::Media>,
}

impl Worker {
    fn emit(&self, evt: Evt) {
        let _ = self.evt_tx.send(evt);
        self.ctx.request_repaint();
    }

    fn broadcast_config(&self) {
        self.emit(Evt::Config(self.cfg.clone()));
    }

    fn api_creds(&self) -> Result<(i32, String), String> {
        self.cfg
            .effective_api_creds()
            .ok_or_else(|| {
                "missing Telegram API credentials — open Settings (gear) and save them"
                    .to_string()
            })
    }

    async fn open_session(
        &self,
        name: &str,
        api_id: i32,
        want_updates: bool,
    ) -> Result<(Client, tokio::task::JoinHandle<()>, Option<tokio::sync::mpsc::UnboundedReceiver<grammers_session::updates::UpdatesLike>>), String> {
        let conn = telegram::connect(&self.data_dir, api_id, name, want_updates)
            .await
            .map_err(|e| e.to_string())?;
        Ok((conn.client, conn.pool, conn.updates))
    }

    /// Connect one account, verify auth, capture identity, start keep-alive
    /// update stream. Failures still report a status so the GUI can show it.
    async fn bring_online(&mut self, name: String) {
        let Ok((api_id, _)) = self.api_creds() else {
            self.emit(Evt::Status {
                name,
                status: ConnStatus::Error("missing API credentials".into()),
            });
            return;
        };
        self.emit(Evt::Status { name: name.clone(), status: ConnStatus::Connecting });
        let connected = self.open_session(&name, api_id, true).await;
        let (client, pool, updates) = match connected {
            Ok(parts) => parts,
            Err(e) => {
                self.emit(Evt::Status { name, status: ConnStatus::Error(e) });
                return;
            }
        };
        match client.is_authorized().await {
            Ok(true) => {}
            Ok(false) => {
                telegram::shutdown(client, pool).await;
                self.emit(Evt::Status { name, status: ConnStatus::Unauthorized });
                return;
            }
            Err(e) => {
                let msg = e.to_string();
                telegram::shutdown(client, pool).await;
                self.emit(Evt::Status { name, status: ConnStatus::Error(msg) });
                return;
            }
        }
        // Capture identity (and keep the "whoami" data fresh).
        if let Ok(me) = client.get_me().await {
            let info = MeInfo {
                full_name: me.full_name(),
                username: me.username().map(str::to_string),
                user_id: me.id().bot_api_dialog_id(),
                server_premium: me.is_premium(),
            };
            // Refresh registry username silently.
            if let Some(acc) = self.cfg.find_account_mut(&name) {
                acc.username = info.username.clone();
                acc.user_id = Some(info.user_id);
            }
            let _ = self.cfg.save(&self.data_dir);
            self.broadcast_config();
            self.emit(Evt::Me { name: name.clone(), me: info });
        }
        self.emit(Evt::Status { name: name.clone(), status: ConnStatus::Online });

        // Keep the update stream hot — this is what keeps the account active.
        let evt_tx = self.evt_tx.clone();
        let ctx = self.ctx.clone();
        let account = name.clone();
        let data_dir = self.data_dir.clone();
        let update_client = client.clone();
        let download_client = client.clone();
        if let Some(updates_rx) = updates {
            tokio::spawn(async move {
                let mut stream = update_client
                    .stream_updates(
                        updates_rx,
                        UpdatesConfiguration { catch_up: false, ..Default::default() },
                    )
                    .await;
                loop {
                    match stream.next().await {
                        Ok(Update::NewMessage(message)) => {
                            let media = split_media(&message);
                            let msg_id = message.id();
                            let peer_id = message.peer_id();
                            let msg = MsgItem {
                                id: msg_id,
                                outgoing: message.outgoing(),
                                unix: message.date().timestamp(),
                                text: message.text().to_string(),
                                media: media.as_ref().map(|(kind, _, _)| kind.clone()),
                            };
                            if let Some((kind, obj, size)) = media {
                                let cache_key = media_cache_key(&account, peer_id, msg_id);
                                match kind {
                                    MsgMedia::Photo => {
                                        if let Some(o) = obj {
                                            spawn_media_download(
                                                &download_client,
                                                &data_dir,
                                                &account,
                                                peer_id,
                                                msg_id,
                                                o,
                                                size,
                                                &evt_tx,
                                                &ctx,
                                                MAX_MEDIA_BYTES,
                                            );
                                        }
                                    }
                                    MsgMedia::Video(_) => {
                                        if let Some(o) = obj {
                                            let _ = evt_tx.send(Evt::StashMedia { cache_key, media: o });
                                        }
                                    }
                                    MsgMedia::Label(_) => {}
                                }
                            }
                            let _ = evt_tx.send(Evt::Incoming {
                                account: account.clone(),
                                peer_id,
                                msg,
                            });
                            ctx.request_repaint();
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = evt_tx.send(Evt::Status {
                                name: account.clone(),
                                status: ConnStatus::Error(format!("update stream ended: {e}")),
                            });
                            ctx.request_repaint();
                            break;
                        }
                    }
                }
                // Update stream ended; persist the sync state so the next
                // session starts from a consistent point.
                stream.sync_update_state().await;
            });
        }
        self.conns.insert(name.clone(), AccountConn { client, pool, peer_refs: HashMap::new() });

        // Pre-load dialogs so the chat list is warm.
        self.refresh_dialogs(&name).await;
    }

    async fn refresh_dialogs(&mut self, name: &str) {
        let Some(conn) = self.conns.get_mut(name) else { return };
        let mut iter = conn.client.iter_dialogs().limit(MAX_DIALOGS);
        let mut items = Vec::with_capacity(MAX_DIALOGS);
        while let Ok(Some(dialog)) = iter.next().await {
            let peer_id = dialog.peer_id();
            let peer_ref = dialog.peer_ref();
            conn.peer_refs.insert(peer_id, peer_ref);
            let preview = dialog.last_message.as_ref().map(|m| {
                m.text()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            let preview = preview.and_then(|p| {
                let p = p.trim().to_string();
                if p.is_empty() { None } else { Some(truncate(&p, 80)) }
            });
            items.push(DialogItem {
                peer_id,
                name: dialog.peer().name().unwrap_or("?").to_string(),
                preview,
            });
        }
        self.emit(Evt::Dialogs { name: name.to_string(), items });
    }

    async fn fetch_history(&mut self, account: &str, peer_id: PeerId) {
        let Some(conn) = self.conns.get(account) else { return };
        let Some(&peer_ref) = conn.peer_refs.get(&peer_id) else {
            self.emit(Evt::Toast("chat not resolved yet — refresh the chat list".into()));
            return;
        };
        let mut iter = conn.client.iter_messages(peer_ref).limit(HISTORY_PAGE);
        let mut messages = Vec::with_capacity(HISTORY_PAGE);
        let mut downloads = Vec::new();
        while let Ok(Some(message)) = iter.next().await {
            let id = message.id();
            let media = split_media(&message);
            let msg = MsgItem {
                id,
                outgoing: message.outgoing(),
                unix: message.date().timestamp(),
                text: message.text().to_string(),
                media: media.as_ref().map(|(kind, _, _)| kind.clone()),
            };
            if let Some((kind, obj, size)) = media {
                let cache_key = media_cache_key(account, peer_id, id);
                match kind {
                    // Photos auto-download (small); videos wait for a tap.
                    MsgMedia::Photo => {
                        if let Some(o) = obj {
                            downloads.push((id, o, size));
                        }
                    }
                    MsgMedia::Video(_) => {
                        if let Some(o) = obj {
                            if self.media_objects.len() >= 96 {
                                self.media_objects.clear();
                            }
                            self.media_objects.insert(cache_key, o);
                        }
                    }
                    MsgMedia::Label(_) => {}
                }
            }
            messages.push(msg);
        }
        messages.reverse(); // newest first from the iterator; show oldest on top
        // Emit history first so text paints immediately…
        self.emit(Evt::History { account: account.to_string(), peer_id, messages });
        // …then fetch images in the background (newest 12).
        if let Some(conn) = self.conns.get(account) {
            for (msg_id, media, size) in downloads.into_iter().take(12) {
                spawn_media_download(
                    &conn.client,
                    &self.data_dir,
                    account,
                    peer_id,
                    msg_id,
                    media,
                    size,
                    &self.evt_tx,
                    &self.ctx,
                    MAX_MEDIA_BYTES,
                );
            }
        }
    }

    async fn send_text(&mut self, account: &str, peer_id: PeerId, text: String) {
        let Some(conn) = self.conns.get(account) else {
            self.emit(Evt::Sent {
                account: account.to_string(),
                peer_id,
                result: Err("account is not connected".into()),
                source_path: None,
            });
            return;
        };
        let Some(&peer_ref) = conn.peer_refs.get(&peer_id) else {
            self.emit(Evt::Sent {
                account: account.to_string(),
                peer_id,
                result: Err("peer not resolved — refresh the chat list".into()),
                source_path: None,
            });
            return;
        };
        let result = match conn.client.send_message(peer_ref, text.as_str()).await {
            Ok(message) => Ok(MsgItem {
                id: message.id(),
                outgoing: true,
                unix: message.date().timestamp(),
                text,
                media: None,
            }),
            Err(e) => Err(e.to_string()),
        };
        self.emit(Evt::Sent { account: account.to_string(), peer_id, result, source_path: None });
    }

    async fn send_photo(&mut self, account: &str, peer_id: PeerId, path: PathBuf, caption: String) {
        let Some(conn) = self.conns.get(account) else {
            self.emit(Evt::Sent {
                account: account.to_string(),
                peer_id,
                result: Err("account is not connected".into()),
                source_path: None,
            });
            return;
        };
        let Some(&peer_ref) = conn.peer_refs.get(&peer_id) else {
            self.emit(Evt::Sent {
                account: account.to_string(),
                peer_id,
                result: Err("peer not resolved — refresh the chat list".into()),
                source_path: None,
            });
            return;
        };
        let uploaded = conn.client.upload_file(&path).await.map_err(|e| e.to_string());
        let result = match uploaded {
            Ok(uploaded) => {
                let message = grammers_client::message::InputMessage::new()
                    .text(caption.clone())
                    .photo(uploaded);
                conn.client
                    .send_message(peer_ref, message)
                    .await
                    .map(|message| MsgItem {
                        id: message.id(),
                        outgoing: true,
                        unix: message.date().timestamp(),
                        text: caption,
                        media: Some(MsgMedia::Photo),
                    })
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(format!("upload failed: {e}")),
        };
        self.emit(Evt::Sent { account: account.to_string(), peer_id, result, source_path: Some(path) });
    }

    /// Kick off (or fast-path) an embedded playback for a video message.
    async fn open_video(&mut self, account: &str, peer_id: PeerId, msg_id: i32) {
        let Some(client) = self.conns.get(account).map(|c| c.client.clone()) else {
            self.emit(Evt::Toast("account is not connected".into()));
            return;
        };
        let cache_key = media_cache_key(account, peer_id, msg_id);
        let dir = self.data_dir.join("cache").join("media");
        // Fast path: already downloaded under any known extension.
        for ext in ["mp4", "webm", "mkv", "mov", "avi", "mpg", "3gp", "ogv", "mpeg", "bin"] {
            let candidate = dir.join(format!("{cache_key}.{ext}"));
            if candidate.exists() {
                self.emit(Evt::MediaReady {
                    account: account.to_string(),
                    peer_id,
                    msg_id,
                    cache_key,
                    path: candidate,
                    kind: MsgMedia::Video(String::new()),
                });
                return;
            }
        }
        let Some(media) = self.media_objects.get(&cache_key).cloned() else {
            self.emit(Evt::Toast(
                "video unavailable — reopen the chat (media list was refreshed)".into(),
            ));
            return;
        };
        let size_hint = match &media {
            grammers_client::media::Media::Document(d) => d.size().map(|s| s as u64),
            _ => None,
        };
        if let Some(size) = size_hint {
            if size > MAX_VIDEO_BYTES {
                self.emit(Evt::Toast(format!(
                    "video is {} — too large to download here (cap {})",
                    util::human_bytes(size),
                    util::human_bytes(MAX_VIDEO_BYTES),
                )));
                return;
            }
            self.emit(Evt::Toast(format!("downloading video ({})…", util::human_bytes(size))));
        } else {
            self.emit(Evt::Toast("downloading video…".into()));
        }
        spawn_media_download(
            &client,
            &self.data_dir,
            account,
            peer_id,
            msg_id,
            media,
            size_hint,
            &self.evt_tx,
            &self.ctx,
            MAX_VIDEO_BYTES,
        );
    }

    fn persist_login(&mut self, name: &str, phone: &str, user: &grammers_client::peer::User) {
        let now = util::now_unix();
        let created = self
            .cfg
            .find_account(name)
            .map(|a| a.created_unix)
            .unwrap_or(now);
        self.cfg.upsert_account(AccountMeta {
            name: name.to_string(),
            phone: Some(phone.to_string()),
            user_id: Some(user.id().bot_api_dialog_id()),
            username: user.username().map(str::to_string),
            created_unix: created,
            last_login_unix: Some(now),
        });
        if let Err(e) = self.cfg.save(&self.data_dir) {
            log_warn!("failed to save config after login: {e}");
        }
    }

    async fn begin_login(&mut self, name: String, phone: String) {
        if !util::valid_account_name(&name) {
            self.emit(Evt::Login {
                name,
                state: LoginEvt::Failed(
                    "invalid account name (allowed: a-z A-Z 0-9 _ - ., max 64 chars)".into(),
                ),
            });
            return;
        }
        if self.cfg.find_account(&name).is_some() || self.pending_logins.contains_key(&name) {
            self.emit(Evt::Login {
                name,
                state: LoginEvt::Failed("an account with that name already exists".into()),
            });
            return;
        }
        let (api_id, api_hash) = match self.api_creds() {
            Ok(c) => c,
            Err(e) => {
                self.emit(Evt::Login { name, state: LoginEvt::Failed(e) });
                return;
            }
        };
        // Fresh login: move any previous session aside rather than mixing credentials.
        let session = util::session_path(&self.data_dir, &name);
        if session.exists() {
            let backup = session.with_extension(format!("sqlite.bak-{}", util::now_unix()));
            let _ = std::fs::rename(&session, &backup);
        }
        let (client, pool, _) = match self.open_session(&name, api_id, false).await {
            Ok(parts) => parts,
            Err(e) => {
                self.emit(Evt::Login { name, state: LoginEvt::Failed(e) });
                return;
            }
        };
        match client.request_login_code(phone.trim(), &api_hash).await {
            Ok(token) => {
                self.pending_logins.insert(
                    name.clone(),
                    PendingLogin::AwaitingCode { client, pool, phone: phone.trim().to_string(), token },
                );
                self.emit(Evt::Login { name, state: LoginEvt::NeedCode });
            }
            Err(e) => {
                let msg = format!("code request failed: {e}");
                telegram::shutdown(client, pool).await;
                self.emit(Evt::Login { name, state: LoginEvt::Failed(msg) });
            }
        }
    }

    async fn submit_code(&mut self, name: String, code: String) {
        let Some(pending) = self.pending_logins.remove(&name) else { return };
        let PendingLogin::AwaitingCode { client, pool, phone, token } = pending else {
            self.pending_logins.insert(name, pending);
            return;
        };
        match client.sign_in(&token, code.trim()).await {
            Ok(user) => self.finish_login(name, phone, client, pool, user).await,
            Err(SignInError::PasswordRequired(password_token)) => {
                self.pending_logins.insert(
                    name.clone(),
                    PendingLogin::AwaitingPassword { client, pool, phone, token: password_token },
                );
                self.emit(Evt::Login { name, state: LoginEvt::NeedPassword });
            }
            Err(SignInError::InvalidCode) => {
                self.pending_logins.insert(
                    name.clone(),
                    PendingLogin::AwaitingCode { client, pool, phone, token },
                );
                self.emit(Evt::Login { name, state: LoginEvt::Failed("invalid code — try again".into()) });
            }
            Err(e) => {
                let msg = format!("{e}");
                telegram::shutdown(client, pool).await;
                self.emit(Evt::Login { name, state: LoginEvt::Failed(msg) });
            }
        }
    }

    async fn submit_password(&mut self, name: String, password: String) {
        let Some(pending) = self.pending_logins.remove(&name) else { return };
        let PendingLogin::AwaitingPassword { client, pool, phone, token } = pending else {
            self.pending_logins.insert(name, pending);
            return;
        };
        match client.check_password(token, password.as_bytes()).await {
            Ok(user) => self.finish_login(name, phone, client, pool, user).await,
            Err(SignInError::InvalidPassword(password_token)) => {
                self.pending_logins.insert(
                    name.clone(),
                    PendingLogin::AwaitingPassword { client, pool, phone, token: password_token },
                );
                self.emit(Evt::Login { name, state: LoginEvt::Failed("wrong 2FA password".into()) });
            }
            Err(e) => {
                let msg = format!("{e}");
                telegram::shutdown(client, pool).await;
                self.emit(Evt::Login { name, state: LoginEvt::Failed(msg) });
            }
        }
    }

    async fn finish_login(
        &mut self,
        name: String,
        phone: String,
        client: Client,
        pool: tokio::task::JoinHandle<()>,
        user: grammers_client::peer::User,
    ) {
        self.persist_login(&name, &phone, &user);
        // Drop this throwaway RPC-only connection; bring the account online
        // properly so dialogs + the update stream come up together.
        telegram::shutdown(client, pool).await;
        self.emit(Evt::Login { name: name.clone(), state: LoginEvt::Done });
        self.broadcast_config();
        self.bring_online(name).await;
    }

    async fn logout(&mut self, name: String) {
        if let Some(conn) = self.conns.remove(&name) {
            match conn.client.sign_out().await {
                Ok(_) => log_info_toast(self, format!("[{name}] signed out on Telegram")),
                Err(e) => {
                    log_warn!("[{name}] sign_out RPC failed: {e}");
                    self.emit(Evt::Toast(format!("[{name}] sign-out RPC failed ({e}); removed locally anyway")));
                }
            }
            telegram::shutdown(conn.client, conn.pool).await;
        }
        self.wipe_local(&name);
    }

    fn remove_local(&mut self, name: String) {
        if let Some(conn) = self.conns.remove(&name) {
            let (client, pool) = (conn.client, conn.pool);
            tokio::spawn(async move { telegram::shutdown(client, pool).await });
        }
        self.wipe_local(&name);
    }

    fn wipe_local(&mut self, name: &str) {
        let session = util::session_path(&self.data_dir, name);
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", session.display()));
            let _ = std::fs::remove_file(path);
        }
        self.cfg.remove_account(name);
        if let Err(e) = self.cfg.save(&self.data_dir) {
            log_err!("failed to save config: {e}");
        }
        self.broadcast_config();
        self.emit(Evt::Toast(format!("[{name}] removed from this client")));
    }

    fn set_local_premium(&mut self, flag: Option<String>, on: bool) {
        match flag {
            None => self.cfg.local_premium.set_all(on),
            Some(key) => {
                if !self.cfg.local_premium.set_named(&key, on) {
                    self.emit(Evt::Toast(format!("unknown premium flag '{key}'")));
                    return;
                }
            }
        }
        if let Err(e) = self.cfg.save(&self.data_dir) {
            self.emit(Evt::Toast(format!("failed to save premium settings: {e}")));
            return;
        }
        self.broadcast_config();
    }

    fn save_creds(&mut self, api_id: i32, api_hash: String) {
        self.cfg.api_id = Some(api_id);
        self.cfg.api_hash = Some(api_hash);
        if let Err(e) = self.cfg.save(&self.data_dir) {
            self.emit(Evt::Toast(format!("failed to save credentials: {e}")));
            return;
        }
        self.emit(Evt::Toast("API credentials saved — connecting accounts…".into()));
        self.broadcast_config();
    }

    async fn run(mut self, mut cmd_rx: UnboundedReceiver<Cmd>) {
        // Announce the starting config right away.
        self.broadcast_config();
        // Bring every registered account online — they STAY online.
        let names: Vec<String> = self.cfg.accounts.iter().map(|a| a.name.clone()).collect();
        for name in names {
            // Each connection is independent; failures are reported per account.
            self.bring_online(name).await;
        }
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Cmd::SaveCreds { api_id, api_hash } => {
                    self.save_creds(api_id, api_hash);
                    // Any account without a live connection gets one now.
                    let missing: Vec<String> = self
                        .cfg
                        .accounts
                        .iter()
                        .map(|a| a.name.clone())
                        .filter(|n| !self.conns.contains_key(n))
                        .collect();
                    for name in missing {
                        self.bring_online(name).await;
                    }
                }
                Cmd::RefreshDialogs { account } => self.refresh_dialogs(&account).await,
                Cmd::OpenChat { account, peer_id } => self.fetch_history(&account, peer_id).await,
                Cmd::SendText { account, peer_id, text } => {
                    self.send_text(&account, peer_id, text).await
                }
                Cmd::SendPhoto { account, peer_id, path, caption } => {
                    self.send_photo(&account, peer_id, path, caption).await
                }
                Cmd::OpenVideo { account, peer_id, msg_id } => {
                    self.open_video(&account, peer_id, msg_id).await
                }
                Cmd::StashMedia { cache_key, media } => {
                    if self.media_objects.len() >= 96 {
                        self.media_objects.clear();
                    }
                    self.media_objects.insert(cache_key, media);
                }
                Cmd::BeginLogin { name, phone } => self.begin_login(name, phone).await,
                Cmd::SubmitCode { name, code } => self.submit_code(name, code).await,
                Cmd::SubmitPassword { name, password } => self.submit_password(name, password).await,
                Cmd::Logout { name } => self.logout(name).await,
                Cmd::RemoveLocal { name } => self.remove_local(name),
                Cmd::SetLocalPremium { flag, on } => self.set_local_premium(flag, on),
            }
        }
    }
}

fn log_info_toast(worker: &Worker, msg: String) {
    crate::log_info!("{msg}");
    worker.emit(Evt::Toast(msg));
}

fn spawn_worker(
    data_dir: PathBuf,
    cmd_rx: UnboundedReceiver<Cmd>,
    evt_tx: std_mpsc::Sender<Evt>,
    ctx: egui::Context,
) {
    std::thread::Builder::new()
        .name("mlxysftele-worker".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = evt_tx.send(Evt::Toast(format!("failed to start tokio runtime: {e}")));
                    return;
                }
            };
            let cfg = Config::load(&data_dir).unwrap_or_else(|_| Config::default());
            let worker = Worker { data_dir, cfg, evt_tx, ctx, conns: HashMap::new(), pending_logins: HashMap::new(), media_objects: HashMap::new() };
            rt.block_on(worker.run(cmd_rx));
        })
        .expect("failed to spawn worker thread");
}

// ---------------------------------------------------------------------------
// GUI side (egui immediate mode, main thread)
// ---------------------------------------------------------------------------

pub fn run(data_dir: PathBuf) -> Result<(), telegram::BoxError> {
    let (cmd_tx, cmd_rx) = unbounded_channel::<Cmd>();
    let (evt_tx, evt_rx) = std_mpsc::channel::<Evt>();
    let cfg = Config::load(&data_dir).unwrap_or_default();
    let show_add = cfg.accounts.is_empty();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 730.0])
            .with_min_inner_size([880.0, 560.0])
            .with_title("mlxysftele"),
        ..Default::default()
    };
    let data_dir_worker = data_dir.clone();
    eframe::run_native(
        "mlxysftele",
        options,
        Box::new(move |cc| {
            spawn_worker(data_dir_worker, cmd_rx, evt_tx, cc.egui_ctx.clone());
            Ok(Box::new(TelegramApp::new(
                cfg,
                cmd_tx,
                evt_rx,
                data_dir.clone(),
                show_add,
                cc.egui_ctx.clone(),
            )))
        }),
    )
    .map_err(|e| format!("{e}").into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddStep {
    Form,
    Code,
    Password,
}

#[derive(Default)]
struct AddAccountDraft {
    name: String,
    phone: String,
    code: String,
    password: String,
    error: Option<String>,
    busy: bool,
    step: Option<AddStep>,
}

struct TelegramApp {
    cfg: Config,
    cmd_tx: UnboundedSender<Cmd>,
    evt_rx: std_mpsc::Receiver<Evt>,
    data_dir: PathBuf,
    ctx: egui::Context,
    /// Decoded inline images, keyed by `media_cache_key(account, peer_id, msg_id)`.
    textures: HashMap<String, egui::TextureHandle>,
    /// Images whose download or decode failed (don't spin forever).
    failed_media: std::collections::HashSet<String>,
    /// An image staged but not yet sent (composer attachment).
    pending_attachment: Option<PathBuf>,
    /// Full-screen image viewer, if open.
    viewer: Option<ViewerState>,
    /// Embedded video player (libmpv), if any.
    video: Option<VideoPlayer>,
    /// Set when the user tapped a video that is not yet downloaded; holds the
    /// cache_key we're waiting on.
    pending_video_open: Option<String>,
    show_picker: bool,
    picker_dir: PathBuf,
    picker_entries: Vec<(String, PathBuf, bool, u64)>,
    picker_err: Option<String>,
    picker_show_all: bool,
    sel_account: Option<String>,
    status: HashMap<String, ConnStatus>,
    me: HashMap<String, MeInfo>,
    dialogs: HashMap<String, Vec<DialogItem>>,
    filter: String,
    sel_peer: HashMap<String, PeerId>,
    chats: HashMap<(String, PeerId), Vec<MsgItem>>,
    unread: HashMap<(String, PeerId), u32>,
    composer: String,
    toast: Option<(String, Instant)>,
    show_settings: bool,
    show_add: bool,
    add: AddAccountDraft,
    api_id_draft: String,
    api_hash_draft: String,
    confirm_remove: Option<String>,
    confirm_logout: Option<String>,
    last_auto_refresh: HashMap<String, Instant>,
}

impl TelegramApp {
    fn new(
        cfg: Config,
        cmd_tx: UnboundedSender<Cmd>,
        evt_rx: std_mpsc::Receiver<Evt>,
        data_dir: PathBuf,
        show_add: bool,
        ctx: egui::Context,
    ) -> Self {
        let api_id_draft = cfg.api_id.map(|id| id.to_string()).unwrap_or_default();
        let api_hash_draft = cfg.api_hash.clone().unwrap_or_default();
        let mut app = Self {
            cfg,
            cmd_tx,
            evt_rx,
            data_dir,
            sel_account: None,
            status: HashMap::new(),
            me: HashMap::new(),
            dialogs: HashMap::new(),
            filter: String::new(),
            sel_peer: HashMap::new(),
            chats: HashMap::new(),
            unread: HashMap::new(),
            composer: String::new(),
            toast: None,
            show_settings: false,
            show_add,
            add: AddAccountDraft::default(),
            api_id_draft,
            api_hash_draft,
            confirm_remove: None,
            confirm_logout: None,
            last_auto_refresh: HashMap::new(),
            ctx,
            textures: HashMap::new(),
            failed_media: std::collections::HashSet::new(),
            pending_attachment: None,
            viewer: None,
            video: None,
            pending_video_open: None,
            show_picker: false,
            picker_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            picker_entries: Vec::new(),
            picker_err: None,
            picker_show_all: false,
        };
        for account in app.cfg.accounts.clone() {
            app.status.insert(account.name.clone(), ConnStatus::Connecting);
        }
        app.sel_account = app.cfg.accounts.first().map(|a| a.name.clone());
        app
    }

    fn apply_event(&mut self, evt: Evt) {
        match evt {
            Evt::Config(cfg) => {
                let known: std::collections::HashSet<String> =
                    cfg.accounts.iter().map(|a| a.name.clone()).collect();
                self.status.retain(|k, _| known.contains(k));
                self.me.retain(|k, _| known.contains(k));
                self.dialogs.retain(|k, _| known.contains(k));
                self.sel_peer.retain(|k, _| known.contains(k));
                self.last_auto_refresh.retain(|k, _| known.contains(k));
                self.chats.retain(|(a, _), _| known.contains(a));
                self.unread.retain(|(a, _), _| known.contains(a));
                if let Some(sel) = &self.sel_account {
                    if !known.contains(sel) {
                        self.sel_account = None;
                    }
                }
                for account in &cfg.accounts {
                    self.status.entry(account.name.clone()).or_insert(ConnStatus::Connecting);
                }
                self.cfg = cfg;
            }
            Evt::Status { name, status } => {
                self.status.insert(name, status);
            }
            Evt::Me { name, me } => {
                self.me.insert(name, me);
            }
            Evt::Dialogs { name, items } => {
                self.dialogs.insert(name, items);
            }
            Evt::History { account, peer_id, messages } => {
                let entry = self.chats.entry((account, peer_id)).or_default();
                *entry = messages;
                if entry.len() > MAX_CACHED_MESSAGES {
                    let excess = entry.len() - MAX_CACHED_MESSAGES;
                    entry.drain(..excess);
                }
            }
            Evt::Incoming { account, peer_id, msg } => {
                let is_open = self.sel_account.as_deref() == Some(account.as_str())
                    && self.sel_peer.get(&account) == Some(&peer_id);
                if let Some(chat) = self.chats.get_mut(&(account.clone(), peer_id)) {
                    chat.push(msg);
                    if chat.len() > MAX_CACHED_MESSAGES {
                        let excess = chat.len() - MAX_CACHED_MESSAGES;
                        chat.drain(..excess);
                    }
                }
                if !is_open {
                    *self.unread.entry((account.clone(), peer_id)).or_insert(0) += 1;
                }
                // Unknown peer? Refresh the dialog list (throttled).
                let known = self
                    .dialogs
                    .get(&account)
                    .map(|d| d.iter().any(|d| d.peer_id == peer_id))
                    .unwrap_or(true);
                if !known {
                    let stale = self
                        .last_auto_refresh
                        .get(&account)
                        .map(|t| t.elapsed() > Duration::from_secs(10))
                        .unwrap_or(true);
                    if stale {
                        self.last_auto_refresh.insert(account.clone(), Instant::now());
                        let _ = self.cmd_tx.send(Cmd::RefreshDialogs { account });
                    }
                }
            }
            Evt::Sent { account, peer_id, result, source_path } => match result {
                Ok(msg) => {
                    // A photo we just sent: cache the local file for rendering.
                    if let Some(src) = source_path {
                        let key = media_cache_key(&account, peer_id, msg.id);
                        let _ = self.decode_into_textures(&key, &src);
                        // Clear the staged attachment only if it is the one we sent.
                        if self.pending_attachment.as_deref() == Some(src.as_path()) {
                            self.pending_attachment = None;
                        }
                    }
                    if let Some(chat) = self.chats.get_mut(&(account.clone(), peer_id)) {
                        chat.push(msg);
                        if chat.len() > MAX_CACHED_MESSAGES {
                            let excess = chat.len() - MAX_CACHED_MESSAGES;
                            chat.drain(..excess);
                        }
                    }
                    // If the chat was not open the user will re-fetch history on next open.
                }
                Err(e) => self.toast = Some((format!("send failed: {e}"), Instant::now())),
            },
            Evt::StashMedia { cache_key, media } => {
                // Echo back to the worker; it owns the raw Media handles.
                self.send_cmd(Cmd::StashMedia { cache_key, media });
            }
            Evt::MediaReady { cache_key, path, kind, .. } => {
                match kind {
                    MsgMedia::Video(_) => {
                        if self.pending_video_open.as_deref() == Some(cache_key.as_str()) {
                            self.pending_video_open = None;
                            if path.as_os_str().is_empty() {
                                self.toast = Some(("video download failed".into(), Instant::now()));
                            } else {
                                let title = path
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "video".to_string());
                                match VideoPlayer::open(&path, &title) {
                                    Ok(player) => self.video = Some(player),
                                    Err(e) => {
                                        self.toast = Some((format!("video failed: {e}"), Instant::now()))
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if path.as_os_str().is_empty() {
                            self.failed_media.insert(cache_key);
                            return;
                        }
                        self.decode_into_textures(&cache_key, &path);
                    }
                }
            }
            Evt::Login { name, state } => match state {
                LoginEvt::NeedCode => {
                    if self.add.name == name {
                        self.add.step = Some(AddStep::Code);
                        self.add.busy = false;
                        self.add.error = None;
                        self.add.code.clear();
                    } else {
                        self.toast = Some((format!("[{name}] login code requested"), Instant::now()));
                    }
                }
                LoginEvt::NeedPassword => {
                    if self.add.name == name {
                        self.add.step = Some(AddStep::Password);
                        self.add.busy = false;
                        self.add.error = None;
                        self.add.password.clear();
                    }
                }
                LoginEvt::Done => {
                    if self.add.name == name {
                        self.toast = Some((format!("account '{name}' is online"), Instant::now()));
                        self.show_add = false;
                        self.add = AddAccountDraft::default();
                    }
                }
                LoginEvt::Failed(msg) => {
                    if self.add.name == name || self.show_add {
                        self.add.error = Some(msg);
                        self.add.busy = false;
                    } else {
                        self.toast = Some((msg, Instant::now()));
                    }
                }
            },
            Evt::Toast(msg) => self.toast = Some((msg, Instant::now())),
        }
    }

    fn send_cmd(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Decode an image file into the texture cache (no-op if already cached).
    fn decode_into_textures(&mut self, key: &str, path: &std::path::Path) -> bool {
        if self.textures.contains_key(key) {
            return true;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                self.failed_media.insert(key.to_string());
                return false;
            }
        };
        let img = match image::load_from_memory(&bytes) {
            Ok(i) => i,
            Err(_) => {
                self.failed_media.insert(key.to_string());
                return false;
            }
        };
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
        let handle = self
            .ctx
            .load_texture(key.to_string(), image, egui::TextureOptions::default());
        self.textures.insert(key.to_string(), handle);
        true
    }

    fn select_account(&mut self, name: String) {
        self.sel_account = Some(name.clone());
        self.pending_attachment = None;
        // Lazily load dialogs the first time the account is picked.
        let online = self.status.get(&name) == Some(&ConnStatus::Online);
        if online && self.dialogs.get(&name).map(|d| d.is_empty()).unwrap_or(true) {
            self.send_cmd(Cmd::RefreshDialogs { account: name });
        }
    }

    fn select_peer(&mut self, peer_id: PeerId) {
        if let Some(account) = self.sel_account.clone() {
            self.sel_peer.insert(account.clone(), peer_id);
            self.pending_attachment = None;
            self.unread.remove(&(account.clone(), peer_id));
            if !self.chats.contains_key(&(account.clone(), peer_id)) {
                self.send_cmd(Cmd::OpenChat { account, peer_id });
            }
        }
    }

    fn selected_peer_name(&self, account: &str, peer_id: PeerId) -> String {
        self.dialogs
            .get(account)
            .and_then(|d| d.iter().find(|d| d.peer_id == peer_id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "chat".to_string())
    }
}

fn is_image_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff")
    )
}

fn premium_flag(t: &PremiumToggles, key: &str) -> bool {
    match key {
        "themes" => t.themes,
        "fast_animations" => t.fast_animations,
        "sticker_shortcuts" => t.sticker_shortcuts,
        "hide_sponsored" => t.hide_sponsored,
        "hidden_premium_nags" => t.hidden_premium_nags,
        _ => false,
    }
}

fn premium_set_local(t: &mut PremiumToggles, key: &str, on: bool) {
    match key {
        "themes" => t.themes = on,
        "fast_animations" => t.fast_animations = on,
        "sticker_shortcuts" => t.sticker_shortcuts = on,
        "hide_sponsored" => t.hide_sponsored = on,
        "hidden_premium_nags" => t.hidden_premium_nags = on,
        _ => {}
    }
}

fn status_color(status: &ConnStatus) -> egui::Color32 {
    match status {
        ConnStatus::Online => egui::Color32::from_rgb(52, 211, 103),
        ConnStatus::Connecting => egui::Color32::from_rgb(255, 176, 46),
        ConnStatus::Unauthorized => egui::Color32::from_rgb(154, 154, 154),
        ConnStatus::Error(_) => egui::Color32::from_rgb(255, 95, 86),
    }
}

fn status_tooltip(status: &ConnStatus) -> String {
    match status {
        ConnStatus::Online => "connected — this account stays active".into(),
        ConnStatus::Connecting => "connecting…".into(),
        ConnStatus::Unauthorized => "session not authorized — remove and re-add the account".into(),
        ConnStatus::Error(e) => format!("error: {e}"),
    }
}

fn avatar_fill(name: &str) -> egui::Color32 {
    const PALETTE: &[egui::Color32] = &[
        egui::Color32::from_rgb(88, 101, 242),
        egui::Color32::from_rgb(235, 69, 158),
        egui::Color32::from_rgb(0, 168, 132),
        egui::Color32::from_rgb(255, 126, 27),
        egui::Color32::from_rgb(83, 189, 235),
        egui::Color32::from_rgb(255, 99, 71),
        egui::Color32::from_rgb(156, 89, 209),
        egui::Color32::from_rgb(31, 158, 137),
    ];
    let mut hash: u64 = 1469598103934665603;
    for b in name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    PALETTE[(hash % PALETTE.len() as u64) as usize]
}

fn initials(name: &str) -> String {
    let mut it = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase);
    let s: String = it.by_ref().collect();
    if s.is_empty() { "?".into() } else { s }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn format_msg_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local))
        .map(|dt| {
            let today = chrono::Local::now().date_naive();
            if dt.date_naive() == today {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%b %d %H:%M").to_string()
            }
        })
        .unwrap_or_else(|| "?".to_string())
}

// ---------------- eframe::App implementation ----------------

impl eframe::App for TelegramApp {
    /// Background logic: drain worker events. No painting here.
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(evt) = self.evt_rx.try_recv() {
            self.apply_event(evt);
        }
        if let Some((_, since)) = &self.toast {
            if since.elapsed() > Duration::from_secs(6) {
                self.toast = None;
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ESC closes the top-most thing: viewer → picker → settings/add → chat.
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if esc {
            if self.viewer.take().is_some() {
                // viewer closed
            } else if self.video.take().is_some() {
                // video player closed
            } else if self.show_picker {
                self.show_picker = false;
            } else if self.show_settings {
                self.show_settings = false;
            } else if self.show_add {
                self.show_add = false;
            } else if let Some(account) = self.sel_account.clone() {
                // "close the chat" — return to the "pick a chat" screen.
                self.sel_peer.remove(&account);
                self.pending_attachment = None;
            }
        }

        // Stage an image pasted into the app (Ctrl/Cmd+V with no usable text
        // on the clipboard — egui-winit emits Event::PasteImage then).
        let pasted: Vec<egui::ColorImage> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::PasteImage(image) => Some((**image).clone()),
                    _ => None,
                })
                .collect()
        });
        for image in pasted {
            match self.stage_pasted_image(&image) {
                Ok(()) => {
                    self.toast = Some(("image pasted — press Enter to send".into(), Instant::now()))
                }
                Err(e) => self.toast = Some((format!("paste failed: {e}"), Instant::now())),
            }
        }

        // Stage an image dropped from the OS onto the window.
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect()
        });
        if let Some(path) = dropped.into_iter().find(|p| is_image_file(p)) {
            let chat_open = self
                .sel_account
                .as_ref()
                .map(|a| self.sel_peer.get(a).is_some())
                .unwrap_or(false);
            if chat_open {
                self.pending_attachment = Some(path);
            } else {
                self.toast = Some(("open a chat first".into(), Instant::now()));
            }
        }

        self.ui_topbar(ui);

        egui::Panel::left("account_list")
            .default_size(232.0)
            .min_size(180.0)
            .resizable(true)
            .show(ui, |ui| self.ui_accounts(ui));

        egui::Panel::left("chat_list")
            .default_size(296.0)
            .min_size(220.0)
            .resizable(true)
            .show(ui, |ui| self.ui_chats(ui));

        egui::CentralPanel::default().show(ui, |ui| self.ui_chat_view(ui));

        if self.video.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        self.ui_settings_window(ui.ctx());
        self.ui_add_window(ui.ctx());
        self.ui_picker_window(ui.ctx());
        self.ui_viewer(ui.ctx());
        self.ui_video(ui.ctx());
        self.ui_toast(ui.ctx());
    }
}

impl TelegramApp {
    fn ui_topbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("topbar")
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("mlxysftele").strong().size(16.0));
                    ui.label(egui::RichText::new("ZapFast-style · native multi-account Telegram").weak().small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Settings").clicked() {
                            self.show_settings = true;
                        }
                        if ui.button("+ Add account").clicked() {
                            self.show_add = true;
                        }
                        let total = self.cfg.accounts.len();
                        let online = self
                            .cfg
                            .accounts
                            .iter()
                            .filter(|a| self.status.get(&a.name) == Some(&ConnStatus::Online))
                            .count();
                        let color = if online > 0 {
                            egui::Color32::from_rgb(52, 211, 103)
                        } else {
                            egui::Color32::from_rgb(255, 176, 46)
                        };
                        ui.label(
                            egui::RichText::new(format!("{online}/{total} accounts online"))
                                .color(color)
                                .strong(),
                        )
                        .on_hover_text("all registered accounts stay connected while mlxysftele runs");
                        if self.cfg.local_premium.enabled {
                            ui.label(egui::RichText::new("Local premium").weak().small())
                                .on_hover_text(
                                    "client-side conveniences only — server-enforced Telegram Premium is never faked (docs/LOCAL_PREMIUM.md)",
                                );
                        }
                    });
                });
            });
    }

    fn ui_accounts(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("Accounts ({})", self.cfg.accounts.len())).strong().size(13.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if ui.button("+").on_hover_text("Add account").clicked() {
                    self.show_add = true;
                }
            });
        });
        ui.add_space(4.0);
        let mut clicked_name: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for account in self.cfg.accounts.clone() {
                let name = account.name.clone();
                let selected = self.sel_account.as_deref() == Some(name.as_str());
                let status = self.status.get(&name).cloned().unwrap_or(ConnStatus::Connecting);
                let me = self.me.get(&name).cloned();
                let phone = account.phone.clone();
                let text = ui.visuals().text_color();
                let response = painted_row(ui, selected, 54.0, |p, rect| {
                    let dot = egui::pos2(rect.left() + 15.0, rect.center().y);
                    p.circle_filled(dot, 5.0, status_color(&status));
                    let tx = dot.x + 14.0;
                    p.text(
                        egui::pos2(tx, rect.center().y - 9.0),
                        egui::Align2::LEFT_CENTER,
                        truncate(&name, 16),
                        egui::FontId::proportional(13.5),
                        text,
                    );
                    let caption = me
                        .as_ref()
                        .and_then(|m| m.username.as_ref().map(|u| format!("@{u}")))
                        .or_else(|| phone.clone())
                        .unwrap_or_else(|| "—".into());
                    p.text(
                        egui::pos2(tx, rect.center().y + 9.0),
                        egui::Align2::LEFT_CENTER,
                        truncate(&caption, 22),
                        egui::FontId::proportional(11.0),
                        egui::Color32::from_gray(150),
                    );
                    if me.as_ref().map(|m| m.server_premium).unwrap_or(false) {
                        p.text(
                            egui::pos2(rect.right() - 12.0, rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            "PRO",
                            egui::FontId::proportional(10.0),
                            egui::Color32::from_rgb(232, 184, 78),
                        );
                    }
                });
                let hover = match &me {
                    Some(m) => format!(
                        "{}\n{}\nid {}{}",
                        m.full_name,
                        name,
                        m.user_id,
                        if m.server_premium { " · server premium" } else { "" }
                    ),
                    None => name.clone(),
                };
                // on_hover_text consumes the response; read the click state first.
                let clicked = response.clicked();
                response.on_hover_text(format!("{hover}\n{}", status_tooltip(&status)));
                if clicked {
                    clicked_name = Some(name);
                }
            }
        });
        if let Some(name) = clicked_name {
            self.select_account(name);
        }
    }

    fn ui_chats(&mut self, ui: &mut egui::Ui) {
        let Some(account) = self.sel_account.clone() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No account selected").weak());
            });
            return;
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Chats").strong().size(13.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if ui.button("Refresh").clicked() {
                    self.last_auto_refresh.insert(account.clone(), Instant::now());
                    self.send_cmd(Cmd::RefreshDialogs { account: account.clone() });
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Search chats…")
                    .desired_width(ui.available_width() - 16.0),
            );
            ui.add_space(8.0);
        });
        ui.add_space(4.0);

        let filter = self.filter.trim().to_lowercase();
        let dialogs = self.dialogs.get(&account).cloned().unwrap_or_default();
        let empty = dialogs.is_empty();
        let mut clicked_peer: Option<PeerId> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for dialog in dialogs {
                if !filter.is_empty() && !dialog.name.to_lowercase().contains(&filter) {
                    continue;
                }
                let selected = self.sel_peer.get(&account) == Some(&dialog.peer_id);
                let unread = self.unread.get(&(account.clone(), dialog.peer_id)).copied().unwrap_or(0);
                let name = dialog.name.clone();
                let preview = dialog.preview.clone();
                let text = ui.visuals().text_color();
                let response = painted_row(ui, selected, 56.0, |p, rect| {
                    let center = egui::pos2(rect.left() + 26.0, rect.center().y);
                    paint_avatar(p, center, 16.0, &name);
                    let tx = center.x + 26.0;
                    p.text(
                        egui::pos2(tx, rect.center().y - 9.0),
                        egui::Align2::LEFT_CENTER,
                        truncate(&name, 22),
                        egui::FontId::proportional(13.0),
                        text,
                    );
                    p.text(
                        egui::pos2(tx, rect.center().y + 9.0),
                        egui::Align2::LEFT_CENTER,
                        preview
                            .map(|pv| truncate(&pv, 26))
                            .unwrap_or_else(|| "—".to_string()),
                        egui::FontId::proportional(10.5),
                        egui::Color32::from_gray(145),
                    );
                    if unread > 0 {
                        let digits = format!("{}", unread.min(99));
                        let w = 12.0 + 7.0 * digits.len() as f32;
                        let pill = egui::Rect::from_center_size(
                            egui::pos2(rect.right() - w / 2.0 - 10.0, rect.center().y),
                            egui::vec2(w, 17.0),
                        );
                        p.rect_filled(pill, 8.5, egui::Color32::from_rgb(255, 95, 86));
                        p.text(
                            pill.center(),
                            egui::Align2::CENTER_CENTER,
                            digits,
                            egui::FontId::proportional(10.5),
                            egui::Color32::WHITE,
                        );
                    }
                });
                if response.clicked() {
                    clicked_peer = Some(dialog.peer_id);
                }
            }
            if empty {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("No dialogs loaded yet.").weak());
                    ui.label(egui::RichText::new("Press Refresh if the account just came online.").weak().small());
                });
            }
        });
        if let Some(peer_id) = clicked_peer {
            self.select_peer(peer_id);
        }
    }

    fn ui_chat_view(&mut self, ui: &mut egui::Ui) {
        let Some(account) = self.sel_account.clone() else {
            self.ui_welcome(ui);
            return;
        };
        let Some(peer_id) = self.sel_peer.get(&account).copied() else {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Pick a chat on the left").size(18.0).weak());
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Incoming messages appear live in every account — they stay connected.").weak());
            });
            return;
        };
        let peer_name = self.selected_peer_name(&account, peer_id);

        // Header
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
            let p = ui.painter();
            paint_avatar(&p, rect.center(), 17.0, &peer_name);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(truncate(&peer_name, 48)).strong().size(16.0));
                ui.label(egui::RichText::new(format!("via {account}")).weak().small());
            });
        });
        ui.add_space(4.0);
        ui.separator();

        let has_attachment = self.pending_attachment.is_some();
        // Composer docked at the bottom; history fills the middle.
        egui::Panel::bottom("composer_dock")
            .show_separator_line(false)
            .exact_size(if has_attachment { 96.0 } else { 58.0 })
            .show(ui, |ui| {
                if self.pending_attachment.is_some() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        let name = self
                            .pending_attachment
                            .as_ref()
                            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                            .unwrap_or_else(|| "image".into());
                        egui::Frame::default()
                            .fill(ui.visuals().widgets.noninteractive.bg_fill)
                            .corner_radius(8.0)
                            .inner_margin(egui::Margin::symmetric(10, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(format!("Image: {}", truncate(&name, 44))).small());
                                    if ui.small_button("×").on_hover_text("remove attachment").clicked() {
                                        self.pending_attachment = None;
                                    }
                                });
                            });
                    });
                }
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    if ui
                        .button("Attach…")
                        .on_hover_text("Send an image — or just drop a file anywhere in the window")
                        .clicked()
                    {
                        self.open_picker();
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.composer)
                            .hint_text(format!("Message {peer_name}…"))
                            .desired_width(ui.available_width() - 178.0),
                    );
                    let enter = response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let can_send = !self.composer.trim().is_empty() || self.pending_attachment.is_some();
                    let send_clicked = ui
                        .add_enabled(
                            can_send,
                            egui::Button::new(egui::RichText::new("Send").color(egui::Color32::WHITE))
                                .fill(egui::Color32::from_rgb(46, 122, 214)),
                        )
                        .clicked();
                    if enter || send_clicked {
                        let text = self.composer.trim().to_string();
                        let attachment = self.pending_attachment.take();
                        self.composer.clear();
                        if let Some(path) = attachment {
                            self.send_cmd(Cmd::SendPhoto { account: account.clone(), peer_id, path, caption: text });
                        } else if !text.is_empty() {
                            self.send_cmd(Cmd::SendText { account: account.clone(), peer_id, text });
                        }
                    }
                    ui.add_space(6.0);
                });
            });

        let messages = self.chats.get(&(account.clone(), peer_id)).cloned().unwrap_or_default();
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                if messages.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(egui::RichText::new("No messages yet — say hi.").weak());
                    });
                }
                let text_color = ui.visuals().text_color();
                let mut last_day: Option<chrono::NaiveDate> = None;
                for message in &messages {
                    let day = chrono::DateTime::from_timestamp(message.unix, 0)
                        .map(|dt| dt.with_timezone(&chrono::Local).date_naive());
                    if day != last_day {
                        if let Some(d) = day {
                            day_chip(ui, &day_label(d));
                        }
                        last_day = day;
                    }
                    if message.outgoing {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(14.0);
                            self.ui_bubble(ui, message, true, text_color, &account, peer_id);
                        });
                    } else {
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add_space(14.0);
                            self.ui_bubble(ui, message, false, text_color, &account, peer_id);
                        });
                    }
                    ui.add_space(3.0);
                }
                ui.add_space(6.0);
            });
    }

    /// Render one chat bubble with readable typography and inline images.
    fn ui_bubble(
        &mut self,
        ui: &mut egui::Ui,
        message: &MsgItem,
        outgoing: bool,
        text_color: egui::Color32,
        account: &str,
        peer_id: PeerId,
    ) {
        let (fill, time_color) = if outgoing {
            (
                egui::Color32::from_rgb(47, 84, 132),
                egui::Color32::from_rgb(170, 196, 224),
            )
        } else {
            (
                ui.visuals().widgets.noninteractive.bg_fill,
                egui::Color32::from_gray(140),
            )
        };
        let stroke = if outgoing {
            egui::Stroke::NONE
        } else {
            egui::Stroke::new(1.0, egui::Color32::from_gray(72))
        };
        let mut open_viewer: Option<ViewerState> = None;
        let mut open_video: Option<(String, PeerId, i32)> = None;
        egui::Frame::default()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(14.0)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_max_width(360.0);
                    match &message.media {
                        Some(MsgMedia::Photo) => {
                            let key = media_cache_key(account, peer_id, message.id);
                            if let Some(tex) = self.textures.get(&key) {
                                let size = tex.size_vec2();
                                let scale = (340.0 / size.x).min(260.0 / size.y).min(1.0);
                                let response = ui.add(
                                    egui::Image::new(egui::load::SizedTexture::new(tex.id(), size * scale))
                                        .corner_radius(8.0)
                                        .sense(egui::Sense::click()),
                                );
                                let response = response.on_hover_text("click to open full-size viewer");
                                if response.clicked() {
                                    open_viewer = Some(ViewerState {
                                        key: key.clone(),
                                        zoom: 1.0,
                                        pan: egui::Vec2::ZERO,
                                    });
                                }
                            } else if self.failed_media.contains(&key) {
                                ui.label(
                                    egui::RichText::new("[image unavailable]")
                                        .italics()
                                        .weak()
                                        .small(),
                                );
                            } else {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(egui::RichText::new("loading image…").weak().small());
                                });
                            }
                        }
                        Some(MsgMedia::Video(label)) => {
                            let key = media_cache_key(account, peer_id, message.id);
                            let busy = self.pending_video_open.as_deref() == Some(key.as_str());
                            if busy {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(egui::RichText::new("downloading video…").weak().small());
                                });
                            } else {
                                let card = egui::Button::new(
                                    egui::RichText::new(format!("▶  {label}")).size(13.0),
                                )
                                .min_size(egui::vec2(240.0, 64.0));
                                let resp = ui
                                    .add(card)
                                    .on_hover_text("download & play inline");
                                if resp.clicked() {
                                    self.pending_video_open = Some(key.clone());
                                    open_video = Some((account.to_string(), peer_id, message.id));
                                }
                            }
                        }
                        Some(MsgMedia::Label(label)) => {
                            ui.label(
                                egui::RichText::new(format!("[{label}]"))
                                    .italics()
                                    .color(time_color)
                                    .size(12.5),
                            );
                        }
                        None => {}
                    }
                    if !message.text.is_empty() {
                        let color = if outgoing { egui::Color32::WHITE } else { text_color };
                        ui.label(egui::RichText::new(&message.text).color(color).size(13.5));
                    }
                    ui.label(
                        egui::RichText::new(format_msg_time(message.unix))
                            .small()
                            .color(time_color),
                    );
                });
            });
        if let Some(viewer) = open_viewer {
            self.viewer = Some(viewer);
        }
        if let Some((account, peer_id, msg_id)) = open_video {
            self.send_cmd(Cmd::OpenVideo { account, peer_id, msg_id });
        }
    }

    /// Write a pasted clipboard image to `<data_dir>/cache/paste/` and stage it.
    fn stage_pasted_image(&mut self, image: &egui::ColorImage) -> Result<(), String> {
        let chat_open = self
            .sel_account
            .as_ref()
            .map(|a| self.sel_peer.get(a).is_some())
            .unwrap_or(false);
        if !chat_open {
            return Err("open a chat first".to_string());
        }
        let dir = self.data_dir.join("cache").join("paste");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let bytes: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|c| c.to_srgba_unmultiplied())
            .collect();
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("pasted_{millis}.png"));
        image::save_buffer(
            &path,
            &bytes,
            image.width() as u32,
            image.height() as u32,
            image::ColorType::Rgba8,
        )
        .map_err(|e| e.to_string())?;
        self.pending_attachment = Some(path);
        Ok(())
    }

    /// ZapFast-style full-screen image viewer: Esc/× closes, scroll zooms,
    /// drag pans, double-click toggles fit ↔ 100%.
    fn ui_viewer(&mut self, ctx: &egui::Context) {
        let Some(mut viewer) = self.viewer.take() else { return };
        // Texture gone (never decoded or cache cleared) → drop the viewer.
        let Some(tex) = self.textures.get(&viewer.key) else { return };
        let tex_size = tex.size_vec2();
        let tex_id = tex.id();

        let screen = ctx.input(|i| i.content_rect());
        let fit = (screen.width() * 0.90 / tex_size.x.max(1.0))
            .min(screen.height() * 0.86 / tex_size.y.max(1.0))
            .min(1.0);

        let mut close = false;
        let mut toggle = false;

        egui::Area::new(egui::Id::new("image_viewer"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.left_top())
            .show(ctx, |ui| {
                let response = ui.allocate_rect(screen, egui::Sense::click_and_drag());
                ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(225));

                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll.abs() > 0.0 && response.hovered() {
                    viewer.zoom = (viewer.zoom * (1.0 + scroll * 0.0016)).clamp(0.35, 12.0);
                }
                if response.dragged() {
                    viewer.pan += ui.input(|i| i.pointer.delta());
                }
                if response.double_clicked() {
                    toggle = true;
                }

                let scale = fit * viewer.zoom;
                let size = tex_size * scale;
                let center = screen.center() + viewer.pan;
                let rect = egui::Rect::from_center_size(center, size);
                ui.painter().image(
                    tex_id,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

            });

        // Hint bar pinned to the bottom-center of the screen.
        egui::Area::new(egui::Id::new("image_viewer_hint"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -40.0))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new("Esc close · scroll zoom · drag pan · double-click 1:1 / fit")
                        .small()
                        .color(egui::Color32::from_gray(190)),
                );
            });

        // HUD buttons pinned to the top-right of the screen.
        egui::Area::new(egui::Id::new("image_viewer_hud"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("−").clicked() {
                        viewer.zoom = (viewer.zoom / 1.25).clamp(0.35, 12.0);
                    }
                    if ui.button("+").clicked() {
                        viewer.zoom = (viewer.zoom * 1.25).clamp(0.35, 12.0);
                    }
                    if ui.button("1:1").on_hover_text("actual pixels").clicked() {
                        viewer.zoom = 1.0 / fit;
                        viewer.pan = egui::Vec2::ZERO;
                    }
                    if ui.button("Fit").clicked() {
                        viewer.zoom = 1.0;
                        viewer.pan = egui::Vec2::ZERO;
                    }
                    if ui.button("×").on_hover_text("close (Esc)").clicked() {
                        close = true;
                    }
                });
            });

        if toggle {
            viewer.zoom = if (viewer.zoom - 1.0).abs() < 0.01 { 1.0 / fit } else { 1.0 };
            if viewer.zoom <= 1.0 {
                viewer.pan = egui::Vec2::ZERO;
            }
        }
        if !close {
            self.viewer = Some(viewer);
        }
    }

    /// Full-screen embedded video player (libmpv): Esc/× closes, the picture
    /// scales to fit, and the bottom bar carries play/pause, ±10 s skip, a
    /// seek slider and a volume slider.
    fn ui_video(&mut self, ctx: &egui::Context) {
        // Take so we can drop it inside this call if it went bad.
        let Some(mut player) = self.video.take() else { return };
        player.refresh_frame(ctx);

        let screen = ctx.input(|i| i.content_rect());
        let tex_id = player.texture_id();
        let tex_size = player.size_vec2().unwrap_or(egui::vec2(640.0, 360.0));
        let title = if player.title.is_empty() {
            player.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into())
        } else {
            player.title.clone()
        };

        let mut close = false;
        let mut toggle_pause = false;
        egui::Area::new(egui::Id::new("video_overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.left_top())
            .show(ctx, |ui| {
                let response = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(235));

                // Fit the frame, preserving aspect.
                let fit = (screen.width() * 0.92 / tex_size.x.max(1.0))
                    .min(screen.height() * 0.78 / tex_size.y.max(1.0))
                    .min(1.0);
                let size = tex_size * fit;
                let video_rect = egui::Rect::from_center_size(
                    egui::pos2(screen.center().x, screen.min.y + screen.height() * 0.42),
                    size,
                );
                if let Some(tex_id) = tex_id {
                    ui.painter().image(
                        tex_id,
                        video_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                } else {
                    ui.painter().text(
                        video_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "loading video…",
                        egui::FontId::proportional(16.0),
                        egui::Color32::from_gray(200),
                    );
                }
                // Click on the picture/backdrop toggles play/pause.
                if response.clicked() {
                    toggle_pause = true;
                }
            });
        if toggle_pause {
            let paused = player.is_paused();
            player.set_paused(!paused);
        }

        // Top bar: title left, close right.
        egui::Area::new(egui::Id::new("video_topbar"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&title)
                            .small()
                            .color(egui::Color32::from_gray(210)),
                    );
                    if ui.button("×").on_hover_text("close (Esc)").clicked() {
                        close = true;
                    }
                });
            });

        // Bottom control bar.
        egui::Area::new(egui::Id::new("video_controls"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let paused = player.is_paused();
                    let eof = player.eof_reached();
                    if eof {
                        if ui.button("↻").on_hover_text("replay").clicked() {
                            player.replay();
                        }
                    } else if ui.button(if paused { "▶" } else { "❚❚" }).on_hover_text(if paused { "play" } else { "pause" }).clicked() {
                        player.set_paused(!paused);
                    }
                    if ui.button("−10s").clicked() {
                        player.skip_relative(-10.0);
                    }
                    if ui.button("+10s").clicked() {
                        player.skip_relative(10.0);
                    }

                    let pos = player.time_pos().unwrap_or(0.0);
                    let dur = player.duration().unwrap_or(0.0).max(1.0);
                    ui.label(egui::RichText::new(format_mmss(pos)).small().monospace());
                    let mut slider = pos.clamp(0.0, dur);
                    let resp = ui.add(
                        egui::Slider::new(&mut slider, 0.0..=dur).show_value(false)
                    );
                    if resp.drag_stopped() {
                        player.seek_to(slider);
                    }
                    ui.label(egui::RichText::new(format_mmss(dur)).small().monospace());

                    ui.separator();
                    ui.label(egui::RichText::new("🔊").small());
                    let mut volume = player.volume().unwrap_or(100.0);
                    if ui
                        .add(egui::Slider::new(&mut volume, 0.0..=100.0).show_value(false))
                        .changed()
                    {
                        player.set_volume(volume);
                    }
                });
            });

        if !player.last_error().is_empty() {
            self.toast = Some((format!("video: {}", player.last_error()), Instant::now()));
        }

        if !close {
            self.video = Some(player);
        }
        // When closing, the VideoPlayer is dropped here → mpv shuts down.
    }

    /// Open the in-app image picker for the composer.
    fn open_picker(&mut self) {
        self.show_picker = true;
        self.refresh_picker();
    }

    fn refresh_picker(&mut self) {
        self.picker_entries.clear();
        self.picker_err = None;
        let read = std::fs::read_dir(&self.picker_dir);
        match read {
            Ok(entries) => {
                let mut items: Vec<(String, PathBuf, bool, u64)> = Vec::new();
                for e in entries.flatten() {
                    let path = e.path();
                    let is_dir = path.is_dir();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !is_dir && !self.picker_show_all && !is_image_file(&path) {
                        continue;
                    }
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    items.push((name, path, is_dir, size));
                }
                items.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
                self.picker_entries = items;
            }
            Err(e) => self.picker_err = Some(format!("{}: {e}", self.picker_dir.display())),
        }
    }

    fn ui_picker_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_picker;
        egui::Window::new("Send image")
            .open(&mut open)
            .resizable(true)
            .default_width(540.0)
            .default_height(440.0)
            .show(ctx, |ui| {
                let can_attach = self
                    .sel_account
                    .as_ref()
                    .map(|a| self.sel_peer.get(a).is_some())
                    .unwrap_or(false);
                ui.label(egui::RichText::new(self.picker_dir.display().to_string()).weak().small());
                ui.horizontal(|ui| {
                    if ui.button("..").on_hover_text("parent directory").clicked() {
                        if let Some(parent) = self.picker_dir.parent().map(|p| p.to_path_buf()) {
                            self.picker_dir = parent;
                            self.refresh_picker();
                        }
                    }
                    if ui.button("~").on_hover_text("home directory").clicked() {
                        if let Some(home) = std::env::var_os("HOME") {
                            self.picker_dir = PathBuf::from(home);
                            self.refresh_picker();
                        }
                    }
                    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                        for label in ["Pictures", "Downloads", "Desktop"] {
                            let candidate = home.join(label);
                            if candidate.is_dir() && ui.button(label).clicked() {
                                self.picker_dir = candidate;
                                self.refresh_picker();
                            }
                        }
                    }
                    let mut show_all = self.picker_show_all;
                    if ui.checkbox(&mut show_all, "show all files").changed() {
                        self.picker_show_all = show_all;
                        self.refresh_picker();
                    }
                });
                if let Some(err) = &self.picker_err {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 110), err.as_str());
                }
                ui.separator();
                if !can_attach {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 176, 46),
                        "open a chat first, then pick an image",
                    );
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let entries = self.picker_entries.clone();
                    if entries.is_empty() {
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new("no images here").weak());
                            ui.label(egui::RichText::new("tick 'show all files' or navigate elsewhere").weak().small());
                        });
                    }
                    for (name, path, is_dir, size) in entries {
                        if is_dir {
                            if ui.selectable_label(false, format!("{name}/")).clicked() {
                                self.picker_dir = path;
                                self.refresh_picker();
                            }
                        } else {
                            let label = format!("{name}  ·  {}", util::human_bytes(size));
                            if ui.selectable_label(false, label).clicked() {
                                if can_attach {
                                    self.pending_attachment = Some(path);
                                    self.show_picker = false;
                                } else {
                                    self.toast = Some(("open a chat first".into(), Instant::now()));
                                }
                            }
                        }
                    }
                });
            });
        self.show_picker = open;
    }

    fn ui_welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(70.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("mlxysftele").strong().size(30.0));
            ui.add_space(4.0);
            ui.label(egui::RichText::new("ZapFast-style, ultra low-RAM Telegram for every account you own").weak());
            ui.add_space(28.0);
            ui.label("• Native Rust + egui — no browser, no Electron, no webview");
            ui.label("• Unlimited accounts, all of them connected at the same time");
            ui.label("• Streaming media, honest local-only premium conveniences");
            ui.add_space(28.0);
            if ui.button(egui::RichText::new("Add your first account").size(16.0)).clicked() {
                self.show_add = true;
            }
            ui.add_space(8.0);
            ui.label(egui::RichText::new("You will need api_id/api_hash from https://my.telegram.org/apps (Settings).").weak().small());
        });
    }
}

// ---------------- shared paint helpers ----------------

fn painted_row(
    ui: &mut egui::Ui,
    selected: bool,
    height: f32,
    contents: impl FnOnce(&egui::Painter, egui::Rect),
) -> egui::Response {
    let width = ui.available_width().max(0.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let painter = ui.painter_at(rect);
    let fill = if selected {
        Some(ui.visuals().selection.bg_fill)
    } else if response.hovered() {
        Some(ui.visuals().widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    if let Some(fill) = fill {
        painter.rect_filled(rect.shrink2(egui::vec2(4.0, 2.0)), 8.0, fill);
    }
    contents(&painter, rect);
    response
}

fn paint_avatar(painter: &egui::Painter, center: egui::Pos2, radius: f32, name: &str) {
    painter.circle_filled(center, radius, avatar_fill(name));
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        initials(name),
        egui::FontId::proportional(radius),
        egui::Color32::WHITE,
    );
}

fn day_label(d: chrono::NaiveDate) -> String {
    let today = chrono::Local::now().date_naive();
    if d == today {
        return "Today".into();
    }
    if let Some(yesterday) = today.pred_opt() {
        if d == yesterday {
            return "Yesterday".into();
        }
    }
    d.format("%d %B %Y").to_string()
}

fn day_chip(ui: &mut egui::Ui, label: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let approx = 36.0 + label.chars().count() as f32 * 6.2;
        ui.add_space(((ui.available_width() - approx) / 2.0).max(0.0));
        egui::Frame::default()
            .fill(egui::Color32::from_gray(50))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 3))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(label).small().color(egui::Color32::from_gray(208)));
            });
    });
    ui.add_space(2.0);
}


// ---------------- windows & toast ----------------

impl TelegramApp {
    fn ui_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.heading("Telegram API credentials");
                ui.label(
                    egui::RichText::new("Create your own at https://my.telegram.org/apps — one pair works for all accounts.")
                        .weak()
                        .small(),
                );
                ui.add_space(4.0);
                egui::Grid::new("creds_grid").num_columns(2).show(ui, |ui| {
                    ui.label("api_id");
                    ui.text_edit_singleline(&mut self.api_id_draft);
                    ui.end_row();
                    ui.label("api_hash");
                    ui.add(egui::TextEdit::singleline(&mut self.api_hash_draft).password(true));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button("Save credentials").clicked() {
                        match self.api_id_draft.trim().parse::<i32>() {
                            Ok(api_id) => {
                                let api_hash = self.api_hash_draft.trim().to_string();
                                if api_hash.is_empty() {
                                    self.toast = Some(("api_hash must not be empty".into(), Instant::now()));
                                } else {
                                    self.send_cmd(Cmd::SaveCreds { api_id, api_hash });
                                }
                            }
                            Err(_) => {
                                self.toast = Some(("api_id must be an integer".into(), Instant::now()));
                            }
                        }
                    }
                    ui.label(
                        egui::RichText::new(format!("stored in {}", Config::path(&self.data_dir).display()))
                            .weak()
                            .small(),
                    );
                });

                ui.separator();
                ui.heading("Local premium (client-side only)");
                ui.label(
                    egui::RichText::new(
                        "These toggles only unlock conveniences inside this client. Server-enforced Telegram Premium (upload limits, faster DCs, reactions…) is never faked — see docs/LOCAL_PREMIUM.md.",
                    )
                    .weak()
                    .small(),
                );
                let mut master = self.cfg.local_premium.enabled;
                if ui.checkbox(&mut master, "Enable local premium").changed() {
                    self.cfg.local_premium.set_all(master);
                    self.send_cmd(Cmd::SetLocalPremium { flag: None, on: master });
                }
                ui.add_enabled_ui(self.cfg.local_premium.enabled, |ui| {
                    for doc in premium::TOGGLE_DOCS {
                        let mut value = premium_flag(&self.cfg.local_premium, doc.key);
                        if ui
                            .checkbox(&mut value, doc.label)
                            .on_hover_text(doc.detail)
                            .changed()
                        {
                            premium_set_local(&mut self.cfg.local_premium, doc.key, value);
                            self.send_cmd(Cmd::SetLocalPremium { flag: Some(doc.key.to_string()), on: value });
                        }
                    }
                });

                ui.separator();
                ui.heading("Accounts");
                if self.cfg.accounts.is_empty() {
                    ui.label(egui::RichText::new("no accounts registered yet").weak());
                }
                for account in self.cfg.accounts.clone() {
                    ui.horizontal(|ui| {
                        let name = account.name.clone();
                        ui.label(egui::RichText::new(&name).strong());
                        let caption = account
                            .username
                            .as_ref()
                            .map(|u| format!("@{u}"))
                            .or_else(|| account.phone.clone())
                            .unwrap_or_default();
                        ui.label(egui::RichText::new(caption).weak().small());

                        let confirm_logout = self.confirm_logout.as_deref() == Some(name.as_str());
                        if ui
                            .button(if confirm_logout { "Confirm sign out?" } else { "Sign out" })
                            .clicked()
                        {
                            if confirm_logout {
                                self.send_cmd(Cmd::Logout { name: name.clone() });
                                self.confirm_logout = None;
                            } else {
                                self.confirm_logout = Some(name.clone());
                            }
                        }

                        let confirm_remove = self.confirm_remove.as_deref() == Some(name.as_str());
                        if ui
                            .button(if confirm_remove { "Confirm remove?" } else { "Remove" })
                            .clicked()
                        {
                            if confirm_remove {
                                self.send_cmd(Cmd::RemoveLocal { name: name.clone() });
                                self.confirm_remove = None;
                            } else {
                                self.confirm_remove = Some(name.clone());
                            }
                        }
                    });
                }

                ui.separator();
                ui.label(
                    egui::RichText::new(format!("data dir: {}", self.data_dir.display())).weak().small(),
                );
                ui.label(egui::RichText::new("sessions stay on disk, never in config.json; they are owner-only files").weak().small());
            });
        self.show_settings = open;
    }

    fn ui_add_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_add;
        egui::Window::new("Add account")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                let step = self.add.step.unwrap_or(AddStep::Form);
                match step {
                    AddStep::Form => {
                        ui.label("Register another Telegram account. Add as many as you like — they all stay connected.");
                        ui.add_space(6.0);
                        egui::Grid::new("add_grid").num_columns(2).show(ui, |ui| {
                            ui.label("handle");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.add.name)
                                    .hint_text("work"),
                            );
                            ui.end_row();
                            ui.label("phone");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.add.phone)
                                    .hint_text("+6591234567"),
                            );
                            ui.end_row();
                        });
                        ui.label(
                            egui::RichText::new("handle: a-z 0-9 _ - . (max 64 chars); phone with country code")
                                .weak()
                                .small(),
                        );
                        if let Some(err) = &self.add.error {
                            ui.colored_label(egui::Color32::from_rgb(255, 120, 110), err.as_str());
                        }
                        ui.horizontal(|ui| {
                            if self.add.busy {
                                ui.spinner();
                                ui.label("requesting code…");
                            } else if ui.button("Send code").clicked() {
                                self.submit_add_form();
                            }
                        });
                    }
                    AddStep::Code => {
                        ui.label(format!("A login code was sent to {}.", self.add.phone.trim()));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("code");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.add.code)
                                    .hint_text("12345")
                                    .desired_width(120.0),
                            );
                        });
                        if let Some(err) = &self.add.error {
                            ui.colored_label(egui::Color32::from_rgb(255, 120, 110), err.as_str());
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() {
                                self.add.step = Some(AddStep::Form);
                                self.add.error = None;
                            }
                            if self.add.busy {
                                ui.spinner();
                            } else if ui.button("Sign in").clicked() {
                                let name = self.add.name.trim().to_string();
                                let code = self.add.code.trim().to_string();
                                if code.is_empty() {
                                    self.add.error = Some("enter the code you received".into());
                                } else {
                                    self.add.busy = true;
                                    self.add.error = None;
                                    self.send_cmd(Cmd::SubmitCode { name, code });
                                }
                            }
                        });
                    }
                    AddStep::Password => {
                        ui.label("This account has two-factor authentication enabled.");
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.add.password)
                                    .password(true)
                                    .desired_width(220.0),
                            );
                        });
                        if let Some(err) = &self.add.error {
                            ui.colored_label(egui::Color32::from_rgb(255, 120, 110), err.as_str());
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() {
                                self.add.step = Some(AddStep::Code);
                                self.add.error = None;
                            }
                            if self.add.busy {
                                ui.spinner();
                            } else if ui.button("Verify").clicked() {
                                let name = self.add.name.trim().to_string();
                                let password = self.add.password.clone();
                                if password.is_empty() {
                                    self.add.error = Some("enter your 2FA password".into());
                                } else {
                                    self.add.busy = true;
                                    self.add.error = None;
                                    self.send_cmd(Cmd::SubmitPassword { name, password });
                                }
                            }
                        });
                    }
                }
            });
        self.show_add = open;
    }

    fn submit_add_form(&mut self) {
        let name = self.add.name.trim().to_string();
        let phone = self.add.phone.trim().to_string();
        if !util::valid_account_name(&name) {
            self.add.error = Some("invalid handle (allowed: a-z A-Z 0-9 _ - ., max 64 chars)".into());
            return;
        }
        if self.cfg.accounts.iter().any(|a| a.name == name) {
            self.add.error = Some(format!("account '{name}' already exists"));
            return;
        }
        if phone.is_empty() {
            self.add.error = Some("phone number is required (with country code)".into());
            return;
        }
        self.add.busy = true;
        self.add.error = None;
        self.send_cmd(Cmd::BeginLogin { name, phone });
    }

    fn ui_toast(&mut self, ctx: &egui::Context) {
        if let Some((msg, _)) = &self.toast {
            egui::Area::new(egui::Id::new("toast"))
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -64.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::default()
                        .fill(egui::Color32::from_rgb(38, 42, 50))
                        .inner_margin(egui::Margin::symmetric(14, 9))
                        .corner_radius(8.0)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(msg.as_str()).color(egui::Color32::WHITE));
                        });
                });
        }
    }
}
