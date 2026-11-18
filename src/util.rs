//! Small, dependency-free helpers shared across the crate.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Root of all mlxysftele state (config & session files).
///
/// Resolution order:
/// 1. `MLXYSFTELE_HOME` if set (handy for tests and portable installs)
/// 2. `$HOME/.mlxysftele` on Unix, `%USERPROFILE%\.mlxysftele` on Windows
/// 3. `./.mlxysftele` as a last resort
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MLXYSFTELE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".mlxysftele")
}

/// Directory holding per-account sqlite session files.
pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// sqlite session file for a given account name. Caller must ensure the name
/// was validated with [`valid_account_name`].
pub fn session_path(data_dir: &Path, name: &str) -> PathBuf {
    sessions_dir(data_dir).join(format!("{name}.sqlite"))
}

/// Account names are file names; keep them conservative.
pub fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Human-friendly byte counts (1.5 MiB style).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Relative time like "3d ago" — coarse on purpose, avoids a chrono dependency.
pub fn human_ago(then_unix: u64) -> String {
    let now = now_unix();
    if then_unix == 0 {
        return "never".to_string();
    }
    if then_unix > now {
        return "in the future".to_string();
    }
    let delta = now - then_unix;
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        86400..=2591999 => format!("{}d ago", delta / 86400),
        2592000..=31535999 => format!("{}mo ago", delta / 2592000),
        _ => format!("{}y ago", delta / 31536000),
    }
}

/// Restrict a file to owner read/write (0600) on Unix; no-op elsewhere.
/// Sessions and config contain credentials and must not be world-readable.
pub fn secure_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Prompt the user and read one line from stdin, without blocking the async
/// runtime thread (stdin is line-delimited and blocking by nature).
pub async fn read_line(prompt: &str) -> io::Result<String> {
    use std::io::Write;
    print!("{prompt}");
    io::stdout().flush()?;
    let handle = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let stdin = io::stdin();
        let mut lock = stdin.lock();
        use io::BufRead;
        lock.read_line(&mut line).map(|_| line)
    });
    let line = handle
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Prompt for a hidden password (no echo) using rpassword on a blocking
/// thread. The plaintext is only held for the duration of the call.
pub async fn read_secret(prompt: &str) -> io::Result<String> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
}

/// Recursively sum file sizes under `dir`. Used by `stats` to report how much
/// disk the (many) idle session files occupy.
pub fn dir_size_bytes(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    if !dir.exists() {
        return Ok(0);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_name_validation() {
        assert!(valid_account_name("work"));
        assert!(valid_account_name("work_2-main.prod"));
        assert!(!valid_account_name(""));
        assert!(!valid_account_name("has space"));
        assert!(!valid_account_name("slash/name"));
        assert!(!valid_account_name("../escape"));
        assert!(!valid_account_name(&"x".repeat(65)));
    }

    #[test]
    fn human_bytes_ranges() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MiB");
    }

    #[test]
    fn human_ago_never() {
        assert_eq!(human_ago(0), "never");
    }
}
