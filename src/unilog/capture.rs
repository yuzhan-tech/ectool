//! UniLog wire framing and record parsing.
//!
//! `parse_records` handles already-unpacked records, each prefixed by a 32-bit
//! little-endian header carrying owner/mod/sub/payLoadLen. `EpatStreamDecoder`
//! handles the packed stream that EPAT receives before those records exist.
//! See debug_trace.h in the ec7xx-csdk for the authoritative record bit layout.

/// Maximum payload length per the UniLog spec (11-bit field).
pub const MAX_PAYLOAD_LEN: usize = 2047;

/// 8-byte marker EPAT writes around each USB read in its `RecvDump/*.bin` files.
/// Our own `--out` captures are the raw stream and contain no such marker.
const EPAT_DUMP_MARKER: [u8; 8] = [0xBA, 0xBA, 0xBA, 0xBA, 0xBA, 0xBA, 0xBA, 0xAA];

/// EPAT's `RecvDump/*.bin` dumps wrap the raw UniLog stream in marker-delimited
/// chunks: `[MARKER][16-byte meta][MARKER][data]` repeated, where each meta block
/// starts with `EA 07`. Strip that framing so the inner stream can be fed to
/// [`EpatStreamDecoder`] exactly like a raw `--out` capture. A buffer that does
/// not begin with the marker is returned unchanged (already raw).
pub fn strip_epat_dump_framing(bytes: &[u8]) -> Vec<u8> {
    if !bytes.starts_with(&EPAT_DUMP_MARKER) {
        return bytes.to_vec();
    }

    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(&EPAT_DUMP_MARKER) {
            i += EPAT_DUMP_MARKER.len();
            continue;
        }
        let next = find_subslice(&bytes[i..], &EPAT_DUMP_MARKER)
            .map(|pos| i + pos)
            .unwrap_or(bytes.len());
        let segment = &bytes[i..next];
        // 16-byte `EA 07 …` blocks are EPAT metadata, not stream data.
        let is_meta = segment.len() == 16 && segment.starts_with(&[0xEA, 0x07]);
        if !is_meta {
            out.extend_from_slice(segment);
        }
        i = next;
    }
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Owner IDs known to be in use. Headers carrying owner IDs outside this set
/// are treated as a resync trigger. Update if the SDK adds new owners.
const KNOWN_OWNERS_MASK: u32 = (1 << 0)  // PHY_ONLINE
    | (1 << 1)                            // PHY_OFFLINE
    | (1 << 2)                            // PLAT_AP
    | (1 << 3)                            // PLAT_CP
    | (1 << 4)                            // PS1
    | (1 << 5)                            // PS2
    | (1 << 6); // CUSTOMER

/// Transport-domain stack that reconstructed a record. This is an EPAT
/// stream-layer detail (the device multiplexes two source domains); it is NOT
/// the UniLog `owner` field. Tracked per record for tooling/future columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Ap,
    Cp,
}

/// Per-record device timestamp recovered from the entry-start word — EPAT's
/// `UE Time` column, displayed as `f1:f2:f3:f4`. See
/// `docs/epat-unilog-re-findings.md` for the bit layout and validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceTime {
    /// 4-bit high block counter.
    pub f1: u8,
    /// 10-bit 16-second-block counter.
    pub f2: u16,
    /// Seconds within the block (0..15).
    pub f3: u8,
    /// Sub-second tick in units of 1/32768 s (always a multiple of 32).
    pub f4: u16,
}

impl DeviceTime {
    /// Decode the timestamp packed above the nest id of an entry-start word.
    fn from_start_word(w: u32) -> Self {
        Self {
            f1: ((w >> 3) & 0xF) as u8,
            f2: ((w >> 7) & 0x3FF) as u16,
            f3: ((w >> 27) & 0xF) as u8,
            f4: (((w >> 17) & 0x3FF) * 32) as u16,
        }
    }

    /// EPAT-style `f1:f2:f3:f4`, fixed 16-character width. The on-wire packing,
    /// validated against EPAT's `UE Time`; kept for a future column view. The
    /// field units (block sizes) are not yet independently confirmed, so no
    /// seconds/uptime conversion is provided here.
    pub fn format_ue(&self) -> String {
        format!(
            "{:02}:{:04}:{:02}:{:05}",
            self.f1, self.f2, self.f3, self.f4
        )
    }
}

#[derive(Debug, Clone)]
pub struct Record {
    /// Full 32-bit header verbatim from the wire.
    pub header: u32,
    /// Payload bytes (length matches payload_len()).
    pub payload: Vec<u8>,
    /// Transport domain that reconstructed this record (`None` for the legacy
    /// `parse_records` path). Not rendered today; kept for future column views.
    pub transport: Option<Transport>,
    /// Device timestamp from the entry-start word (`None` when unavailable).
    pub device_time: Option<DeviceTime>,
}

impl Record {
    /// Construct a record with no transport/timestamp metadata (legacy + tests).
    pub fn new(header: u32, payload: Vec<u8>) -> Self {
        Self {
            header,
            payload,
            transport: None,
            device_time: None,
        }
    }

    pub fn owner_id(&self) -> u8 {
        ((self.header >> 28) & 0xF) as u8
    }
    pub fn mod_id(&self) -> u8 {
        ((self.header >> 21) & 0x7F) as u8
    }
    pub fn sub_id(&self) -> u16 {
        ((self.header >> 11) & 0x3FF) as u16
    }
    pub fn payload_len(&self) -> usize {
        (self.header & 0x7FF) as usize
    }
    /// swLogID used for comdb lookup (header with payLoadLen masked off).
    pub fn sw_log_id_masked(&self) -> u32 {
        self.header & 0xFFFF_F800
    }
}

#[derive(Debug, Clone)]
struct EpatEntry {
    nest_id: u16,
    expected_words: Option<usize>,
    header: u32,
    words: Vec<u32>,
    transport: Transport,
    device_time: DeviceTime,
}

impl EpatEntry {
    fn new(start_word: u32, transport: Transport) -> Self {
        Self {
            nest_id: (start_word & 0x7) as u16,
            expected_words: None,
            header: 0,
            words: Vec::new(),
            transport,
            device_time: DeviceTime::from_start_word(start_word),
        }
    }
}

/// Stateful decoder for the EPAT `0x64`/`0x14` packed UniLog stream.
///
/// EPAT does not receive plain `[u32 header][payload]` records from the USB CDC
/// endpoint. `Communications.dll` first removes filler runs and repacks the
/// stream into 32-bit words; only then does `Cores.dll` see normal UniLog
/// records. This is a focused port of that stream layer.
#[derive(Debug, Default)]
pub struct EpatStreamDecoder {
    state: u8,
    filler_state: u8,
    packet_type: u8,
    bit_count: u8,
    scratch: u32,
    word: u32,
    cp_stack: Vec<EpatEntry>,
    ap_stack: Vec<EpatEntry>,
}

impl EpatStreamDecoder {
    /// Decode a chunk of EPAT stream bytes and return any complete UniLog
    /// records emitted by the stream parser.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Record> {
        let mut out = Vec::new();
        let mut offset = 0;

        while offset < bytes.len() {
            match self.state {
                0 | 1 => {
                    let old_state = self.state;
                    let filler = self.update_filler(bytes[offset]);
                    offset += 1;
                    if filler == 5 || (filler == 0 && old_state == 1) {
                        self.enter_word_stream();
                    }
                }
                3 => {
                    self.state = 2;
                }
                4 => {
                    self.process_current_word(&mut out);
                    self.state = 2;
                }
                _ => {
                    let byte = bytes[offset];
                    offset += 1;

                    if self.update_filler(byte) == 5 {
                        self.enter_word_stream();
                        continue;
                    }

                    let produced_word = self.push_stream_byte(byte);
                    if !produced_word && self.bit_count < 16 {
                        self.packet_type = classify_epat_packet((self.scratch >> 24) as u8);
                        if self.packet_type == 9 {
                            self.state = 1;
                            continue;
                        }
                    }

                    if produced_word && self.packet_type < 9 {
                        self.state = if (6..=8).contains(&self.packet_type) {
                            3
                        } else {
                            4
                        };
                    }
                }
            }
        }

        out
    }

    /// Flush a completed top-level record when decoding a finite capture file.
    /// Live serial capture normally emits records when the next top-level start
    /// marker arrives, so callers do not need this inside the read loop.
    pub fn flush(&mut self) -> Vec<Record> {
        let mut out = Vec::new();
        flush_stack(&mut self.cp_stack, &mut out);
        flush_stack(&mut self.ap_stack, &mut out);
        out
    }

    fn enter_word_stream(&mut self) {
        self.state = 2;
        self.bit_count = 0;
        self.scratch = 0;
    }

    fn update_filler(&mut self, byte: u8) -> u8 {
        let mut next = u8::from((byte & 0x0f) == 0x0f);
        match self.filler_state {
            0 => {}
            4 => {
                next = match byte {
                    0xfe => 5,
                    0xff => 4,
                    _ => 0,
                };
            }
            5 => {}
            state => {
                if byte == 0xff {
                    next = state.saturating_add(1);
                }
            }
        }
        self.filler_state = next;
        next
    }

    fn push_stream_byte(&mut self, byte: u8) -> bool {
        if self.bit_count == 0 {
            self.scratch = (byte as u32) << 24;
            self.bit_count = 8;
            return false;
        }

        if self.bit_count <= 24 {
            self.scratch |= (byte as u32) << (24 - self.bit_count);
            self.bit_count += 8;
            return false;
        }

        let remaining = self.bit_count - 26;
        self.word = (self.scratch << 2) | ((byte as u32) >> remaining);
        self.scratch = if remaining == 0 {
            0
        } else {
            (byte as u32) << (32 - remaining)
        };
        self.bit_count = remaining;
        true
    }

    fn process_current_word(&mut self, out: &mut Vec<Record>) {
        let source_is_ap = !matches!(self.packet_type, 0..=2);
        let transport = if source_is_ap {
            Transport::Ap
        } else {
            Transport::Cp
        };
        let stack = if source_is_ap {
            &mut self.ap_stack
        } else {
            &mut self.cp_stack
        };

        match self.packet_type {
            1 | 4 => start_entry(stack, self.word, transport, out),
            0 | 3 => append_entry_word(stack, self.word),
            2 | 5 => close_entry(stack, self.word, out),
            _ => {}
        }
    }
}

fn start_entry(stack: &mut Vec<EpatEntry>, word: u32, transport: Transport, out: &mut Vec<Record>) {
    if stack.len() >= 64 {
        stack.clear();
    }

    if let Some(top) = stack.last() {
        let diff = word.wrapping_sub(top.nest_id as u32) & 0x7;
        if diff == 0 {
            if top.nest_id == 0 {
                if let Some(entry) = stack.pop() {
                    emit_entry(entry, out);
                }
            } else {
                stack.clear();
            }
        } else if diff != 1 {
            stack.clear();
        }
    }

    stack.push(EpatEntry::new(word, transport));
}

fn append_entry_word(stack: &mut [EpatEntry], word: u32) {
    let Some(entry) = stack.last_mut() else {
        return;
    };

    match entry.expected_words {
        None => {
            entry.header = word;
            entry.expected_words = Some(((word & 0x7ff) as usize).div_ceil(4));
        }
        Some(expected) if entry.words.len() < expected => {
            entry.words.push(word);
        }
        Some(_) => {}
    }
}

fn close_entry(stack: &mut Vec<EpatEntry>, word: u32, out: &mut Vec<Record>) {
    let Some(top) = stack.last() else {
        return;
    };
    if (word & 0x7) as u16 == top.nest_id {
        if let Some(entry) = stack.pop() {
            emit_entry(entry, out);
        }
    }
}

fn flush_stack(stack: &mut Vec<EpatEntry>, out: &mut Vec<Record>) {
    if stack.len() != 1 {
        return;
    }

    let complete = stack
        .last()
        .and_then(|entry| {
            entry
                .expected_words
                .map(|expected| entry.words.len() >= expected)
        })
        .unwrap_or(false);
    if complete {
        if let Some(entry) = stack.pop() {
            emit_entry(entry, out);
        }
    }
}

fn emit_entry(entry: EpatEntry, out: &mut Vec<Record>) {
    let Some(expected_words) = entry.expected_words else {
        return;
    };
    if entry.words.len() < expected_words || !is_plausible_header(entry.header) {
        return;
    }

    let payload_len = (entry.header & 0x7ff) as usize;
    let mut payload = Vec::with_capacity(expected_words * 4);
    for word in entry.words.into_iter().take(expected_words) {
        payload.extend_from_slice(&word.to_le_bytes());
    }
    payload.truncate(payload_len);

    out.push(Record {
        header: entry.header,
        payload,
        transport: Some(entry.transport),
        device_time: Some(entry.device_time),
    });
}

fn classify_epat_packet(byte: u8) -> u8 {
    let top = byte & 0xc0;
    if top == 0x00 {
        return 0;
    }
    if top == 0x80 {
        return 3;
    }
    if top == 0xc0 {
        return 9;
    }

    if (byte & 0xe0) == 0x40 {
        if byte == 0x5e {
            return 2;
        }
        if byte == 0x5f {
            return 5;
        }
        if (byte & 0xf8) == 0x58 {
            return if ((byte & 0x07) * 2) > 9 { 10 } else { 6 };
        }
        return if (byte & 0x1e) > 0x12 { 10 } else { 1 };
    }

    if (byte & 0xfe) == 0x7e {
        return 8;
    }
    if (byte & 0xf8) == 0x78 {
        return if ((byte & 0x07) * 2) > 9 { 10 } else { 7 };
    }
    if (byte & 0x1e) > 0x12 {
        10
    } else {
        4
    }
}

fn is_plausible_header(header: u32) -> bool {
    let owner = (header >> 28) & 0xF;
    (KNOWN_OWNERS_MASK & (1u32 << owner)) != 0
}

/// Parse as many complete records as possible from `bytes`. Returns the
/// records plus the number of bytes consumed; the caller should retain the
/// remaining `bytes[consumed..]` for the next read.
///
/// Resync: if a header is implausible (unknown owner), or if the claimed
/// payload exceeds twice the total input buffer (a safeguard for small/synthetic
/// streams — see inline comment), shift forward 1 byte and try again.
pub fn parse_records(bytes: &[u8]) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut offset = 0;

    while offset + 4 <= bytes.len() {
        let header = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);

        if !is_plausible_header(header) {
            offset += 1;
            continue;
        }

        let pay_len = (header & 0x7FF) as usize;
        if offset + 4 + pay_len > bytes.len() {
            // Two cases:
            //   (a) Genuine partial — the header is real and the rest of the
            //       payload hasn't been read yet.
            //   (b) False positive — stray bytes happened to decode as a plausible
            //       header (valid owner ID) but the claimed payLoadLen is bogus.
            //
            // Safeguard: if the claimed payload is more than 2× the entire
            // remaining buffer, we're almost certainly in case (b). Resync by 1
            // byte rather than waiting forever for bytes that may never come.
            //
            // In normal operation src/unilog/mod.rs feeds a residual buffer of
            // several KB while pay_len is capped at 2047 (the 11-bit field), so
            // this branch rarely fires in production. It exists primarily to
            // keep small/synthetic streams (and the corresponding tests) from
            // stalling on a bogus header.
            if pay_len > 2 * bytes.len() {
                offset += 1;
                continue;
            }
            break;
        }

        let payload = bytes[offset + 4..offset + 4 + pay_len].to_vec();
        records.push(Record::new(header, payload));
        offset += 4 + pay_len;
    }

    (records, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_time_decodes_known_start_word() {
        // 0x83ea1d80 is the entry-start word for EPAT CSV row 0, whose UE Time is
        // 00:0059:00:16032 (see docs/epat-unilog-re-findings.md).
        let t = DeviceTime::from_start_word(0x83ea_1d80);
        assert_eq!((t.f1, t.f2, t.f3, t.f4), (0, 59, 0, 16032));
        assert_eq!(t.format_ue(), "00:0059:00:16032");
    }

    #[test]
    fn device_time_decodes_block_rollover_word() {
        // 0x881c1d80: f3=1 second, f2=59, f4=448 -> EPAT "00:0059:01:00448".
        let t = DeviceTime::from_start_word(0x881c_1d80);
        assert_eq!((t.f1, t.f2, t.f3, t.f4), (0, 59, 1, 448));
        assert_eq!(t.format_ue(), "00:0059:01:00448");
    }
}
