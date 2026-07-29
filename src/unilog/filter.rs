//! Record filtering for the `unilog` command.
//!
//! Filters narrow the decoded output by owner, module, sub (site name), and/or
//! log level. Categories combine with AND; values within a category combine with
//! OR. A purely-numeric value matches the corresponding numeric id; any other
//! value is a case-insensitive substring match against the decoded name.

use crate::unilog::capture::Record;
use crate::unilog::comdb::Level;
use crate::unilog::decode::DecodedLine;
use anyhow::{bail, Result};

/// One filter token: a numeric id (matched exactly against the record's id) or a
/// name fragment (case-insensitive substring of the decoded name).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Term {
    Id(u32),
    Name(String),
}

impl Term {
    fn parse(token: &str) -> Self {
        let t = token.trim();
        match t.parse::<u32>() {
            Ok(id) => Term::Id(id),
            Err(_) => Term::Name(t.to_ascii_lowercase()),
        }
    }

    fn matches(&self, name: &str, id: u32) -> bool {
        match self {
            Term::Id(v) => *v == id,
            Term::Name(s) => name.to_ascii_lowercase().contains(s),
        }
    }
}

/// Parsed filter criteria. An empty category imposes no constraint.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    owners: Vec<Term>,
    modules: Vec<Term>,
    subs: Vec<Term>,
    levels: Vec<Level>,
}

impl Filters {
    /// Parse comma-separated CLI lists. `level` tokens must name a level
    /// (DBG/INF/VAL/SIG/WRN/ERR, also accepts INFO/WARNING/… and P_ forms).
    pub fn parse(
        owner: &[String],
        module: &[String],
        sub: &[String],
        level: &[String],
    ) -> Result<Self> {
        let mut levels = Vec::new();
        for tok in split_tokens(level) {
            let parsed = parse_level(&tok)?;
            if !levels.contains(&parsed) {
                levels.push(parsed);
            }
        }
        Ok(Self {
            owners: split_tokens(owner).iter().map(|t| Term::parse(t)).collect(),
            modules: split_tokens(module)
                .iter()
                .map(|t| Term::parse(t))
                .collect(),
            subs: split_tokens(sub).iter().map(|t| Term::parse(t)).collect(),
            levels,
        })
    }

    /// True if any constraint is set.
    pub fn is_active(&self) -> bool {
        !(self.owners.is_empty()
            && self.modules.is_empty()
            && self.subs.is_empty()
            && self.levels.is_empty())
    }

    /// True if any name (non-numeric) or level constraint is set — these only
    /// work in decoded mode, so callers can warn when combined with `--raw`.
    pub fn has_name_or_level_terms(&self) -> bool {
        !self.levels.is_empty()
            || [&self.owners, &self.modules, &self.subs]
                .iter()
                .any(|terms| terms.iter().any(|t| matches!(t, Term::Name(_))))
    }

    /// Whether a decoded line passes all active filters.
    pub fn accepts(&self, record: &Record, line: &DecodedLine) -> bool {
        dim(&self.owners, &line.owner, record.owner_id() as u32)
            && dim(&self.modules, &line.module, record.mod_id() as u32)
            && dim(&self.subs, &line.site, record.sub_id() as u32)
            && (self.levels.is_empty() || line.level.is_some_and(|l| self.levels.contains(&l)))
    }

    /// Whether a raw (un-decoded) record passes. Only numeric-id constraints
    /// apply; name and level constraints are unavailable without decoding and
    /// are ignored here.
    pub fn accepts_raw(&self, record: &Record) -> bool {
        raw_dim(&self.owners, record.owner_id() as u32)
            && raw_dim(&self.modules, record.mod_id() as u32)
            && raw_dim(&self.subs, record.sub_id() as u32)
    }
}

/// Split each CLI value on commas and drop blanks (clap may already have split
/// on commas via the value delimiter; this is idempotent and handles both).
fn split_tokens(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn dim(terms: &[Term], name: &str, id: u32) -> bool {
    terms.is_empty() || terms.iter().any(|t| t.matches(name, id))
}

/// Raw-mode dimension match: ignore name terms, honor numeric-id terms.
fn raw_dim(terms: &[Term], id: u32) -> bool {
    let ids: Vec<u32> = terms
        .iter()
        .filter_map(|t| match t {
            Term::Id(v) => Some(*v),
            Term::Name(_) => None,
        })
        .collect();
    ids.is_empty() || ids.contains(&id)
}

fn parse_level(token: &str) -> Result<Level> {
    let lower = token.trim().to_ascii_lowercase();
    let name = lower.strip_prefix("p_").unwrap_or(&lower);
    Ok(match name {
        "dbg" | "debug" => Level::Debug,
        "inf" | "info" => Level::Info,
        "val" | "value" => Level::Value,
        "sig" => Level::Sig,
        "wrn" | "warn" | "warning" => Level::Warning,
        "err" | "error" => Level::Error,
        _ => bail!("unknown log level '{token}' (use DBG/INF/VAL/SIG/WRN/ERR)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(owner: &str, module: &str, site: &str, level: Option<Level>) -> DecodedLine {
        DecodedLine {
            level,
            owner: owner.to_string(),
            module: module.to_string(),
            site: site.to_string(),
            body: String::new(),
            unmapped: false,
        }
    }

    #[test]
    fn empty_filters_accept_everything() {
        let f = Filters::default();
        assert!(!f.is_active());
        let rec = Record::new(0x6000_0000, vec![]);
        assert!(f.accepts(&rec, &line("CUSTOMER", "APP", "debug", Some(Level::Error))));
        assert!(f.accepts_raw(&rec));
    }

    #[test]
    fn owner_name_and_id_match() {
        // header owner=2 (PLAT_AP)
        let rec = Record::new(0x2000_0000, vec![]);
        let l = line("PLAT_AP", "CCIO", "HANDLE_CMSG_3", Some(Level::Value));

        assert!(Filters::parse(&["plat_ap".into()], &[], &[], &[])
            .unwrap()
            .accepts(&rec, &l));
        assert!(Filters::parse(&["2".into()], &[], &[], &[])
            .unwrap()
            .accepts(&rec, &l));
        assert!(!Filters::parse(&["CUSTOMER".into()], &[], &[], &[])
            .unwrap()
            .accepts(&rec, &l));
    }

    #[test]
    fn module_substring_and_within_category_or() {
        let rec = Record::new(0x2000_0000, vec![]);
        let l = line("PLAT_AP", "CCIO", "x", None);
        // substring
        assert!(Filters::parse(&[], &["cci".into()], &[], &[])
            .unwrap()
            .accepts(&rec, &l));
        // OR within category
        assert!(Filters::parse(&[], &["PMU,CCIO".into()], &[], &[])
            .unwrap()
            .accepts(&rec, &l));
        assert!(!Filters::parse(&[], &["PMU,PLA_HAL".into()], &[], &[])
            .unwrap()
            .accepts(&rec, &l));
    }

    #[test]
    fn categories_combine_with_and() {
        let rec = Record::new(0x2000_0000, vec![]);
        let l = line("PLAT_AP", "CCIO", "x", Some(Level::Error));
        // owner matches but module does not -> rejected
        assert!(
            !Filters::parse(&["PLAT_AP".into()], &["PMU".into()], &[], &[])
                .unwrap()
                .accepts(&rec, &l)
        );
        // both match -> accepted
        assert!(
            Filters::parse(&["PLAT_AP".into()], &["CCIO".into()], &[], &[])
                .unwrap()
                .accepts(&rec, &l)
        );
    }

    #[test]
    fn level_filter() {
        let rec = Record::new(0x2000_0000, vec![]);
        let err = line("PLAT_AP", "CCIO", "x", Some(Level::Error));
        let dbg = line("PLAT_AP", "CCIO", "x", Some(Level::Debug));
        let none = line("1", "11", "67", None);

        let f = Filters::parse(&[], &[], &[], &["WRN,ERR".into()]).unwrap();
        assert!(f.accepts(&rec, &err));
        assert!(!f.accepts(&rec, &dbg));
        // unknown level is excluded when a level filter is active
        assert!(!f.accepts(&rec, &none));
    }

    #[test]
    fn raw_mode_uses_numeric_ids_only() {
        let phy = Record::new(0x1000_0000, vec![]); // owner=1
        let f_id = Filters::parse(&["1".into()], &[], &[], &[]).unwrap();
        assert!(f_id.accepts_raw(&phy));
        let f_other = Filters::parse(&["2".into()], &[], &[], &[]).unwrap();
        assert!(!f_other.accepts_raw(&phy));
        // a name-only owner filter is ignored in raw mode (passes), level ignored
        let f_name = Filters::parse(&["PHY".into()], &[], &[], &["ERR".into()]).unwrap();
        assert!(f_name.accepts_raw(&phy));
        assert!(f_name.has_name_or_level_terms());
    }

    #[test]
    fn bad_level_errors() {
        assert!(Filters::parse(&[], &[], &[], &["bogus".into()]).is_err());
    }
}
