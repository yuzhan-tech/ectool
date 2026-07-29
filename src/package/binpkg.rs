use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

pub const UNKNOWN_PRODUCT: &str = "unknown";

/// A single entry parsed from a binpkg file.
#[derive(Debug, Clone)]
pub struct BinpkgEntry {
    pub name: String,
    pub addr: u32,
    pub flash_size: u32,
    pub offset: u32,
    pub image_size: u32,
    pub hash: String,
    pub image_type: String,
    pub vt: u16,
    pub vtsize: u16,
    pub rsvd: u32,
    pub pdata: u32,
    pub data: Option<Vec<u8>>,
}

/// Result of parsing a binpkg file.
#[derive(Debug)]
pub struct BinpkgResult {
    pub product_name: String,
    /// Raw header bytes before the first entry (preserved for serialization).
    pub raw_header: Vec<u8>,
    /// Entries in order as they appear in the binpkg.
    pub entries: Vec<BinpkgEntry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BundledFlashConfig {
    pub agent_baud: Option<u32>,
    pub pullup_qspi: Option<bool>,
    pub dribble_download: Option<bool>,
}

impl BinpkgResult {
    /// Find an entry by name.
    pub fn find_entry(&self, name: &str) -> Option<&BinpkgEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Find a mutable entry by name.
    pub fn find_entry_mut(&mut self, name: &str) -> Option<&mut BinpkgEntry> {
        self.entries.iter_mut().find(|e| e.name == name)
    }

    /// Parse the transport-specific base INI embedded in the package.
    ///
    /// Missing files and keys use caller-owned documented defaults. Recognized
    /// keys with malformed values are rejected rather than silently changing a
    /// low-level boot control.
    pub fn flash_config(&self, transport: &str) -> Result<BundledFlashConfig> {
        let suffix = format!("_{transport}.baseini");
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.name.to_ascii_lowercase().ends_with(&suffix))
        else {
            return Ok(BundledFlashConfig::default());
        };
        let Some(data) = entry.data.as_deref() else {
            return Ok(BundledFlashConfig::default());
        };
        parse_flash_config(data)
            .with_context(|| format!("invalid bundled configuration {}", entry.name))
    }
}

fn parse_flash_config(data: &[u8]) -> Result<BundledFlashConfig> {
    let text = String::from_utf8_lossy(data);
    let mut config = BundledFlashConfig::default();

    for (line_index, line) in text.lines().enumerate() {
        let line = line.split([';', '#']).next().unwrap_or_default().trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("agbaud") {
            let baud = value.parse::<u32>().with_context(|| {
                format!(
                    "line {}: agbaud must be an unsigned integer",
                    line_index + 1
                )
            })?;
            if baud == 0 {
                bail!("line {}: agbaud must be greater than zero", line_index + 1);
            }
            config.agent_baud = Some(baud);
        } else if key.eq_ignore_ascii_case("pullup_qspi") {
            config.pullup_qspi = Some(parse_bool_control(key, value, line_index + 1)?);
        } else if key.eq_ignore_ascii_case("dribble_dld_en") {
            config.dribble_download = Some(parse_bool_control(key, value, line_index + 1)?);
        }
    }

    Ok(config)
}

fn parse_bool_control(key: &str, value: &str, line: usize) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => bail!("line {line}: {key} must be 0 or 1, got {value:?}"),
    }
}

/// Magic string at offset 0x38 identifying the new pkgmode binpkg format.
const PKGMODE_MAGIC: &[u8] = b"pkgmode";

/// Entry metadata size in the binpkg format.
/// struct.unpack("64sIIII256s16sHHII") = 64+4+4+4+4+256+16+2+2+4+4 = 364
const ENTRY_META_SIZE: usize = 364;

/// Parse a binpkg binary blob.
///
/// If `keep_data` is true, each entry's `data` field will contain the image bytes.
pub fn parse_binpkg(fdata: &[u8], keep_data: bool) -> Result<BinpkgResult> {
    let fsize = fdata.len();
    if fsize < 0x34 {
        bail!("binpkg data too small ({} bytes)", fsize);
    }

    let foffset: usize;
    let product_name: String;

    // Detect format: pkgmode vs legacy
    if fsize > 0x3F && &fdata[0x38..0x3F] == PKGMODE_MAGIC {
        // New pkgmode format
        foffset = 0x1D8;
        if fsize < foffset {
            bail!("truncated pkgmode header ({} bytes)", fsize);
        }
        let raw = &fdata[0x190..std::cmp::min(0x1A0, fsize)];
        product_name = raw
            .split(|&b| b == 0)
            .next()
            .map(|s| String::from_utf8_lossy(s).to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| UNKNOWN_PRODUCT.to_string());
    } else {
        // Legacy format: 52-byte header
        foffset = 0x34;
        product_name = UNKNOWN_PRODUCT.to_string();
    }

    let raw_header = fdata[..foffset].to_vec();
    let mut entries = Vec::new();
    let mut cursor = foffset;

    while cursor + ENTRY_META_SIZE <= fsize {
        let meta = &fdata[cursor..cursor + ENTRY_META_SIZE];

        let name_raw = &meta[0..64];
        let name = name_raw
            .split(|&b| b == 0)
            .next()
            .map(|s| String::from_utf8_lossy(s).to_string())
            .unwrap_or_default();

        let addr = u32::from_le_bytes(meta[64..68].try_into().unwrap());
        let flash_size = u32::from_le_bytes(meta[68..72].try_into().unwrap());
        let offset = u32::from_le_bytes(meta[72..76].try_into().unwrap());
        let img_size = u32::from_le_bytes(meta[76..80].try_into().unwrap());

        let hash_raw = &meta[80..336];
        let hash = hash_raw
            .split(|&b| b == 0)
            .next()
            .map(|s| String::from_utf8_lossy(s).to_string().to_lowercase())
            .unwrap_or_default();

        let img_type_raw = &meta[336..352];
        let image_type = img_type_raw
            .split(|&b| b == 0)
            .next()
            .map(|s| String::from_utf8_lossy(s).to_string())
            .unwrap_or_default();

        let vt = u16::from_le_bytes(meta[352..354].try_into().unwrap());
        let vtsize = u16::from_le_bytes(meta[354..356].try_into().unwrap());
        let rsvd = u32::from_le_bytes(meta[356..360].try_into().unwrap());
        let pdata = u32::from_le_bytes(meta[360..364].try_into().unwrap());

        cursor += ENTRY_META_SIZE;

        let data_end = cursor
            .checked_add(img_size as usize)
            .ok_or_else(|| anyhow::anyhow!("entry {name} size overflows the package"))?;
        if data_end > fsize {
            bail!(
                "entry {} extends past the end of the package (end {}, size {})",
                name,
                data_end,
                fsize
            );
        }
        let data = keep_data.then(|| fdata[cursor..data_end].to_vec());

        log::debug!("{}", name);

        entries.push(BinpkgEntry {
            name,
            addr,
            flash_size,
            offset,
            image_size: img_size,
            hash,
            image_type,
            vt,
            vtsize,
            rsvd,
            pdata,
            data,
        });

        cursor = data_end;
    }

    Ok(BinpkgResult {
        product_name,
        raw_header,
        entries,
    })
}

/// Serialize a BinpkgResult back to binary format.
pub fn serialize_binpkg(result: &BinpkgResult) -> Vec<u8> {
    let mut out = result.raw_header.clone();

    for entry in &result.entries {
        // 64-byte name
        let mut name_buf = [0u8; 64];
        let name_bytes = entry.name.as_bytes();
        let copy_len = name_bytes.len().min(63);
        name_buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
        out.extend_from_slice(&name_buf);

        out.extend_from_slice(&entry.addr.to_le_bytes());
        out.extend_from_slice(&entry.flash_size.to_le_bytes());
        out.extend_from_slice(&entry.offset.to_le_bytes());
        out.extend_from_slice(&entry.image_size.to_le_bytes());

        // 256-byte hash
        let mut hash_buf = [0u8; 256];
        let hash_bytes = entry.hash.as_bytes();
        let copy_len = hash_bytes.len().min(255);
        hash_buf[..copy_len].copy_from_slice(&hash_bytes[..copy_len]);
        out.extend_from_slice(&hash_buf);

        // 16-byte image_type
        let mut type_buf = [0u8; 16];
        let type_bytes = entry.image_type.as_bytes();
        let copy_len = type_bytes.len().min(15);
        type_buf[..copy_len].copy_from_slice(&type_bytes[..copy_len]);
        out.extend_from_slice(&type_buf);

        out.extend_from_slice(&entry.vt.to_le_bytes());
        out.extend_from_slice(&entry.vtsize.to_le_bytes());
        out.extend_from_slice(&entry.rsvd.to_le_bytes());
        out.extend_from_slice(&entry.pdata.to_le_bytes());

        // Image data
        if let Some(ref data) = entry.data {
            out.extend_from_slice(data);
        }
    }

    out
}

/// Recalculate the SHA256 hash for an entry's data.
pub fn rehash_entry(entry: &mut BinpkgEntry) {
    if let Some(ref data) = entry.data {
        entry.hash = hex::encode(Sha256::digest(data));
        entry.image_size = data.len() as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_binpkg, serialize_binpkg, BinpkgEntry, BinpkgResult, BundledFlashConfig};

    #[test]
    fn parses_transport_config_case_insensitively() {
        let package = BinpkgResult {
            product_name: "test".to_string(),
            raw_header: vec![0; 0x34],
            entries: vec![entry(
                "board_UsB.BaSeInI",
                "CFG",
                b"[config]\nAgBaud = 460800 ; comment\n[control]\nPULLUP_QSPI=1\nDribble_Dld_En=0 # safe\n",
            )],
        };
        assert_eq!(
            package.flash_config("usb").unwrap(),
            BundledFlashConfig {
                agent_baud: Some(460800),
                pullup_qspi: Some(true),
                dribble_download: Some(false),
            }
        );
    }

    #[test]
    fn parses_pkgmode_product_name() {
        let mut package = vec![0u8; 0x1d8];
        package[0x38..0x3f].copy_from_slice(b"pkgmode");
        package[0x190..0x19a].copy_from_slice(b"YCOM_718PM");

        let parsed = parse_binpkg(&package, false).unwrap();

        assert_eq!(parsed.product_name, "YCOM_718PM");
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn self_contained_package_selects_usb_and_uart_configs() {
        let package = BinpkgResult {
            product_name: "fixture".to_string(),
            raw_header: vec![0; 0x34],
            entries: vec![
                entry(
                    "board_usb.baseini",
                    "CFG",
                    b"agbaud=3000000\npullup_qspi=1\ndribble_dld_en=0\n",
                ),
                entry(
                    "board_uart.baseini",
                    "CFG",
                    b"agbaud=921600\npullup_qspi=0\ndribble_dld_en=1\n",
                ),
                entry("ap.bin", "AP", &[1, 2, 3, 4]),
            ],
        };
        let encoded = serialize_binpkg(&package);
        let parsed = parse_binpkg(&encoded, true).unwrap();

        assert_eq!(
            parsed.flash_config("usb").unwrap(),
            BundledFlashConfig {
                agent_baud: Some(3_000_000),
                pullup_qspi: Some(true),
                dribble_download: Some(false),
            }
        );
        assert_eq!(
            parsed.flash_config("uart").unwrap(),
            BundledFlashConfig {
                agent_baud: Some(921_600),
                pullup_qspi: Some(false),
                dribble_download: Some(true),
            }
        );
        let ap = parsed.find_entry("ap.bin").unwrap();
        assert_eq!(ap.image_type, "AP");
        assert_eq!(ap.image_size, 4);
        assert_eq!(ap.data.as_deref(), Some(&[1, 2, 3, 4][..]));
    }

    #[test]
    fn missing_optional_config_uses_defaults() {
        let package = BinpkgResult {
            product_name: "fixture".to_string(),
            raw_header: vec![0; 0x34],
            entries: vec![entry("ap.bin", "AP", &[1])],
        };
        assert_eq!(
            package.flash_config("uart").unwrap(),
            BundledFlashConfig::default()
        );
    }

    #[test]
    fn malformed_recognized_config_values_are_rejected() {
        let package = BinpkgResult {
            product_name: "fixture".to_string(),
            raw_header: vec![0; 0x34],
            entries: vec![entry(
                "board_uart.baseini",
                "CFG",
                b"dribble_dld_en=maybe\n",
            )],
        };
        let error = package.flash_config("uart").unwrap_err().to_string();
        assert!(error.contains("invalid bundled configuration"));
    }

    fn entry(name: &str, image_type: &str, data: &[u8]) -> BinpkgEntry {
        BinpkgEntry {
            name: name.to_string(),
            addr: 0x0080_0000,
            flash_size: data.len() as u32,
            offset: 0,
            image_size: data.len() as u32,
            hash: String::new(),
            image_type: image_type.to_string(),
            vt: 0,
            vtsize: 0,
            rsvd: 0,
            pdata: 0,
            data: Some(data.to_vec()),
        }
    }
}
