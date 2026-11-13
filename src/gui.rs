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

#[derive(Clone)]
struct MsgItem {
    id: i32,
    outgoing: bool,
    unix: i64,
    text: String,
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
    Sent { account: String, peer_id: PeerId, result: Result<MsgItem, String> },
    Login { name: String, state: LoginEvt },
    Toast(String),
}

// ---------------------------------------------------------------------------
// Worker side (background tokio thread). Owns the Config so the GUI never
// writes config.json behind its back — single writer, no races.
// ---------------------------------------------------------------------------

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
        let update_client = client.clone();
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
                            let msg = MsgItem {
                                id: message.id(),
                                outgoing: message.outgoing(),
                                unix: message.date().timestamp(),
                                text: message.text().to_string(),
                            };
                            let _ = evt_tx.send(Evt::Incoming {
                                account: account.clone(),
                                peer_id: message.peer_id(),
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
        while let Ok(Some(message)) = iter.next().await {
            messages.push(MsgItem {
                id: message.id(),
                outgoing: message.outgoing(),
                unix: message.date().timestamp(),
                text: message.text().to_string(),
            });
        }
        messages.reverse(); // newest first from the iterator; show oldest on top
        self.emit(Evt::History { account: account.to_string(), peer_id, messages });
    }

    async fn send_text(&mut self, account: &str, peer_id: PeerId, text: String) {
        let Some(conn) = self.conns.get(account) else {
            self.emit(Evt::Sent {
                account: account.to_string(),
                peer_id,
                result: Err("account is not connected".into()),
            });
            return;
        };
        let Some(&peer_ref) = conn.peer_refs.get(&peer_id) else {
            self.emit(Evt::Sent {
                account: account.to_string(),
                peer_id,
                result: Err("peer not resolved — refresh the chat list".into()),
            });
            return;
        };
        let result = match conn.client.send_message(peer_ref, text.as_str()).await {
            Ok(message) => Ok(MsgItem {
                id: message.id(),
                outgoing: true,
                unix: message.date().timestamp(),
                text,
            }),
            Err(e) => Err(e.to_string()),
        };
        self.emit(Evt::Sent { account: account.to_string(), peer_id, result });
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
            Err(SignInError::InvalidPassword(_)) => {
                self.pending_logins.insert(
                    name.clone(),
                    PendingLogin::AwaitingPassword { client, pool, phone, token },
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
            let worker = Worker { data_dir, cfg, evt_tx, ctx, conns: HashMap::new(), pending_logins: HashMap::new() };
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
            Ok(Box::new(TelegramApp::new(cfg, cmd_tx, evt_rx, data_dir.clone(), show_add)))
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
            Evt::Sent { account, peer_id, result } => match result {
                Ok(msg) => {
                    if let Some(chat) = self.chats.get_mut(&(account.clone(), peer_id)) {
                        chat.push(msg);
                        if chat.len() > MAX_CACHED_MESSAGES {
                            let excess = chat.len() - MAX_CACHED_MESSAGES;
                            chat.drain(..excess);
                        }
                    }
                }
                Err(e) => self.toast = Some((format!("send failed: {e}"), Instant::now())),
            },
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

    fn select_account(&mut self, name: String) {
        self.sel_account = Some(name.clone());
        // Lazily load dialogs the first time the account is picked.
        let online = self.status.get(&name) == Some(&ConnStatus::Online);
        if online && self.dialogs.get(&name).map(|d| d.is_empty()).unwrap_or(true) {
            self.send_cmd(Cmd::RefreshDialogs { account: name });
        }
    }

    fn select_peer(&mut self, peer_id: PeerId) {
        if let Some(account) = self.sel_account.clone() {
            self.sel_peer.insert(account.clone(), peer_id);
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

        self.ui_settings_window(ui.ctx());
        self.ui_add_window(ui.ctx());
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
                response.on_hover_text(format!("{hover}\n{}", status_tooltip(&status)));
                if response.clicked() {
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
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::hover());
            let p = ui.painter();
            paint_avatar(&p, rect.center(), 15.0, &peer_name);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(truncate(&peer_name, 40)).strong().size(15.0));
                ui.label(egui::RichText::new(format!("via {account}")).weak().small());
            });
        });
        ui.separator();

        // Composer docked at the bottom; history fills the middle.
        egui::Panel::bottom("composer_dock")
            .show_separator_line(false)
            .exact_size(56.0)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.composer)
                            .hint_text(format!("Message {peer_name}…"))
                            .desired_width(ui.available_width() - 74.0),
                    );
                    let enter = response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let send_clicked = ui
                        .add_enabled(!self.composer.trim().is_empty(), egui::Button::new("Send"))
                        .clicked();
                    if (enter || send_clicked) && !self.composer.trim().is_empty() {
                        let text = self.composer.trim().to_string();
                        self.composer.clear();
                        self.send_cmd(Cmd::SendText { account: account.clone(), peer_id, text });
                        response.request_focus();
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
                for message in &messages {
                    let outgoing = message.outgoing;
                    if outgoing {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(12.0);
                            bubble(ui, message, true, ui.visuals().text_color());
                        });
                    } else {
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add_space(12.0);
                            bubble(ui, message, false, ui.visuals().text_color());
                        });
                    }
                    ui.add_space(2.0);
                }
                ui.add_space(6.0);
            });
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

fn bubble(ui: &mut egui::Ui, message: &MsgItem, outgoing: bool, text_color: egui::Color32) {
    let fill = if outgoing {
        egui::Color32::from_rgb(46, 92, 160)
    } else {
        ui.visuals().widgets.noninteractive.bg_fill
    };
    let time_color = if outgoing {
        egui::Color32::from_rgb(198, 214, 236)
    } else {
        egui::Color32::from_gray(140)
    };
    egui::Frame::default()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(10, 7))
        .corner_radius(12.0)
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_max_width(330.0);
                let color = if outgoing { egui::Color32::WHITE } else { text_color };
                ui.label(egui::RichText::new(&message.text).color(color));
                ui.label(
                    egui::RichText::new(format_msg_time(message.unix))
                        .small()
                        .color(time_color),
                );
            });
        });
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
