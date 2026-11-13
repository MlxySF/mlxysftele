//! Zero-dependency, allocation-free-when-disabled logger.
//!
//! ZapFast-style clients do not ship a logging framework; they ship `eprintln!`
//! and a level switch. This module does the same, with `format_args!` so a
//! disabled debug path does not allocate a `String`.
//!
//! Set `MLXYSFTELE_LOG=debug` for verbose output. Anything else (or unset)
//! prints informational messages and above.

use std::sync::OnceLock;

static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

/// Returns true when `MLXYSFTELE_LOG` is set to a truthy/debug value.
pub fn debug_enabled() -> bool {
    *DEBUG_ENABLED.get_or_init(|| {
        std::env::var("MLXYSFTELE_LOG")
            .map(|v| {
                let v = v.to_ascii_lowercase();
                v == "debug" || v == "trace" || v == "1" || v == "true" || v == "on"
            })
            .unwrap_or(false)
    })
}

/// Emit an error line. Always printed.
#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => {
        eprintln!("[err ] {}", format_args!($($arg)*))
    };
}

/// Emit a warning line. Always printed.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        eprintln!("[warn] {}", format_args!($($arg)*))
    };
}

/// Emit an informational line. Always printed.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        eprintln!("[info] {}", format_args!($($arg)*))
    };
}

/// Emit a debug line. Only printed when `MLXYSFTELE_LOG=debug` is set.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::log::debug_enabled() {
            eprintln!("[dbg ] {}", format_args!($($arg)*))
        }
    };
}
