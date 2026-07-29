//! Resolve a captured Record against a Comdb to produce a DecodedLine.

use crate::unilog::capture::Record;
use crate::unilog::comdb::{Comdb, Level, LogSite};
use crate::util::printf::fmt_printf;

pub struct DecodedLine {
    pub level: Option<Level>,
    pub owner: String,
    pub module: String,
    pub site: String,
    pub body: String,
    /// True if no comdb entry matched.
    pub unmapped: bool,
}

/// Resolve a record against the comdb and render a line.
///
/// Lookup is by owner/module/sub only (`Comdb::lookup_omsub`). The wire header's
/// low 11 bits are the runtime payload length — the firmware overwrites the
/// compile-time placeholder in those bits — so a full 32-bit ("exact") match
/// could never hold for variable-length records. owner/module/sub uniquely
/// identifies a site (collision-free in the comdbs we've seen).
pub fn decode(record: &Record, db: &Comdb) -> DecodedLine {
    match db.lookup_omsub(record.header) {
        Some(site) => {
            let body = if site.is_dump {
                render_dump_body(site, &record.payload)
            } else {
                fmt_printf(&site.fmt, &record.payload)
            };
            DecodedLine {
                level: Some(site.level),
                owner: site.owner.clone(),
                module: site.module.clone(),
                site: site.site.clone(),
                body,
                unmapped: false,
            }
        }
        None => DecodedLine {
            level: None,
            owner: record.owner_id().to_string(),
            module: record.mod_id().to_string(),
            site: record.sub_id().to_string(),
            body: render_dump(&record.payload),
            unmapped: true,
        },
    }
}

/// Render a dump-site payload. Dump sites carry a plain label (e.g. `"Sig = > "`)
/// followed by raw bytes; EPAT prints the label then the bytes. For `SIG_DUMP`
/// records the bytes are a full-signal dump with a `[u16 sig_id][u16 body_len]`
/// header, which we surface when it self-validates. Everything else (and any
/// payload that doesn't validate) falls back to label + raw hex, matching EPAT.
///
/// We can't resolve `sig_id` to a name (`SIG_CMS_SYN_API_REQ`) — that table ships
/// only as an encrypted blob decrypted inside EPAT's closed DLL.
fn render_dump_body(site: &LogSite, payload: &[u8]) -> String {
    if site.module == "SIG_DUMP" {
        if let Some(decoded) = render_sig_dump(&site.fmt, payload) {
            return decoded;
        }
    }
    let hex = render_dump(payload);
    if site.fmt.is_empty() {
        hex
    } else {
        format!("{}{}", site.fmt, hex)
    }
}

/// Decode a full-signal dump payload `[u16 sig_id][u16 body_len][body..]`.
/// Returns `None` unless the embedded length matches the payload exactly, so a
/// non-structured dump never gets misframed.
fn render_sig_dump(label: &str, payload: &[u8]) -> Option<String> {
    if payload.len() < 4 {
        return None;
    }
    let sig_id = u16::from_le_bytes([payload[0], payload[1]]);
    let body_len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    if body_len != payload.len() - 4 {
        return None;
    }
    Some(format!(
        "{}0x{:x}; body len:{}; body data:{}",
        label,
        sig_id,
        body_len,
        render_dump(&payload[4..])
    ))
}

// TODO(dedup): same hex-byte loop as output::render_raw's payload formatter.
// Promote to util when a third call site appears.
fn render_dump(payload: &[u8]) -> String {
    let mut s = String::with_capacity(payload.len() * 3);
    for (i, b) in payload.iter().enumerate() {
        use std::fmt::Write as _;
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unilog::comdb::Comdb;

    fn make_comdb() -> Comdb {
        let text = "\
0,0,0,0,PHY_ONLINE,FOO_MOD,Site0,P_INFO,swLogPrintf(\"value=%d \");
";
        Comdb::parse(text).expect("parse")
    }

    #[test]
    fn decodes_known_record_with_args() {
        let db = make_comdb();
        // header for sw_log_id 0 — owner=0, mod=0, sub=0, payload_len=4
        let record = Record::new(0x0000_0004, 42i32.to_le_bytes().to_vec());
        let line = decode(&record, &db);
        assert!(!line.unmapped);
        assert_eq!(line.owner, "PHY_ONLINE");
        assert_eq!(line.module, "FOO_MOD");
        assert_eq!(line.site, "Site0");
        assert_eq!(line.body, "value=42 ");
    }

    fn sig_dump_db() -> Comdb {
        // swLogID 0x20000000 = owner=2 PLAT_AP, mod=0 SIG_DUMP, sub=0.
        Comdb::parse(
            "0,536870912,0,0,PLAT_AP,SIG_DUMP,DUMP_FULL_SIGNAL,P_VALUE,swLogDumpPolling(\"Sig = > \");\n",
        )
        .expect("parse")
    }

    #[test]
    fn dump_site_falls_back_to_label_plus_hex_when_unstructured() {
        // 4-byte payload: the embedded length (4) can't match payload.len()-4 (0),
        // so it renders as label + raw hex rather than a misframed signal.
        let db = sig_dump_db();
        let record = Record::new(0x2000_0004, vec![0x42, 0x09, 0x04, 0x00]);
        let line = decode(&record, &db);
        assert!(!line.unmapped);
        assert_eq!(line.module, "SIG_DUMP");
        assert_eq!(line.body, "Sig = > 42 09 04 00");
    }

    #[test]
    fn sig_dump_decodes_structured_full_signal() {
        // Payload [sig_id=0x0942][body_len=4][84 2c 0b 0c] — the real DUMP_FULL_SIGNAL
        // shape; matches EPAT's "Sig = > …(0x942); body len:4; body data:84 2C 0B 0C".
        let db = sig_dump_db();
        // len=8
        let record = Record::new(
            0x2000_0008,
            vec![0x42, 0x09, 0x04, 0x00, 0x84, 0x2c, 0x0b, 0x0c],
        );
        let line = decode(&record, &db);
        assert_eq!(
            line.body,
            "Sig = > 0x942; body len:4; body data:84 2c 0b 0c"
        );
    }

    #[test]
    fn unmapped_record_returns_raw_metadata() {
        let db = make_comdb();
        // CUSTOMER owner, not present in db
        let record = Record::new(0x6000_2000, vec![0xAB, 0xCD]);
        let line = decode(&record, &db);
        assert!(line.unmapped);
        assert_eq!(line.owner, "6");
        assert_eq!(line.body, "ab cd");
        assert!(line.level.is_none());
    }

    #[test]
    fn resolves_regardless_of_payload_length_low_bits() {
        // The comdb sw_log_id 0x60000405 carries a compile-time placeholder in its
        // low 11 bits; wire headers carry the runtime length there instead. Lookup
        // must match on owner/module/sub only, so headers differing solely in the
        // low 11 bits both resolve to the same site.
        let text = "\
786432,1610613765,0,0,CUSTOMER,APP,app_service_14,P_SIG,swLogPrintf(\"%s \");
";
        let db = Comdb::parse(text).expect("parse");

        for header in [0x6000_0000u32, 0x6000_0010, 0x6000_07ff] {
            let line = decode(&Record::new(header, Vec::new()), &db);
            assert!(!line.unmapped, "header {header:#010x} should resolve");
            assert_eq!(line.site, "app_service_14");
        }
    }
}
