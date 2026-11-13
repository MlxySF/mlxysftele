//! mlxysftele — a ZapFast-style, ultra low-memory, multi-account Telegram
//! client with local (client-side) premium conveniences.
//!
//! Single-binary, current-thread async runtime, no GUI stack, no frameworks.
//! See README.md and docs/LOCAL_PREMIUM.md.

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod config;
mod daemon;
mod log;
mod lowmem;
mod premium;
mod telegram;
mod util;

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
mlxysftele — ZapFast-style low-RAM multi-account Telegram client

USAGE:
  mlxysftele <command> [options]

COMMANDS:
  init                   Store Telegram API credentials (https://my.telegram.org/apps)
                           --api-id=123456 --api-hash=hex
  login <name>           Register / sign in an account (interactive)
                           [--phone=+6581234567]
  accounts               List registered accounts
                           [--check]                (connect & verify auth)
  whoami <name>          Identity + server/local premium status
  dialogs <name>         List dialogs
                           [--limit=50]
  send <name> <peer> <text…>   Send a text message (peer: @username or 'me')
  tail <name>            Live message stream (Ctrl-C to stop)
                           [--no-catch-up]
  download <name> <peer> <msg_id> [--out=PATH]
                         Download media, streamed straight to disk
  daemon                 Headless multi-account hub (stdin commands)
                           [--max-hot=4] [--idle-ttl=120]
  premium                Local premium conveniences
                           premium status             (default)
                           premium on | premium off
                           premium set <flag> on|off
                           premium info               (full honesty notice)
  stats                  Process memory & on-disk footprint
  logout <name>          Sign out on Telegram + delete local session  [--force]
  remove <name>          Delete local session only (no RPC)
  help                   This text

Environment:
  MLXYSFTELE_HOME        Override data dir (default ~/.mlxysftele)
  MLXYSFTELE_API_ID      API credentials (override config.json)
  MLXYSFTELE_API_HASH
  MLXYSFTELE_LOG=debug   Verbose logs

Notes:
  • Unlimited accounts: register as many as you want; dormant ones cost ~0 RAM.
  • 'Local premium' unlocks CLIENT-SIDE conveniences only — see `premium info`.
";

/// Parsed arguments: `--key=value` options, bare `--flag` booleans, rest positional.
struct Cli {
    positional: Vec<String>,
    flags: HashMap<String, Option<String>>,
}

impl Cli {
    fn parse(argv: &[String]) -> Result<Cli, String> {
        let mut cli = Cli {
            positional: Vec::new(),
            flags: HashMap::new(),
        };
        for arg in argv {
            if let Some(rest) = arg.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    if k.is_empty() {
                        return Err(format!("malformed flag '--{rest}'"));
                    }
                    cli.flags.insert(k.to_string(), Some(v.to_string()));
                } else {
                    cli.flags.insert(rest.to_string(), None);
                }
            } else {
                cli.positional.push(arg.clone());
            }
        }
        Ok(cli)
    }

    fn flag(&self, key: &str) -> bool {
        self.flags.contains_key(key)
    }

    fn opt(&self, key: &str) -> Option<&str> {
        self.flags.get(key).and_then(|v| v.as_deref())
    }

    fn pos(&self, idx: usize) -> Option<&str> {
        self.positional.get(idx).map(String::as_str)
    }
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(dispatch())
}

async fn dispatch() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().cloned() else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    if matches!(cmd.as_str(), "help" | "--help" | "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let cli = match Cli::parse(&argv[1..]) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let data_dir = util::data_dir();
    let result = run_command(&cmd, &cli, &data_dir).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run_command(
    cmd: &str,
    cli: &Cli,
    data_dir: &std::path::Path,
) -> Result<(), telegram::BoxError> {
    match cmd {
        "init" => {
            let mut cfg = config::Config::load(data_dir)?;
            let api_id: Option<i32> = cli
                .opt("api-id")
                .map(|v| v.parse::<i32>())
                .transpose()
                .map_err(|_| "invalid --api-id (expected integer)")?;
            let api_hash = cli.opt("api-hash").map(str::to_string);
            if api_id.is_none() && api_hash.is_none() && !cli.flag("keep") {
                return Err("provide --api-id and/or --api-hash (see `mlxysftele help`)".into());
            }
            if let Some(id) = api_id {
                cfg.api_id = Some(id);
            }
            if let Some(hash) = api_hash {
                cfg.api_hash = Some(hash);
            }
            cfg.save(data_dir)?;
            println!("credentials stored in {}", config::Config::path(data_dir).display());
            println!("next: mlxysftele login <name>");
            Ok(())
        }
        "login" => {
            let name = required_pos(cli, 0, "login <name>")?.to_string();
            let mut cfg = config::Config::load(data_dir)?;
            let phone = cli.opt("phone").map(str::to_string);
            telegram::login(data_dir, &mut cfg, &name, phone).await
        }
        "accounts" => {
            let cfg = config::Config::load(data_dir)?;
            print!("{}", config::render_account_table(&cfg));
            if cli.flag("check") {
                let (api_id, _) = telegram::require_creds(&cfg)?;
                for account in cfg.accounts.iter() {
                    match telegram::connect(data_dir, api_id, &account.name, false).await {
                        Ok(conn) => {
                            let authorized = conn.client.is_authorized().await.unwrap_or(false);
                            println!(
                                "  [{}] {}",
                                account.name,
                                if authorized { "authorized" } else { "NOT AUTHORIZED" }
                            );
                            let client = conn.client;
                            let pool = conn.pool;
                            telegram::shutdown(client, pool).await;
                        }
                        Err(e) => println!("  [{}] error: {e}", account.name),
                    }
                }
            }
            Ok(())
        }
        "whoami" => {
            let name = required_pos(cli, 0, "whoami <name>")?.to_string();
            let cfg = config::Config::load(data_dir)?;
            telegram::whoami(data_dir, &cfg, &name).await
        }
        "dialogs" => {
            let name = required_pos(cli, 0, "dialogs <name>")?.to_string();
            let limit: usize = cli
                .opt("limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(50);
            let cfg = config::Config::load(data_dir)?;
            telegram::dialogs(data_dir, &cfg, &name, limit).await
        }
        "send" => {
            let name = required_pos(cli, 0, "send <name> <peer> <text…>")?.to_string();
            let peer = required_pos(cli, 1, "send <name> <peer> <text…>")?.to_string();
            let text_args: Vec<&str> = cli
                .positional
                .iter()
                .skip(2)
                .map(String::as_str)
                .collect();
            if text_args.is_empty() {
                return Err("missing message text".into());
            }
            let cfg = config::Config::load(data_dir)?;
            telegram::send(data_dir, &cfg, &name, &peer, &text_args.join(" ")).await
        }
        "tail" => {
            let name = required_pos(cli, 0, "tail <name>")?.to_string();
            let catch_up = !cli.flag("no-catch-up");
            let cfg = config::Config::load(data_dir)?;
            telegram::tail(data_dir, &cfg, &name, catch_up).await
        }
        "download" => {
            let name = required_pos(cli, 0, "download <name> <peer> <msg_id>")?.to_string();
            let peer = required_pos(cli, 1, "download <name> <peer> <msg_id>")?.to_string();
            let msg_id: i32 = required_pos(cli, 2, "download <name> <peer> <msg_id>")?
                .parse()
                .map_err(|_| "msg_id must be an integer")?;
            let out = cli.opt("out").map(std::path::PathBuf::from);
            let cfg = config::Config::load(data_dir)?;
            telegram::download(data_dir, &cfg, &name, &peer, msg_id, out).await
        }
        "daemon" => {
            let max_hot: usize = cli
                .opt("max-hot")
                .and_then(|v| v.parse().ok())
                .unwrap_or(4)
                .clamp(1, 256);
            let idle_ttl = Duration::from_secs(
                cli.opt("idle-ttl")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(120)
                    .clamp(5, 86_400),
            );
            let cfg_path = config::Config::path(data_dir);
            daemon::run(data_dir, &cfg_path, max_hot, idle_ttl).await
        }
        "premium" => {
            let mut cfg = config::Config::load(data_dir)?;
            match cli.pos(0).unwrap_or("status") {
                "status" => {
                    print!("{}", cfg.local_premium.render());
                    if let Some(account) = cli.opt("account") {
                        println!("(see `mlxysftele whoami {account}` for its server-premium status)");
                    }
                }
                "on" => {
                    cfg.local_premium.set_all(true);
                    cfg.save(data_dir)?;
                    println!("local premium: ON (client-side conveniences only — `premium info`)");
                }
                "off" => {
                    cfg.local_premium.set_all(false);
                    cfg.save(data_dir)?;
                    println!("local premium: OFF");
                }
                "set" => {
                    let flag = required_pos(cli, 1, "premium set <flag> on|off")?.to_string();
                    let value = required_pos(cli, 2, "premium set <flag> on|off")?;
                    let on = match value {
                        "on" => true,
                        "off" => false,
                        other => return Err(format!("expected 'on' or 'off', got '{other}'").into()),
                    };
                    if !cfg.local_premium.set_named(&flag, on) {
                        return Err(format!(
                            "unknown premium flag '{flag}' (known: enabled, themes, fast_animations, sticker_shortcuts, hide_sponsored, hidden_premium_nags)"
                        )
                        .into());
                    }
                    cfg.save(data_dir)?;
                    println!("{flag} = {on}");
                }
                "info" => print!("{}", premium::render_info()),
                other => {
                    return Err(format!("unknown premium subcommand '{other}'").into());
                }
            }
            Ok(())
        }
        "stats" => {
            let cfg = config::Config::load(data_dir)?;
            let data_bytes = util::dir_size_bytes(data_dir).unwrap_or(0);
            print!(
                "{}",
                lowmem::report(
                    cfg.accounts.len(),
                    data_bytes,
                    &data_dir.display().to_string()
                )
            );
            Ok(())
        }
        "logout" => {
            let name = required_pos(cli, 0, "logout <name>")?.to_string();
            let mut cfg = config::Config::load(data_dir)?;
            telegram::logout(data_dir, &mut cfg, &name, cli.flag("force")).await
        }
        "remove" => {
            let name = required_pos(cli, 0, "remove <name>")?.to_string();
            let mut cfg = config::Config::load(data_dir)?;
            telegram::remove_local(data_dir, &mut cfg, &name)
        }
        other => Err(format!("unknown command '{other}' — try `mlxysftele help`").into()),
    }
}

fn required_pos<'a>(cli: &'a Cli, idx: usize, usage: &str) -> Result<&'a str, telegram::BoxError> {
    cli.pos(idx)
        .ok_or_else(|| format!("missing argument — usage: mlxysftele {usage}").into())
}
