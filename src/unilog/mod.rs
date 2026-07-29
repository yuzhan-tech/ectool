pub mod capture;
pub mod comdb;
pub mod decode;
pub mod filter;
pub mod output;

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Baud rate for the UniLog USB CDC interface. EC7xx firmware fixes this
/// value; the host side passes it through because the `serialport` crate
/// requires a rate, but USB CDC ACM ignores it at the wire.
const UNILOG_BAUD: u32 = 921600;

/// Stack-scratch buffer for one `serial.read()` call.
const READ_BUF_SIZE: usize = 4096;
/// EPAT sends `at^logversion\r\n` (note the CRLF) on the same port; the device's
/// `logToolCommandHandle` matches the `^logversion` substring. The reply, if any,
/// comes back as a UniLog record (`"LOGVERSION : %s"`), not a plain AT response.
const LOG_VERSION_COMMAND: &[u8] = b"^logversion\r\n";
const LOG_VERSION_TIMEOUT: Duration = Duration::from_millis(1500);

/// Arguments for the `unilog` subcommand.
pub struct UnilogArgs {
    pub port: Option<String>,
    pub comdb: Option<PathBuf>,
    pub raw: bool,
    pub phy: bool,
    pub owner: Vec<String>,
    pub module: Vec<String>,
    pub sub: Vec<String>,
    pub level: Vec<String>,
    pub file: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub append: bool,
    pub version_check: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DictionarySource {
    Comdb(PathBuf),
}

fn select_dictionary_source(raw: bool, comdb: Option<PathBuf>) -> Result<Option<DictionarySource>> {
    if raw {
        return Ok(None);
    }

    if let Some(path) = comdb {
        return Ok(Some(DictionarySource::Comdb(path)));
    }

    bail!("unilog decoding requires a dictionary: pass --comdb <comdb.txt> or --raw")
}

fn load_dictionary(source: &DictionarySource) -> Result<comdb::Comdb> {
    match source {
        DictionarySource::Comdb(path) => {
            let db = comdb::Comdb::from_path(path)
                .with_context(|| format!("loading comdb {}", path.display()))?;
            log::info!("Loaded comdb {} ({} sites)", path.display(), db.len());
            Ok(db)
        }
    }
}

fn open_out_file(path: &Path, append: bool) -> Result<File> {
    if append {
        return OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open --out {}", path.display()));
    }

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "--out {} already exists; pass --append to append",
                path.display()
            )
        }
        Err(err) => Err(err).with_context(|| format!("Failed to create --out {}", path.display())),
    }
}

fn normalize_log_version(version: &str) -> Option<String> {
    let trimmed = version
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '\0');
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    let parsed = u32::from_str_radix(hex, 16).ok()?;
    Some(format!("0x{parsed:08x}"))
}

fn extract_log_version(body: &str) -> Option<String> {
    let marker = body.find("LOGVERSION")?;
    let rest = &body[marker + "LOGVERSION".len()..];
    let value = rest.trim_start().strip_prefix(':')?.trim_start();
    let token = value.split_whitespace().next()?;
    normalize_log_version(token)
}

fn validate_log_version(device_version: &str, db: &comdb::Comdb) -> Result<()> {
    let expected = db
        .db_version()
        .ok_or_else(|| anyhow::anyhow!("comdb has no DbVersion block"))?;
    let actual = normalize_log_version(device_version)
        .ok_or_else(|| anyhow::anyhow!("invalid device LOGVERSION: {device_version:?}"))?;

    if actual == expected {
        Ok(())
    } else {
        bail!("UniLog DB version mismatch: device {actual}, comdb {expected}")
    }
}

/// Probe the device's UniLog DB version with `at^logversion` — best effort.
///
/// The reply (when it comes) is a UniLog record `"LOGVERSION : <hex>"` emitted by
/// `logToolCommandHandle` in the SDK, not a plain AT response. In practice many
/// firmware builds never answer: the USB UniLog endpoint can be IN-only, so
/// the downlink command never reaches the handler. EPAT itself sends the command
/// repeatedly, gets no reply, and proceeds anyway. We mirror that: warn on no
/// reply or a mismatch, but never abort the capture. Records seen while waiting
/// are printed so no data is lost.
fn verify_device_log_version(
    serial: &mut dyn serialport::SerialPort,
    db: &comdb::Comdb,
    capture: &mut capture::EpatStreamDecoder,
    out_file: &mut Option<File>,
    show_phy: bool,
    filters: &filter::Filters,
) -> Result<()> {
    let Some(expected) = db.db_version() else {
        log::warn!("comdb has no DbVersion block; skipping UniLog DB version check");
        return Ok(());
    };

    serial
        .write_all(LOG_VERSION_COMMAND)
        .context("write logversion command")?;
    serial.flush().context("flush logversion command")?;
    let cmd = String::from_utf8_lossy(LOG_VERSION_COMMAND);
    println!("{}", output::render_tx(cmd.trim_end()));

    let deadline = Instant::now() + LOG_VERSION_TIMEOUT;
    let mut buf = [0u8; READ_BUF_SIZE];

    while Instant::now() < deadline {
        match serial.read(&mut buf) {
            Ok(n) if n > 0 => {
                let chunk = &buf[..n];
                if let Some(f) = out_file.as_mut() {
                    f.write_all(chunk).context("write --out file")?;
                }

                let records = capture.feed(chunk);
                for rec in &records {
                    let line = decode::decode(rec, db);
                    if let Some(version) = extract_log_version(&line.body) {
                        // Show the device's LOGVERSION response like any record,
                        // then compare quietly — only a mismatch is worth a log.
                        println!("{}", output::render_decoded(&line));
                        if let Err(e) = validate_log_version(&version, db) {
                            log::warn!("{e}");
                        }
                        return Ok(());
                    }
                    if hide_unmapped_phy(rec, &line, show_phy) {
                        continue;
                    }
                    if !filters.accepts(rec, &line) {
                        continue;
                    }
                    println!("{}", output::render_decoded(&line));
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("read at^logversion response"),
        }
    }

    log::warn!(
        "Device did not report a UniLog DB version (this firmware may not answer \
         ^logversion); proceeding with comdb {}",
        expected
    );
    Ok(())
}

/// Owner IDs 0 (PHY_ONLINE) and 1 (PHY_OFFLINE) are the PHY layer. PHY records
/// have no text definition in any comdb/database and make up the bulk of the
/// stream, so unmapped PHY records are hidden unless `--phy` is set.
const UNILOG_PHY_OWNER_MAX: u8 = 1;

/// Whether to suppress a decoded line: an unmapped PHY record with `--phy` off.
fn hide_unmapped_phy(record: &capture::Record, line: &decode::DecodedLine, show_phy: bool) -> bool {
    line.unmapped && !show_phy && record.owner_id() <= UNILOG_PHY_OWNER_MAX
}

/// Entrypoint for the `unilog` subcommand. Captures the UniLog binary stream
/// from the device and prints decoded log records.
pub fn run(args: UnilogArgs) -> Result<()> {
    let UnilogArgs {
        port,
        comdb,
        raw,
        phy,
        owner,
        module,
        sub,
        level,
        file,
        out,
        append,
        version_check,
    } = args;

    let dictionary_source = select_dictionary_source(raw, comdb)?;
    let db = dictionary_source
        .as_ref()
        .map(load_dictionary)
        .transpose()?;
    let raw_mode = raw;
    let show_phy = phy;

    let filters = filter::Filters::parse(&owner, &module, &sub, &level)?;
    if raw_mode && filters.has_name_or_level_terms() {
        log::warn!("--raw mode can only filter by numeric id; name/level filters are ignored");
    }

    if let Some(path) = file {
        return replay_from_file(&path, db.as_ref(), raw_mode, show_phy, &filters);
    }

    // Note up front (with the other startup logs) rather than mid-stream, so the
    // record output stays clean. Only relevant in decoded mode — raw mode shows
    // PHY records.
    if !raw_mode && !show_phy && db.is_some() {
        log::info!("Hiding undecodable PHY records (owner 0/1); pass --phy to show them");
    }

    let mut out_file = match &out {
        Some(path) => Some(open_out_file(path, append)?),
        None => None,
    };

    let mut port_name =
        port.ok_or_else(|| anyhow::anyhow!("live UniLog capture requires --port <PORT>"))?;

    let mut serial = match open_unilog_port(&port_name) {
        Ok(serial) => {
            log::info!("Open {}", port_name);
            serial
        }
        Err(e) => return Err(e),
    };

    let mut buf = [0u8; READ_BUF_SIZE];
    let mut capture = capture::EpatStreamDecoder::default();
    if version_check {
        if let Some(ref db) = db {
            verify_device_log_version(
                serial.as_mut(),
                db,
                &mut capture,
                &mut out_file,
                show_phy,
                &filters,
            )?;
        }
    }

    loop {
        match serial.read(&mut buf) {
            Ok(n) if n > 0 => {
                let chunk = &buf[..n];
                if let Some(f) = out_file.as_mut() {
                    f.write_all(chunk).context("write --out file")?;
                }
                let records = capture.feed(chunk);
                for rec in &records {
                    if raw_mode {
                        if filters.accepts_raw(rec) {
                            println!("{}", output::render_raw(rec));
                        }
                    } else if let Some(ref db) = db {
                        let line = decode::decode(rec, db);
                        if hide_unmapped_phy(rec, &line, show_phy) {
                            continue;
                        }
                        if !filters.accepts(rec, &line) {
                            continue;
                        }
                        println!("{}", output::render_decoded(&line));
                    }
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                log::warn!(
                    "UniLog port read error on {}: {}. Waiting for reconnect...",
                    port_name,
                    e
                );
                capture = capture::EpatStreamDecoder::default();
                let reopened = wait_open_unilog_port(&port_name)?;
                port_name = reopened.0;
                serial = reopened.1;
                if version_check {
                    if let Some(ref db) = db {
                        verify_device_log_version(
                            serial.as_mut(),
                            db,
                            &mut capture,
                            &mut out_file,
                            show_phy,
                            &filters,
                        )?;
                    }
                }
            }
        }
    }
}

/// Replay a captured byte file through the same decode pipeline as the live
/// path, so a `--out` capture (or an EPAT `RecvDump/*.bin` dump) can be decoded
/// offline. EPAT dump framing is stripped automatically when present.
fn replay_from_file(
    path: &Path,
    db: Option<&comdb::Comdb>,
    raw_mode: bool,
    show_phy: bool,
    filters: &filter::Filters,
) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let stream = capture::strip_epat_dump_framing(&bytes);
    if stream.len() != bytes.len() {
        log::info!(
            "Stripped EPAT dump framing: {} -> {} bytes",
            bytes.len(),
            stream.len()
        );
    }

    let mut capture = capture::EpatStreamDecoder::default();

    // Feed in read-sized chunks so the decoder exercises the same incremental
    // path as live capture (a single huge feed would behave differently across
    // resyncs).
    let mut records = Vec::new();
    for chunk in stream.chunks(READ_BUF_SIZE) {
        records.extend(capture.feed(chunk));
    }
    records.extend(capture.flush());

    let mut printed = 0usize;
    let mut hidden_phy = 0usize;
    for rec in &records {
        if raw_mode {
            if filters.accepts_raw(rec) {
                println!("{}", output::render_raw(rec));
                printed += 1;
            }
        } else if let Some(db) = db {
            let line = decode::decode(rec, db);
            if hide_unmapped_phy(rec, &line, show_phy) {
                hidden_phy += 1;
                continue;
            }
            if !filters.accepts(rec, &line) {
                continue;
            }
            println!("{}", output::render_decoded(&line));
            printed += 1;
        }
    }

    if hidden_phy > 0 {
        log::info!(
            "Replayed {} records from {} ({} undecodable PHY records hidden; pass --phy to show)",
            printed,
            path.display(),
            hidden_phy
        );
    } else {
        log::info!("Replayed {} records from {}", printed, path.display());
    }
    Ok(())
}

fn open_unilog_port(port_name: &str) -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new(port_name, UNILOG_BAUD)
        .timeout(Duration::from_millis(100))
        .open()
        .with_context(|| format!("Failed to open UniLog port {}", port_name))
}

fn wait_open_unilog_port(port: &str) -> Result<(String, Box<dyn serialport::SerialPort>)> {
    loop {
        let is_present = serialport::available_ports()
            .context("Failed to list serial ports")?
            .iter()
            .any(|candidate| candidate.port_name == port);
        if !is_present {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        match open_unilog_port(port) {
            Ok(serial) => {
                log::info!("Open {}", port);
                return Ok((port.to_string(), serial));
            }
            Err(e) => {
                log::warn!(
                    "Failed to open UniLog port {} after it appeared: {}. Retrying...",
                    port,
                    e
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dictionary_is_required_unless_raw_is_enabled() {
        let err = select_dictionary_source(false, None).unwrap_err();

        assert!(err.to_string().contains("--comdb"));
        assert!(err.to_string().contains("--raw"));
    }

    #[test]
    fn raw_mode_allows_missing_dictionary() {
        assert_eq!(select_dictionary_source(true, None).unwrap(), None);
    }

    #[test]
    fn explicit_comdb_is_used() {
        let comdb = PathBuf::from("comdb.txt");

        assert_eq!(
            select_dictionary_source(false, Some(comdb.clone())).unwrap(),
            Some(DictionarySource::Comdb(comdb))
        );
    }

    fn decoded(unmapped: bool) -> decode::DecodedLine {
        decode::DecodedLine {
            level: None,
            owner: String::new(),
            module: String::new(),
            site: String::new(),
            body: String::new(),
            unmapped,
        }
    }

    #[test]
    fn hides_unmapped_phy_records_unless_requested() {
        let phy = capture::Record::new(0x1000_0000, vec![]); // owner=1 PHY_OFFLINE
        let custom = capture::Record::new(0x6000_0000, vec![]); // owner=6 CUSTOMER

        // Unmapped PHY is hidden by default, shown with --phy.
        assert!(hide_unmapped_phy(&phy, &decoded(true), false));
        assert!(!hide_unmapped_phy(&phy, &decoded(true), true));
        // Mapped PHY (has a comdb hit) is never hidden.
        assert!(!hide_unmapped_phy(&phy, &decoded(false), false));
        // Unmapped non-PHY (e.g. SIG_DUMP / PS records) is always shown.
        assert!(!hide_unmapped_phy(&custom, &decoded(true), false));
    }

    #[test]
    fn extracts_log_version_from_decoded_response() {
        assert_eq!(
            extract_log_version("LOGVERSION : 0x09573d6f "),
            Some("0x09573d6f".to_string())
        );
        assert_eq!(
            extract_log_version("LOGVERSION:0X09573D6F"),
            Some("0x09573d6f".to_string())
        );
    }

    #[test]
    fn validates_matching_log_version() {
        let db = comdb::Comdb::parse(
            "\
DbVersion
156712303,100
<end>
0,0,0,0,PHY_ONLINE,FOO_MOD,Site0,P_INFO,swLogPrintf(\"value=%d \");
",
        )
        .unwrap();

        validate_log_version("0X09573D6F", &db).unwrap();
    }

    #[test]
    fn rejects_mismatched_log_version() {
        let db = comdb::Comdb::parse(
            "\
DbVersion
156712303,100
<end>
0,0,0,0,PHY_ONLINE,FOO_MOD,Site0,P_INFO,swLogPrintf(\"value=%d \");
",
        )
        .unwrap();

        let err = validate_log_version("0x11111111", &db).unwrap_err();

        assert!(err.to_string().contains("mismatch"));
        assert!(err.to_string().contains("0x11111111"));
        assert!(err.to_string().contains("0x09573d6f"));
    }

    #[test]
    fn out_file_without_append_errors_if_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("capture.bin");
        std::fs::write(&path, b"old").unwrap();

        let err = open_out_file(&path, false).unwrap_err();

        assert!(err.to_string().contains("--append"));
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
    }

    #[test]
    fn out_file_with_append_preserves_existing_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("capture.bin");
        std::fs::write(&path, b"old").unwrap();

        let mut file = open_out_file(&path, true).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&path).unwrap(), b"oldnew");
    }

    #[test]
    fn out_file_without_append_creates_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("capture.bin");

        let mut file = open_out_file(&path, false).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}
