//! `mlxysftele daemon` — a headless multi-account hub.
//!
//! This is the feature that makes "unlimited accounts" practical:
//!
//! * Any number of accounts can be *registered*; each dormant one costs a
//!   JSON line plus a sqlite file on disk and **zero RAM**.
//! * At most `--max-hot` connections stay live (default 4). A connection is
//!   opened on first use and closed again after `--idle-ttl` seconds unused.
//! * Updates from hot accounts stream to stdout, one line each, so the daemon
//!   doubles as a poor-man's push gateway.
//! * Memory is bounded by design: no message cache, no media cache, no peer
//!   cache beyond what Telegram hands us during a session.
//!
//! Stdin commands (one per line):
//!   help                          show commands
//!   accounts                      list registered accounts
//!   status                        hot set + memory usage
//!   send <account> <peer> <text>  send a message
//!   whoami <account>              print identity
//!   dialogs <account> [limit]     list dialogs
//!   premium on|off|status         toggle local premium conveniences
//!   quit | exit                   graceful shutdown

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use grammers_client::client::UpdatesConfiguration;
use grammers_client::Client;
use tokio::task::JoinHandle;

use crate::config::{render_account_table, Config};
use crate::lowmem;
use crate::telegram::{self, BoxError, Connection};
use crate::util;
use crate::{log_err, log_info};

/// Live connection bookkeeping.
struct HotAccount {
    client: Client,
    pool: JoinHandle<()>,
    pump: JoinHandle<()>,
    last_used: Instant,
}

pub async fn run(
    data_dir: &Path,
    cfg_path_hint: &Path,
    max_hot: usize,
    idle_ttl: Duration,
) -> Result<(), BoxError> {
    let mut cfg = Config::load(data_dir)
        .map_err(|e| format!("failed to load {}: {e}", cfg_path_hint.display()))?;
    let (api_id, _) = telegram::require_creds(&cfg)?;

    println!("mlxysftele daemon — data dir {}", data_dir.display());
    println!(
        "{} account(s) registered · max {} live connection(s) · idle ttl {}s",
        cfg.accounts.len(),
        max_hot,
        idle_ttl.as_secs()
    );
    println!("type 'help' for commands, 'quit' to exit");

    let mut hot: HashMap<String, HotAccount> = HashMap::new();

    // stdin lines are read on a blocking thread and forwarded over an mpsc.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<Option<String>>(32);
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            match lock.read_line(&mut line) {
                Ok(0) => {
                    let _ = line_tx.blocking_send(None); // EOF
                    break;
                }
                Ok(_) => {
                    let text = line.trim_end_matches(['\n', '\r']).to_string();
                    if line_tx.blocking_send(Some(text)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = line_tx.blocking_send(None);
                    break;
                }
            }
        }
    });

    let mut reaper = tokio::time::interval(Duration::from_secs(2));
    reaper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = reaper.tick() => {
                reap_idle(&mut hot, idle_ttl).await;
            }
            line = line_rx.recv() => {
                let Some(line) = line else { break };
                match line {
                    Some(text) => {
                        let keep_going = handle_line(
                            &text, data_dir, &mut cfg, api_id, &mut hot, max_hot,
                        ).await;
                        if !keep_going {
                            break;
                        }
                    }
                    None => break, // EOF on stdin
                }
            }
        }
    }

    // Graceful shutdown: close every hot connection, persist nothing else.
    let names: Vec<String> = hot.keys().cloned().collect();
    for name in names {
        if let Some(account) = hot.remove(&name) {
            account.pump.abort();
            telegram::shutdown(account.client, account.pool).await;
        }
    }
    println!("daemon stopped");
    Ok(())
}

/// Borrow a hot connection, opening it lazily if necessary.
async fn hot_get<'h>(
    hot: &'h mut HashMap<String, HotAccount>,
    cfg: &Config,
    data_dir: &Path,
    api_id: i32,
    name: &str,
    max_hot: usize,
) -> Result<&'h mut HotAccount, BoxError> {
    if !hot.contains_key(name) {
        if cfg.find_account(name).is_none() {
            return Err(format!(
                "unknown account '{name}' — registered: {}",
                cfg.accounts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
            )
            .into());
        }
        // Evict the least-recently-used account if we are at capacity.
        while hot.len() >= max_hot {
            let victim_key = hot
                .iter()
                .min_by_key(|(_, h)| h.last_used)
                .map(|(k, _)| k.clone());
            if let Some(lru) = victim_key {
                if let Some(victim) = hot.remove(&lru) {
                    log_info!("[{lru}] evicted (idle, at {max_hot} hot limit)");
                    victim.pump.abort();
                    telegram::shutdown(victim.client, victim.pool).await;
                }
            } else {
                break;
            }
        }

        let Connection {
            client,
            pool,
            updates,
        } = telegram::connect(data_dir, api_id, name, true).await?;
        if !client.is_authorized().await? {
            telegram::shutdown(client, pool).await;
            return Err(format!(
                "account '{name}' is not authorized — run `mlxysftele login {name}` first"
            )
            .into());
        }
        let updates_rx = updates.expect("want_updates=true");
        let hide_sponsored = cfg.local_premium.hide_sponsored_active();
        let pump_name = name.to_string();
        let pump_client = client.clone();
        let pump = tokio::spawn(async move {
            let mut stream = pump_client
                .stream_updates(
                    updates_rx,
                    UpdatesConfiguration {
                        catch_up: false,
                        ..Default::default()
                    },
                )
                .await;
            loop {
                match stream.next().await {
                    Ok(update) => telegram::print_update(&pump_name, &update, hide_sponsored),
                    Err(e) => {
                        log_err!("[{pump_name}] update stream ended: {e}");
                        break;
                    }
                }
            }
        });
        log_info!("[{name}] connection opened ({} hot)", hot.len() + 1);
        hot.insert(
            name.to_string(),
            HotAccount {
                client,
                pool,
                pump,
                last_used: Instant::now(),
            },
        );
    }
    let entry = hot.get_mut(name).expect("just inserted");
    entry.last_used = Instant::now();
    Ok(entry)
}

/// Evict connections that have been idle longer than `ttl`.
async fn reap_idle(hot: &mut HashMap<String, HotAccount>, ttl: Duration) {
    let now = Instant::now();
    let stale: Vec<String> = hot
        .iter()
        .filter(|(_, h)| now.duration_since(h.last_used) > ttl)
        .map(|(k, _)| k.clone())
        .collect();
    for name in stale {
        if let Some(victim) = hot.remove(&name) {
            log_info!("[{name}] closed after idle timeout");
            victim.pump.abort();
            telegram::shutdown(victim.client, victim.pool).await;
        }
    }
}

/// Handle one stdin command. Returns false when the daemon should stop.
async fn handle_line(
    line: &str,
    data_dir: &Path,
    cfg: &mut Config,
    api_id: i32,
    hot: &mut HashMap<String, HotAccount>,
    max_hot: usize,
) -> bool {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return true;
    };
    match cmd {
        "help" => {
            println!(
                "commands:\n\
                 \x20 accounts                      list registered accounts\n\
                 \x20 status                        hot set + memory usage\n\
                 \x20 send <account> <peer> <text>  send a message\n\
                 \x20 whoami <account>              identity + premium status\n\
                 \x20 dialogs <account> [limit]     list dialogs\n\
                 \x20 premium on|off|status         toggle local premium conveniences\n\
                 \x20 quit                          graceful shutdown"
            );
        }
        "accounts" => {
            print!("{}", render_account_table(cfg));
        }
        "status" => {
            println!("registered: {} account(s)", cfg.accounts.len());
            if hot.is_empty() {
                println!("hot set:    (empty)");
            } else {
                println!("hot set:");
                for (name, account) in hot.iter() {
                    println!("  {name:<20} idle {:?}", account.last_used.elapsed());
                }
            }
            match lowmem::sample() {
                Some(s) => println!(
                    "memory:     rss {} · vm {} · threads {}",
                    util::human_bytes(s.rss_kb * 1024),
                    util::human_bytes(s.vm_kb * 1024),
                    s.threads
                ),
                None => println!("memory:     /proc/self/status unavailable on this platform"),
            }
        }
        "send" => {
            let Some(name) = parts.next() else {
                eprintln!("usage: send <account> <peer> <text…>");
                return true;
            };
            let Some(peer) = parts.next() else {
                eprintln!("usage: send <account> <peer> <text…>");
                return true;
            };
            let text: Vec<&str> = parts.collect();
            if text.is_empty() {
                eprintln!("usage: send <account> <peer> <text…>");
                return true;
            }
            let text = text.join(" ");
            match hot_get(hot, cfg, data_dir, api_id, name, max_hot).await {
                Ok(hot_account) => {
                    match telegram::resolve_peer_ref(&hot_account.client, peer).await {
                        Ok(peer_ref) => {
                            match hot_account.client.send_message(peer_ref, text).await {
                                Ok(msg) => println!("[{name}] sent (id {})", msg.id()),
                                Err(e) => eprintln!("[{name}] send failed: {e}"),
                            }
                        }
                        Err(e) => eprintln!("[{name}] {e}"),
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
        }
        "whoami" => {
            let Some(name) = parts.next() else {
                eprintln!("usage: whoami <account>");
                return true;
            };
            match hot_get(hot, cfg, data_dir, api_id, name, max_hot).await {
                Ok(hot_account) => match hot_account.client.get_me().await {
                    Ok(me) => println!(
                        "[{name}] {} (id {}{}) — server premium: {}",
                        me.full_name(),
                        me.id().bot_api_dialog_id(),
                        me.username().map(|u| format!(", @{u}")).unwrap_or_default(),
                        if me.is_premium() { "yes" } else { "no" },
                    ),
                    Err(e) => eprintln!("[{name}] {e}"),
                },
                Err(e) => eprintln!("{e}"),
            }
        }
        "dialogs" => {
            let Some(name) = parts.next() else {
                eprintln!("usage: dialogs <account> [limit]");
                return true;
            };
            let limit: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(20);
            match hot_get(hot, cfg, data_dir, api_id, name, max_hot).await {
                Ok(hot_account) => {
                    let mut iter = hot_account.client.iter_dialogs();
                    let mut count = 0usize;
                    while count < limit {
                        match iter.next().await {
                            Ok(Some(dialog)) => {
                                let label = dialog.peer().name().unwrap_or("?").to_string();
                                println!("{:>4}  {:<44} {}", count, label, dialog.peer_id());
                                count += 1;
                            }
                            Ok(None) => break,
                            Err(e) => {
                                eprintln!("[{name}] {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
        }
        "premium" => {
            match parts.next().unwrap_or("status") {
                "on" => {
                    cfg.local_premium.set_all(true);
                    let _ = cfg.save(data_dir);
                    println!("local premium: ON (client-side conveniences only; see `premium info`)");
                }
                "off" => {
                    cfg.local_premium.set_all(false);
                    let _ = cfg.save(data_dir);
                    println!("local premium: OFF");
                }
                _ => print!("{}", cfg.local_premium.render()),
            }
        }
        "quit" | "exit" => return false,
        other => {
            eprintln!("unknown command '{other}' — try 'help'");
        }
    }
    true
}
