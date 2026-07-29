//! Render UniLog records to lines of stdout (both raw and decoded forms).

use crate::unilog::capture::Record;
use crate::unilog::comdb::Level;
use crate::unilog::decode::DecodedLine;
use chrono::Local;

const TS_WIDTH: usize = 12; // "HH:MM:SS.mmm"

fn ts_now() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

fn level_short(level: Option<Level>) -> &'static str {
    match level {
        Some(l) => l.short(),
        None => "???",
    }
}

fn level_color(level: Option<Level>) -> &'static str {
    if !is_tty() {
        return "";
    }
    match level {
        Some(Level::Error) => "\x1b[31m",
        Some(Level::Warning) => "\x1b[33m",
        Some(Level::Debug) => "\x1b[2m",
        None => "\x1b[2m",
        _ => "",
    }
}

fn reset() -> &'static str {
    if is_tty() {
        "\x1b[0m"
    } else {
        ""
    }
}

fn is_tty() -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static IS_TTY: OnceLock<bool> = OnceLock::new();
    *IS_TTY.get_or_init(|| std::io::stdout().is_terminal())
}

/// Format a host->device command echo, e.g. `[HH:MM:SS.mmm] TX   ^logversion`.
/// Tagged `TX` and coloured distinctly so sent commands stand out from received
/// log records.
pub fn render_tx(command: &str) -> String {
    let ts = ts_now();
    let color = if is_tty() { "\x1b[36m" } else { "" }; // cyan
    format!(
        "{color}[{ts:>width$}] {tag:<3}  {command}{reset}",
        color = color,
        ts = ts,
        tag = "TX",
        command = command,
        reset = reset(),
        width = TS_WIDTH
    )
}

/// Format a record as `[HH:MM:SS.mmm] raw(O/M/S, len=N) <hex>`.
pub fn render_raw(record: &Record) -> String {
    let ts = ts_now();
    let mut hex = String::with_capacity(record.payload.len() * 3);
    for (i, b) in record.payload.iter().enumerate() {
        use std::fmt::Write as _;
        if i > 0 {
            hex.push(' ');
        }
        let _ = write!(hex, "{:02x}", b);
    }
    format!(
        "[{:>width$}] raw({}/{}/{}, len={}) {}",
        ts,
        record.owner_id(),
        record.mod_id(),
        record.sub_id(),
        record.payload_len(),
        hex,
        width = TS_WIDTH
    )
}

/// Format a comdb-resolved line.
pub fn render_decoded(line: &DecodedLine) -> String {
    let ts = ts_now();
    let color = level_color(line.level);
    let reset = reset();
    let level = level_short(line.level);
    let prefix = if line.unmapped {
        format!("raw({}/{}/{})", line.owner, line.module, line.site)
    } else {
        format!("{}/{}/{}", line.owner, line.module, line.site)
    };
    format!(
        "{color}[{ts:>width$}] {level}  {prefix:<40}  {body}{reset}",
        color = color,
        ts = ts,
        level = level,
        prefix = prefix,
        body = line.body,
        reset = reset,
        width = TS_WIDTH
    )
}
