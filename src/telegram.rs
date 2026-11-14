//! Core MTProto engine: connection management and high-level account actions.
//!
//! Design rules that keep RAM flat no matter how many accounts are registered:
//!
//! * **Lazy connections.** Registering an account costs one sqlite file and
//!   one small JSON entry (~200 bytes). No socket, no crypto context, until
//!   you connect on purpose.
//! * **One connection at a time for one-shot commands.** `send`, `dialogs`,
//!   `whoami`, `download` open a connection, do the work, and tear it down.
//! * **Streamed downloads.** Media flows from the DC straight to disk; file
//!   bodies are never buffered in memory.
//! * **Bounded hot set for the daemon** (see daemon.rs): at most `--max-hot`
//!   live connections, idle ones are evicted and disconnected.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::UpdatesConfiguration;
use grammers_client::peer::User;
use grammers_client::update::Update;
use grammers_client::{Client, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerRef;
use grammers_session::updates::UpdatesLike;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::{AccountMeta, Config};
use crate::util;
use crate::{log_debug, log_err, log_info, log_warn};

pub type BoxError = Box<dyn Error + Send + Sync>;

/// A wired client plus the pool task driving its socket.
pub struct Connection {
    pub client: Client,
    pub(crate) pool: JoinHandle<()>,
    /// Present only when the caller asked for the update stream.
    pub updates: Option<mpsc::UnboundedReceiver<UpdatesLike>>,
}

/// Open (or create) the sqlite session for `name` and connect to Telegram.
///
/// `want_updates = false` gives a cheap RPC-only connection; the mtsender
/// pool's update channel is drained in a background task so the sender never
/// blocks on a full buffer.
pub async fn connect(
    data_dir: &Path,
    api_id: i32,
    name: &str,
    want_updates: bool,
) -> Result<Connection, BoxError> {
    let path = util::session_path(data_dir, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let session = Arc::new(SqliteSession::open(&path).await?);
    // Sessions are bearer credentials — keep them owner-only.
    let _ = util::secure_file(&path);
    let SenderPool {
        runner,
        updates,
        handle,
    } = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(handle);
    let pool = tokio::spawn(async move {
        let _ = runner.run().await;
    });

    if want_updates {
        Ok(Connection {
            client,
            pool,
            updates: Some(updates),
        })
    } else {
        // Drain and discard updates so the pool never applies backpressure
        // to a client that only does RPCs.
        tokio::spawn(async move {
            let mut updates = updates;
            while updates.recv().await.is_some() {}
        });
        Ok(Connection {
            client,
            pool,
            updates: None,
        })
    }
}

/// Ask the pool to quit and stop the socket. Bounded wait, then we abort.
pub async fn shutdown(client: Client, pool: JoinHandle<()>) {
    client.disconnect();
    let mut pool = pool;
    tokio::select! {
        _ = &mut pool => {}
        _ = tokio::time::sleep(Duration::from_secs(3)) => {
            // Detach-and-abandon is not an option; stop the task instead.
            pool.abort();
        }
    }
}

/// Effective (api_id, api_hash) or a helpful error.
pub fn require_creds(cfg: &Config) -> Result<(i32, String), BoxError> {
    cfg.effective_api_creds().ok_or_else(|| {
        "missing Telegram API credentials.\n\
         Create yours at https://my.telegram.org/apps, then either:\n\
         \x20 mlxysftele init --api-id=123456 --api-hash=abcdef0123456789abcdef0123456789\n\
         \x20 or export MLXYSFTELE_API_ID and MLXYSFTELE_API_HASH"
            .to_string()
            .into()
    })
}

/// Resolve "me"/"self"/"@username" into a PeerRef. Raw numeric ids are
/// intentionally not guessed: Telegram keys them to access hashes cached
/// from prior interactions, and we will not fabricate those.
pub async fn resolve_peer_ref(client: &Client, s: &str) -> Result<PeerRef, BoxError> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("me") || trimmed.eq_ignore_ascii_case("self") {
        let me = client.get_me().await?;
        return me
            .to_ref()
            .await
            .ok_or_else(|| "could not resolve self peer reference".into());
    }
    let username = trimmed.strip_prefix('@').unwrap_or(trimmed);
    let looks_like_username = username
        .chars()
        .next()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false)
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if looks_like_username {
        return match client.resolve_username(username).await? {
            Some(peer) => peer
                .to_ref()
                .await
                .ok_or_else(|| format!("@{username}: no usable peer reference").into()),
            None => Err(format!("@{username} not found").into()),
        };
    }
    Err(format!(
        "unsupported peer '{trimmed}' — use @username or 'me'.\
         For chats without a username, open `mlxysftele dialogs <account>` and use a peer with a public username."
    )
    .into())
}

/// Interactive login flow: code request, sign-in, optional 2FA.
///
/// `name` is the local handle for the account; `phone` may be supplied to
/// skip the prompt. On success the registry in `config.json` is updated.
pub async fn login(
    data_dir: &Path,
    cfg: &mut Config,
    name: &str,
    phone: Option<String>,
) -> Result<(), BoxError> {
    if !util::valid_account_name(name) {
        return Err(format!(
            "invalid account name '{name}' (allowed: a-z A-Z 0-9 _ - ., max 64 chars)"
        )
        .into());
    }
    let (api_id, api_hash) = require_creds(cfg)?;

    // Preserve any previous session instead of silently mixing credentials.
    let session = util::session_path(data_dir, name);
    if session.exists() {
        let backup = session.with_extension(format!("sqlite.bak-{}", util::now_unix()));
        std::fs::rename(&session, &backup)?;
        log_info!("previous session moved to {}", backup.display());
    }

    let phone = match phone {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => util::read_line("phone (with country code, e.g. +6581234567): ").await?,
    };
    if phone.is_empty() {
        return Err("phone number is required".into());
    }

    let conn = connect(data_dir, api_id, name, false).await?;
    let client = conn.client;

    if client.is_authorized().await? {
        log_info!("account '{name}' is already authorized; refreshing registry");
        let user = client.get_me().await?;
        persist_user(data_dir, cfg, name, &phone, &user)?;
        shutdown(client, conn.pool).await;
        return Ok(());
    }

    println!("requesting login code for {phone}…");
    let token = client.request_login_code(&phone, &api_hash).await?;
    println!("code sent via Telegram. Check the login code in your Telegram app.");

    let mut signed_in: Option<User> = None;
    for attempt in 1..=3u8 {
        let code = util::read_line("login code: ").await?;
        match client.sign_in(&token, code.trim()).await {
            Ok(user) => {
                signed_in = Some(user);
                break;
            }
            Err(SignInError::PasswordRequired(password_token)) => {
                signed_in = Some(complete_2fa(&client, password_token).await?);
                break;
            }
            Err(SignInError::InvalidCode) => {
                if attempt == 3 {
                    // Keep the half-finished session; the user can retry.
                    return Err("too many invalid codes — aborting login".into());
                }
                eprintln!("invalid code, try again ({attempt}/3)");
            }
            Err(SignInError::SignUpRequired) => {
                return Err(
                    "this phone number has no Telegram account. \
                     Register first in an official Telegram app, then try again."
                        .into(),
                );
            }
            Err(SignInError::InvalidPassword(_)) => {
                return Err("unexpected 2FA state before password prompt".into());
            }
            Err(SignInError::Other(e)) => return Err(e.to_string().into()),
        }
    }

    let user = signed_in.ok_or("login did not complete")?;
    persist_user(data_dir, cfg, name, &phone, &user)?;
    shutdown(client, conn.pool).await;
    Ok(())
}

/// 2FA password loop with up to 3 attempts. Wrong passwords hand back a
/// fresh token, which we keep.
async fn complete_2fa(
    client: &Client,
    password_token: grammers_client::client::PasswordToken,
) -> Result<User, BoxError> {
    let mut token = password_token;
    for attempt in 1..=3u8 {
        let hint = token.hint().unwrap_or("no hint").to_string();
        let password =
            util::read_secret(&format!("2FA password (hint: {hint}): ")).await?;
        match client.check_password(token, password.as_bytes()).await {
            Ok(user) => return Ok(user),
            Err(SignInError::InvalidPassword(new_token)) => {
                if attempt == 3 {
                    return Err("too many wrong 2FA passwords".into());
                }
                eprintln!("wrong password, try again ({attempt}/3)");
                token = new_token;
            }
            Err(SignInError::Other(e)) => return Err(e.to_string().into()),
            Err(e) => return Err(format!("{e}").into()),
        }
    }
    unreachable!()
}

/// Persist user details into the registry and print a summary.
fn persist_user(
    data_dir: &Path,
    cfg: &mut Config,
    name: &str,
    phone: &str,
    user: &User,
) -> Result<(), BoxError> {
    let user_id = user.id().bot_api_dialog_id();
    let username = user.username().map(str::to_string);
    let full_name = user.full_name();
    let now = util::now_unix();
    let created = cfg
        .find_account(name)
        .map(|a| a.created_unix)
        .unwrap_or(now);
    cfg.upsert_account(AccountMeta {
        name: name.to_string(),
        phone: Some(phone.to_string()),
        user_id: Some(user_id),
        username: username.clone(),
        created_unix: created,
        last_login_unix: Some(now),
    });
    cfg.save(data_dir)?;
    println!();
    println!(
        "logged in as {full_name} (id {user_id}{})",
        username.map(|u| format!(", @{u}")).unwrap_or_default()
    );
    println!("session stored at {}", util::session_path(data_dir, name).display());
    println!("account '{name}' is ready. try: mlxysftele whoami {name}");
    Ok(())
}

/// `whoami` — show account identity + both premium axes.
pub async fn whoami(data_dir: &Path, cfg: &Config, name: &str) -> Result<(), BoxError> {
    let (api_id, _) = require_creds(cfg)?;
    let conn = connect(data_dir, api_id, name, false).await?;
    let client = conn.client;
    if !client.is_authorized().await? {
        return Err(format!(
            "account '{name}' is not authorized (session missing or revoked).\n\
             Run: mlxysftele login {name}"
        )
        .into());
    }
    let me = client.get_me().await?;
    println!("account:        {name}");
    println!("id:             {}", me.id().bot_api_dialog_id());
    println!("name:           {}", me.full_name());
    println!(
        "username:       {}",
        me.username().map(|u| format!("@{u}")).unwrap_or_else(|| "—".into())
    );
    println!("phone:          {}", me.phone().unwrap_or("—"));
    println!("bot:            {}", me.is_bot());
    println!("server premium: {}", if me.is_premium() { "yes" } else { "no" });
    println!(
        "local premium:  {}",
        if cfg.local_premium.enabled { "on" } else { "off" }
    );
    shutdown(client, conn.pool).await;
    Ok(())
}

/// `dialogs` — chat list, bounded by `limit`.
pub async fn dialogs(
    data_dir: &Path,
    cfg: &Config,
    name: &str,
    limit: usize,
) -> Result<(), BoxError> {
    let (api_id, _) = require_creds(cfg)?;
    let conn = connect(data_dir, api_id, name, false).await?;
    let client = conn.client;
    if !client.is_authorized().await? {
        return Err(format!("account '{name}' is not authorized").into());
    }
    let mut iter = client.iter_dialogs();
    let mut count = 0usize;
    while count < limit {
        match iter.next().await? {
            Some(dialog) => {
                let label = dialog.peer().name().unwrap_or("?").to_string();
                println!("{:>4}  {:<44} {}", count, label, dialog.peer_id());
                count += 1;
            }
            None => break,
        }
    }
    if count == 0 {
        println!("no dialogs (fresh account?)");
    } else {
        println!("-- {count} dialog(s)");
    }
    shutdown(client, conn.pool).await;
    Ok(())
}

/// `send` — text to a peer (`@username` or `me`).
pub async fn send(
    data_dir: &Path,
    cfg: &Config,
    name: &str,
    peer: &str,
    text: &str,
) -> Result<(), BoxError> {
    let (api_id, _) = require_creds(cfg)?;
    let conn = connect(data_dir, api_id, name, false).await?;
    let client = conn.client;
    if !client.is_authorized().await? {
        return Err(format!("account '{name}' is not authorized").into());
    }
    let peer_ref = resolve_peer_ref(&client, peer).await?;
    let message = client.send_message(peer_ref, text).await?;
    println!("sent (id {}) to {peer}", message.id());
    shutdown(client, conn.pool).await;
    Ok(())
}

/// Print one update line. Shared by `tail` and the daemon.
pub fn print_update(account: &str, update: &Update, hide_sponsored: bool) {
    match update {
        Update::NewMessage(message) => {
            let who = message
                .peer()
                .and_then(|p| p.name().map(str::to_string))
                .unwrap_or_else(|| message.peer_id().to_string());
            let direction = if message.outgoing() { "out" } else { "in " };
            println!("[{account}] {direction} {who}: {}", message.text());
        }
        Update::Raw(raw) => {
            if hide_sponsored && format!("{:?}", raw.raw).contains("Sponsored") {
                log_debug!("[{account}] dropped sponsored raw update (local premium)");
            } else {
                log_debug!("[{account}] raw update: {:?}", raw.raw);
            }
        }
        other => {
            log_debug!("[{account}] update: {other:?}");
        }
    }
}

/// `tail` — live update stream until Ctrl-C.
pub async fn tail(
    data_dir: &Path,
    cfg: &Config,
    name: &str,
    catch_up: bool,
) -> Result<(), BoxError> {
    let (api_id, _) = require_creds(cfg)?;
    let hide_sponsored = cfg.local_premium.hide_sponsored_active();
    let conn = connect(data_dir, api_id, name, true).await?;
    let client = conn.client;
    if !client.is_authorized().await? {
        return Err(format!("account '{name}' is not authorized").into());
    }
    let updates_rx = conn.updates.expect("want_updates=true");
    let mut updates = client
        .stream_updates(
            updates_rx,
            UpdatesConfiguration {
                catch_up,
                ..Default::default()
            },
        )
        .await;
    println!("[{name}] listening (Ctrl-C to stop)…");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n[{name}] stopping…");
                break;
            }
            res = updates.next() => {
                match res {
                    Ok(update) => print_update(name, &update, hide_sponsored),
                    Err(e) => {
                        log_err!("[{name}] update stream ended: {e}");
                        break;
                    }
                }
            }
        }
    }
    updates.sync_update_state().await;
    shutdown(client, conn.pool).await;
    Ok(())
}

/// `download` — media streamed straight to disk, never buffered in RAM.
pub async fn download(
    data_dir: &Path,
    cfg: &Config,
    name: &str,
    peer: &str,
    msg_id: i32,
    out: Option<PathBuf>,
) -> Result<(), BoxError> {
    let (api_id, _) = require_creds(cfg)?;
    let conn = connect(data_dir, api_id, name, false).await?;
    let client = conn.client;
    if !client.is_authorized().await? {
        return Err(format!("account '{name}' is not authorized").into());
    }
    let peer_ref = resolve_peer_ref(&client, peer).await?;
    let messages = client.get_messages_by_id(peer_ref, &[msg_id]).await?;
    let message = messages
        .into_iter()
        .next()
        .flatten()
        .ok_or("message not found (or no access)")?;
    let media = message
        .media()
        .ok_or("that message carries no downloadable media")?;
    let out_path = match out {
        Some(p) => p,
        None => {
            let dir = data_dir.join("downloads");
            std::fs::create_dir_all(&dir)?;
            dir.join(format!("{name}_{msg_id}.bin"))
        }
    };
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    client.download_media(&media, &out_path).await?;
    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    println!(
        "downloaded {} → {}",
        util::human_bytes(size),
        out_path.display()
    );
    shutdown(client, conn.pool).await;
    Ok(())
}

/// `logout` — server-side sign-out, then local cleanup (session + registry).
pub async fn logout(
    data_dir: &Path,
    cfg: &mut Config,
    name: &str,
    force: bool,
) -> Result<(), BoxError> {
    if !force {
        let confirm =
            util::read_line(&format!("type '{name}' to confirm logout & deletion: ")).await?;
        if confirm.trim() != name {
            return Err("aborted (name did not match)".into());
        }
    }
    if let Ok((api_id, _)) = require_creds(cfg) {
        match connect(data_dir, api_id, name, false).await {
            Ok(conn) => {
                let client = conn.client;
                match client.sign_out().await {
                    Ok(_) => println!("[{name}] signed out on Telegram"),
                    Err(e) => log_warn!("[{name}] sign_out RPC failed (continuing cleanup): {e}"),
                }
                shutdown(client, conn.pool).await;
            }
            Err(e) => log_warn!("[{name}] could not connect to sign out: {e}"),
        }
    }
    remove_local(data_dir, cfg, name)
}

/// `remove` — delete local session & registry entry only (no RPC, no sign-out).
pub fn remove_local(
    data_dir: &Path,
    cfg: &mut Config,
    name: &str,
) -> Result<(), BoxError> {
    let mut removed_files = 0usize;
    let session = util::session_path(data_dir, name);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", session.display()));
        if path.exists() {
            std::fs::remove_file(&path)?;
            removed_files += 1;
        }
    }
    let removed_registry = cfg.remove_account(name).is_some();
    cfg.save(data_dir)?;
    println!(
        "[{name}] local state removed ({removed_files} file(s){})",
        if removed_registry {
            ", registry entry"
        } else {
            ""
        }
    );
    if !removed_registry && removed_files == 0 {
        println!("(note: account '{name}' was not in the registry)");
    }
    Ok(())
}
