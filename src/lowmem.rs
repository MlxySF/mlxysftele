//! Low-memory introspection.
//!
//! The whole point of a ZapFast-style client is that you can look at the
//! numbers. `mlxysftele stats` prints what this process actually costs.

#[derive(Debug, Clone, Copy)]
pub struct MemSample {
    pub rss_kb: u64,
    pub vm_kb: u64,
    pub threads: u64,
}

/// Read VmRSS / VmSize / Threads from /proc/self/status (Linux).
/// Returns None elsewhere — the caller prints an explanation instead of
/// a made-up number.
pub fn sample() -> Option<MemSample> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut rss_kb = None;
    let mut vm_kb = None;
    let mut threads = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmSize:") {
            vm_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            threads = rest.trim().parse::<u64>().ok();
        }
    }
    Some(MemSample {
        rss_kb: rss_kb?,
        vm_kb: vm_kb?,
        threads: threads?,
    })
}

fn parse_kb(field: &str) -> Option<u64> {
    field
        .trim()
        .strip_suffix("kB")
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
}

/// Render a short report for `mlxysftele stats`.
pub fn report(registered_accounts: usize, data_bytes: u64, data_dir: &str) -> String {
    let mut out = String::new();
    match sample() {
        Some(s) => {
            out.push_str(&format!("rss:          {}\n", crate::util::human_bytes(s.rss_kb * 1024)));
            out.push_str(&format!("vm:           {}\n", crate::util::human_bytes(s.vm_kb * 1024)));
            out.push_str(&format!("threads:      {}\n", s.threads));
        }
        None => out.push_str("rss/vm/threads: unavailable (/proc not present)\n"),
    }
    out.push_str(&format!(
        "accounts:     {registered_accounts} registered (dormant ones cost ~0 RAM)\n"
    ));
    out.push_str(&format!(
        "on-disk data: {} in {data_dir}\n",
        crate::util::human_bytes(data_bytes)
    ));
    out.push_str("\nLow-RAM levers (see README §Memory model):\n");
    out.push_str("  • current_thread tokio runtime\n");
    out.push_str("  • dormant accounts: 0 sockets, 0 crypto state\n");
    out.push_str("  • streamed downloads (no media buffering)\n");
    out.push_str("  • opt-level=z, LTO fat, panic=abort, stripped binary\n");
    out.push_str("  • optional: --features mimalloc\n");
    out
}
