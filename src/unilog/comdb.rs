//! Parse a comdb.txt produced by the ec7xx-csdk PrePass tool.
//!
//! Row layout (comma-separated, 9 fields):
//! `<mod_key>,<swLogID>,<u1>,<u2>,<owner>,<module>,<site>,<level>,<orig_call>`
//!
//! The 9th field is the original C call, e.g. `swLogPrintf("fmt %d");` and may
//! contain embedded commas inside the quoted format string.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Value,
    Sig,
    Warning,
    Error,
}

impl Level {
    fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "P_DEBUG" => Some(Self::Debug),
            "P_INFO" => Some(Self::Info),
            "P_VALUE" => Some(Self::Value),
            "P_SIG" => Some(Self::Sig),
            "P_WARNING" => Some(Self::Warning),
            "P_ERROR" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            Self::Debug => "DBG",
            Self::Info => "INF",
            Self::Value => "VAL",
            Self::Sig => "SIG",
            Self::Warning => "WRN",
            Self::Error => "ERR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogSite {
    pub sw_log_id: u32,
    pub owner: String,
    pub module: String,
    pub site: String,
    pub level: Level,
    pub fmt: String,
    pub is_dump: bool,
}

pub struct Comdb {
    sites: HashMap<u32, LogSite>,
    /// Secondary index keyed by `(swLogID >> 11) & 0x1FFFFF` (owner+mod+sub only).
    by_omsub: HashMap<u32, u32>,
    db_version: Option<String>,
}

impl Comdb {
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Failed to read comdb at {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut sites: HashMap<u32, LogSite> = HashMap::new();
        let mut by_omsub: HashMap<u32, u32> = HashMap::new();
        let mut db_version = None;
        let mut in_db_version = false;

        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "DbVersion" {
                in_db_version = true;
                continue;
            }
            if in_db_version {
                if trimmed == "<end>" {
                    in_db_version = false;
                    continue;
                }
                if db_version.is_none() {
                    db_version = parse_db_version_line(trimmed);
                }
                continue;
            }
            match parse_row(trimmed) {
                Some(site) => {
                    let omsub = (site.sw_log_id >> 11) & 0x001F_FFFF;
                    by_omsub.entry(omsub).or_insert(site.sw_log_id);
                    sites.insert(site.sw_log_id, site);
                }
                None => {
                    log::debug!("comdb line {} unparseable: {:?}", lineno + 1, trimmed);
                }
            }
        }

        Ok(Self {
            sites,
            by_omsub,
            db_version,
        })
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub fn db_version(&self) -> Option<&str> {
        self.db_version.as_deref()
    }

    pub fn lookup_exact(&self, header: u32) -> Option<&LogSite> {
        self.sites.get(&header)
    }

    pub fn lookup_omsub(&self, header: u32) -> Option<&LogSite> {
        let omsub = (header >> 11) & 0x001F_FFFF;
        let id = self.by_omsub.get(&omsub)?;
        self.sites.get(id)
    }
}

fn parse_db_version_line(line: &str) -> Option<String> {
    let value = line.split(',').next()?.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        let parsed = u32::from_str_radix(hex, 16).ok()?;
        return Some(format!("0x{parsed:08x}"));
    }

    let parsed = value.parse::<u32>().ok()?;
    Some(format!("0x{parsed:08x}"))
}

fn parse_row(line: &str) -> Option<LogSite> {
    let mut parts = Vec::with_capacity(9);
    let mut remaining = line;
    for _ in 0..8 {
        let (head, tail) = remaining.split_once(',')?;
        parts.push(head);
        remaining = tail;
    }
    parts.push(remaining);

    let sw_log_id: u32 = parts[1].trim().parse().ok()?;
    let owner = parts[4].trim().to_string();
    let module = parts[5].trim().to_string();
    let site = parts[6].trim().to_string();
    let level = Level::from_str(parts[7])?;

    let (fmt, is_dump) = extract_fmt(parts[8])?;

    Some(LogSite {
        sw_log_id,
        owner,
        module,
        site,
        level,
        fmt,
        is_dump,
    })
}

fn extract_fmt(call: &str) -> Option<(String, bool)> {
    let trimmed = call.trim().trim_end_matches(';').trim();
    let (kind, rest) = if let Some(r) = trimmed.strip_prefix("swLogPrintf(") {
        (false, r)
    } else if let Some(r) = trimmed.strip_prefix("swLogDumpPolling(") {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix("swLogDump(") {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix("swLogExcep(") {
        (false, r)
    } else if let Some(r) = trimmed.strip_prefix("swLogInternalPrintf(") {
        (false, r)
    } else if let Some(r) = trimmed.strip_prefix("swLogInternalDump(") {
        (true, r)
    } else {
        return None;
    };
    let rest = rest.strip_suffix(')')?;
    let rest = rest.trim();
    let rest = rest.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some((out, kind))
}
