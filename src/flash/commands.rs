use anyhow::{anyhow, bail, Context, Result};
use serialport::SerialPort;
use sha2::{Digest, Sha256};

use super::consts::*;
use super::protocol::*;
use crate::serial::port::{clear_input, com_read_exact, com_write};
use crate::util::checksum::{crc8_maxim, self_def_check1};

fn command_label(command: u8) -> String {
    format!("command 0x{command:02X}")
}

fn encode_checked_length(length: u32) -> u32 {
    if length == 0 {
        return 0;
    }
    let length = length & 0x00ff_ffff;
    let bytes = length.to_le_bytes();
    ((crc8_maxim(&bytes[..3]) as u32) << 24) | length
}

fn validate_request_length(command: u8, declared: u32, data: &[u8]) -> Result<()> {
    let actual = u32::try_from(data.len())
        .with_context(|| format!("{} payload is too large", command_label(command)))?;
    if declared != actual {
        bail!(
            "{} request length field is {}, but payload is {} bytes",
            command_label(command),
            declared,
            actual
        );
    }
    if data.len() > MAX_DATA_BLOCK_SIZE {
        bail!(
            "{} payload is {} bytes, exceeding protocol maximum {}",
            command_label(command),
            data.len(),
            MAX_DATA_BLOCK_SIZE
        );
    }
    Ok(())
}

fn build_cmd_request(cmd: &Cmd, data: &[u8], dlboot: bool) -> Result<Vec<u8>> {
    validate_request_length(cmd.cmd, cmd.len, data)?;

    let mut plain = cmd.pack();
    plain.extend_from_slice(data);

    if dlboot {
        if cmd.cmd == CMD_DOWNLOAD_DATA {
            plain.extend_from_slice(&self_def_check1(
                cmd.cmd,
                cmd.index,
                cmd.order_id,
                cmd.norder_id,
                cmd.len,
                data,
            ));
        }
        return Ok(plain);
    }

    // AgentBoot calculates the request CRC over the unencoded length, then
    // places the CRC8-protected length on the wire.
    let checksum = crc32fast::hash(&plain);
    let mut wire_cmd = cmd.clone();
    wire_cmd.len = encode_checked_length(cmd.len);
    let mut wire = wire_cmd.pack();
    wire.extend_from_slice(data);
    wire.extend_from_slice(&checksum.to_le_bytes());
    Ok(wire)
}

fn build_lpc_request(cmd: &LpcCmd, data: &[u8]) -> Result<Vec<u8>> {
    validate_request_length(cmd.cmd, cmd.len, data)?;

    let mut plain = cmd.pack();
    plain.extend_from_slice(data);
    let checksum = crc32fast::hash(&plain);

    let mut wire_cmd = cmd.clone();
    wire_cmd.len = encode_checked_length(cmd.len);
    let mut wire = wire_cmd.pack();
    wire.extend_from_slice(data);
    wire.extend_from_slice(&checksum.to_le_bytes());
    Ok(wire)
}

fn read_validated_response(
    port: &mut dyn SerialPort,
    request_cmd: u8,
    request_index: u8,
    request_order_id: u8,
    request_norder_id: u8,
    has_crc: bool,
    max_payload_len: usize,
) -> Result<Vec<u8>> {
    let label = command_label(request_cmd);
    let header = com_read_exact(port, FIXED_PROTOCOL_RSP_LEN)
        .with_context(|| format!("{label} response header"))?;
    let response = Rsp::unpack(&header).with_context(|| format!("{label} response header"))?;
    let response_len = response.len as usize;
    if response_len > max_payload_len {
        bail!(
            "{label} response length {} exceeds protocol maximum {}",
            response_len,
            max_payload_len
        );
    }

    let payload = if response_len == 0 {
        Vec::new()
    } else {
        com_read_exact(port, response_len).with_context(|| format!("{label} response payload"))?
    };

    if has_crc {
        let crc_bytes =
            com_read_exact(port, 4).with_context(|| format!("{label} response CRC32"))?;
        let received_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        let mut protected = header.clone();
        protected.extend_from_slice(&payload);
        let calculated_crc = crc32fast::hash(&protected);
        if received_crc != calculated_crc {
            bail!(
                "{label} response CRC32 mismatch: received 0x{received_crc:08X}, calculated 0x{calculated_crc:08X}"
            );
        }
    }

    if response.cmd != request_cmd {
        bail!(
            "{label} response command mismatch: received 0x{:02X}",
            response.cmd
        );
    }
    if response.order_id != request_order_id {
        bail!(
            "{label} response order ID mismatch: received 0x{:02X}, expected 0x{:02X}",
            response.order_id,
            request_order_id
        );
    }
    if response.norder_id != request_norder_id {
        bail!(
            "{label} response inverse order ID mismatch: received 0x{:02X}, expected 0x{:02X}",
            response.norder_id,
            request_norder_id
        );
    }
    let expected_index = if request_cmd == CMD_DOWNLOAD_DATA {
        request_index.wrapping_add(1)
    } else {
        request_index
    };
    if response.index != expected_index {
        bail!(
            "{label} response index mismatch: received {}, expected {}",
            response.index,
            expected_index
        );
    }
    if response.state != 0 {
        bail!(
            "{label} response state is NAK/error {} (payload {})",
            response.state,
            hex::encode(&payload)
        );
    }

    Ok(payload)
}

fn send_recv_cmd_once(
    port: &mut dyn SerialPort,
    cmd: &Cmd,
    data: &[u8],
    dlboot: bool,
) -> Result<Vec<u8>> {
    let request = build_cmd_request(cmd, data, dlboot)?;
    log::debug!("CMD {}", hex::encode(&request));
    com_write(port, &request).with_context(|| command_label(cmd.cmd))?;
    std::thread::sleep(std::time::Duration::from_millis(2));
    read_validated_response(
        port,
        cmd.cmd,
        cmd.index,
        cmd.order_id,
        cmd.norder_id,
        !dlboot,
        MAX_DL_RSP_DATA_LEN,
    )
}

/// Send an AgentBoot/DLBOOT command with the vendor-compatible retry bound.
///
/// The complete wire request is rebuilt from the immutable command and payload
/// on each attempt. This is important because the on-wire length contains a
/// CRC8 byte and must never become the input to a subsequent retry.
pub fn send_recv_cmd(
    port: &mut dyn SerialPort,
    cmd: &Cmd,
    data: &[u8],
    dlboot: bool,
) -> Result<Vec<u8>> {
    // Reject caller-side mistakes once, before any bytes are written.
    validate_request_length(cmd.cmd, cmd.len, data)?;

    let mut last_error = None;
    for attempt in 1..=MAX_COMMAND_ATTEMPTS {
        let result = clear_input(port)
            .with_context(|| {
                format!(
                    "{} attempt {attempt}: clearing stale input",
                    command_label(cmd.cmd)
                )
            })
            .and_then(|()| send_recv_cmd_once(port, cmd, data, dlboot));
        match result {
            Ok(payload) => return Ok(payload),
            Err(error) => {
                log::warn!(
                    "{} attempt {}/{} failed: {:#}",
                    command_label(cmd.cmd),
                    attempt,
                    MAX_COMMAND_ATTEMPTS,
                    error
                );
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("unknown protocol failure"))).with_context(|| {
        format!(
            "{} failed after {} attempts",
            command_label(cmd.cmd),
            MAX_COMMAND_ATTEMPTS
        )
    })
}

fn send_recv_lpc_cmd_once(port: &mut dyn SerialPort, cmd: &LpcCmd, data: &[u8]) -> Result<Vec<u8>> {
    let request = build_lpc_request(cmd, data)?;
    log::debug!("CMD LPC {}", hex::encode(&request));
    com_write(port, &request).with_context(|| command_label(cmd.cmd))?;
    read_validated_response(
        port,
        cmd.cmd,
        cmd.index,
        cmd.order_id,
        cmd.norder_id,
        true,
        MAX_LPC_RSP_DATA_LEN,
    )
}

/// Send an LPC command with the vendor-compatible retry bound.
pub fn send_recv_lpc_cmd(port: &mut dyn SerialPort, cmd: &LpcCmd, data: &[u8]) -> Result<Vec<u8>> {
    validate_request_length(cmd.cmd, cmd.len, data)?;

    let mut last_error = None;
    for attempt in 1..=MAX_COMMAND_ATTEMPTS {
        let result = clear_input(port)
            .with_context(|| {
                format!(
                    "{} attempt {attempt}: clearing stale input",
                    command_label(cmd.cmd)
                )
            })
            .and_then(|()| send_recv_lpc_cmd_once(port, cmd, data));
        match result {
            Ok(payload) => return Ok(payload),
            Err(error) => {
                log::warn!(
                    "LPC {} attempt {}/{} failed: {:#}",
                    command_label(cmd.cmd),
                    attempt,
                    MAX_COMMAND_ATTEMPTS,
                    error
                );
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("unknown LPC protocol failure"))).with_context(|| {
        format!(
            "LPC {} failed after {} attempts",
            command_label(cmd.cmd),
            MAX_COMMAND_ATTEMPTS
        )
    })
}

fn version_diagnostics(data: &[u8]) -> String {
    if data.is_empty() {
        return "empty response".to_string();
    }

    let words = data
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
        .map(|word| format!("0x{word:08X}"))
        .collect::<Vec<_>>();
    let remainder = data.len() % 4;
    if words.is_empty() {
        format!("{} byte(s), hex={}", data.len(), hex::encode(data))
    } else if remainder == 0 {
        format!("{} byte(s), words=[{}]", data.len(), words.join(", "))
    } else {
        format!(
            "{} byte(s), words=[{}], hex={}",
            data.len(),
            words.join(", "),
            hex::encode(data)
        )
    }
}

/// Query and log the bootloader version response without imposing a numeric
/// compatibility policy. The V2.3 host only requires command success.
pub fn package_get_version(port: &mut dyn SerialPort, dlboot: bool) -> Result<Vec<u8>> {
    let cmd = Cmd::new(CMD_GET_VERSION);
    let data = send_recv_cmd(port, &cmd, &[], dlboot)?;
    log::debug!("GET_VERSION: {}", version_diagnostics(&data));
    Ok(data)
}

/// Select image type on device and validate the returned identifier.
pub fn package_sel_image(port: &mut dyn SerialPort, img_type: u32, dlboot: bool) -> Result<()> {
    let cmd = Cmd::new(CMD_SEL_IMAGE);
    let data = send_recv_cmd(port, &cmd, &[], dlboot)?;
    if data.len() != 4 {
        bail!(
            "{} response payload length is {}, expected 4",
            command_label(CMD_SEL_IMAGE),
            data.len()
        );
    }
    let returned = u32::from_le_bytes(data.try_into().unwrap());
    if returned != img_type {
        bail!(
            "{} image identifier mismatch: received 0x{returned:08X}, expected 0x{img_type:08X}",
            command_label(CMD_SEL_IMAGE)
        );
    }
    Ok(())
}

/// Verify image on device.
pub fn package_verify_image(port: &mut dyn SerialPort, dlboot: bool) -> Result<()> {
    let cmd = Cmd::new(CMD_VERIFY_IMAGE);
    let data = send_recv_cmd(port, &cmd, &[], dlboot)?;
    if !data.is_empty() {
        log::debug!("VERIFY_IMAGE response {}", hex::encode(data));
    }
    Ok(())
}

/// Query version, select an image class, and verify it.
pub fn package_base_info(port: &mut dyn SerialPort, img_type: u32, dlboot: bool) -> Result<()> {
    package_get_version(port, dlboot)?;
    package_sel_image(port, img_type, dlboot)?;
    package_verify_image(port, dlboot)
}

fn validate_transfer_size(remaining: u32, transfer_size: u32) -> Result<usize> {
    if transfer_size == 0 {
        bail!("DATA_HEAD negotiated a zero-byte transfer");
    }
    if transfer_size > remaining {
        bail!(
            "DATA_HEAD negotiated {} bytes with only {} bytes remaining",
            transfer_size,
            remaining
        );
    }
    if transfer_size as usize > MAX_DATA_BLOCK_SIZE {
        bail!(
            "DATA_HEAD negotiated {} bytes, exceeding protocol maximum {}",
            transfer_size,
            MAX_DATA_BLOCK_SIZE
        );
    }
    Ok(transfer_size as usize)
}

/// Send DATA_HEAD and validate the negotiated transfer size.
fn package_data_head(port: &mut dyn SerialPort, remaining: u32, dlboot: bool) -> Result<usize> {
    let mut cmd = Cmd::new(CMD_DATA_HEAD);
    cmd.len = 4;
    let data = remaining.to_le_bytes();
    let response = send_recv_cmd(port, &cmd, &data, dlboot)?;
    if response.len() != 4 {
        bail!(
            "{} response payload length is {}, expected 4",
            command_label(CMD_DATA_HEAD),
            response.len()
        );
    }
    let transfer_size = u32::from_le_bytes(response.try_into().unwrap());
    validate_transfer_size(remaining, transfer_size)
}

fn package_data_single(
    port: &mut dyn SerialPort,
    cmd: &Cmd,
    data: &[u8],
    dlboot: bool,
) -> Result<()> {
    send_recv_cmd(port, cmd, data, dlboot)?;
    Ok(())
}

fn package_done(port: &mut dyn SerialPort, dlboot: bool) -> Result<()> {
    let cmd = Cmd::new(CMD_DONE);
    send_recv_cmd(port, &cmd, &[], dlboot)?;
    Ok(())
}

/// Transfer data using DATA_HEAD, one or more DOWNLOAD_DATA packets, then DONE.
pub fn package_data(
    port: &mut dyn SerialPort,
    base_cmd: &Cmd,
    data: &[u8],
    dlboot: bool,
) -> Result<()> {
    if data.is_empty() {
        bail!("{} payload must not be empty", command_label(base_cmd.cmd));
    }
    let total =
        u32::try_from(data.len()).context("transfer payload is larger than the protocol range")?;
    let mut offset = 0usize;
    let mut packet_index = 0u8;

    while offset < data.len() {
        let remaining = total - offset as u32;
        let transfer_size = package_data_head(port, remaining, dlboot)?;
        let end = offset
            .checked_add(transfer_size)
            .filter(|end| *end <= data.len())
            .context("negotiated transfer size exceeds the local payload")?;

        let mut packet = base_cmd.clone();
        packet.index = packet_index;
        packet.len = transfer_size as u32;
        package_data_single(port, &packet, &data[offset..end], dlboot)?;

        offset = end;
        packet_index = packet_index.wrapping_add(1);
    }

    package_done(port, dlboot)
}

/// Build and send a deterministic, zero-initialized image header.
pub fn package_image_head(
    port: &mut dyn SerialPort,
    image: &[u8],
    img_type: BurnImageType,
    addr: u32,
    baud: u32,
    dlboot: bool,
    controls: ImageHeaderControls,
) -> Result<()> {
    let image_size =
        u32::try_from(image.len()).context("image is larger than the protocol size field")?;
    let image_hash: [u8; 32] = Sha256::digest(image).into();

    let mut header = ImgHead::new();
    header.set_body_id(img_type.identifier());
    header.set_img_size(image_size);
    header.set_burn_addr(addr);
    header.set_hashv(&image_hash);
    header.set_baudrate_ctrl(baud);
    header.set_hashtype(0xee);
    header.set_header_controls(controls);
    // `.binpkg` does not carry the complete flash-topology fields required to
    // make this control valid, so it intentionally remains all zeroes.
    header.set_agentboot_control(AgentBootControl::default());
    header.finalize_hash();

    let mut cmd = Cmd::new(CMD_DOWNLOAD_DATA);
    let header_data = header.pack();
    cmd.len = header_data.len() as u32;
    package_data(port, &cmd, header_data, dlboot).context("failed to transfer image header")
}

#[cfg(test)]
mod tests {
    use super::{
        build_cmd_request, send_recv_cmd, send_recv_cmd_once, send_recv_lpc_cmd_once,
        validate_transfer_size, version_diagnostics,
    };
    use crate::flash::consts::{
        CMD_DOWNLOAD_DATA, CMD_GET_VERSION, DL_COMMAND_ID, DL_COMMAND_ID_INV, LPC_COMMAND_ID,
        LPC_COMMAND_ID_INV, MAX_COMMAND_ATTEMPTS, MAX_DATA_BLOCK_SIZE,
    };
    use crate::flash::protocol::{Cmd, LpcCmd};
    use serialport::{
        ClearBuffer, DataBits, Error, ErrorKind, FlowControl, Parity, SerialPort, StopBits,
    };
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::sync::Mutex;
    use std::time::Duration;

    struct ScriptState {
        responses: VecDeque<Option<Vec<u8>>>,
        rx: VecDeque<u8>,
        writes: Vec<Vec<u8>>,
        clear_count: usize,
    }

    struct ScriptedPort {
        fragment_size: usize,
        state: Mutex<ScriptState>,
    }

    impl ScriptedPort {
        fn new(responses: impl IntoIterator<Item = Option<Vec<u8>>>, fragment_size: usize) -> Self {
            Self {
                fragment_size,
                state: Mutex::new(ScriptState {
                    responses: responses.into_iter().collect(),
                    rx: VecDeque::new(),
                    writes: Vec::new(),
                    clear_count: 0,
                }),
            }
        }

        fn with_stale_input(mut self, stale: &[u8]) -> Self {
            self.state.get_mut().unwrap().rx.extend(stale);
            self
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }

        fn clear_count(&self) -> usize {
            self.state.lock().unwrap().clear_count
        }
    }

    impl Read for ScriptedPort {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            if state.rx.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "script timeout"));
            }
            let count = output
                .len()
                .min(self.fragment_size.max(1))
                .min(state.rx.len());
            for byte in output.iter_mut().take(count) {
                *byte = state.rx.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    impl Write for ScriptedPort {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            state.writes.push(input.to_vec());
            if let Some(Some(response)) = state.responses.pop_front() {
                state.rx.extend(response);
            }
            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for ScriptedPort {
        fn name(&self) -> Option<String> {
            Some("scripted".to_string())
        }

        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(921_600)
        }

        fn data_bits(&self) -> serialport::Result<DataBits> {
            Ok(DataBits::Eight)
        }

        fn flow_control(&self) -> serialport::Result<FlowControl> {
            Ok(FlowControl::None)
        }

        fn parity(&self) -> serialport::Result<Parity> {
            Ok(Parity::None)
        }

        fn stop_bits(&self) -> serialport::Result<StopBits> {
            Ok(StopBits::One)
        }

        fn timeout(&self) -> Duration {
            Duration::from_millis(1)
        }

        fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
            Ok(())
        }

        fn set_data_bits(&mut self, _: DataBits) -> serialport::Result<()> {
            Ok(())
        }

        fn set_flow_control(&mut self, _: FlowControl) -> serialport::Result<()> {
            Ok(())
        }

        fn set_parity(&mut self, _: Parity) -> serialport::Result<()> {
            Ok(())
        }

        fn set_stop_bits(&mut self, _: StopBits) -> serialport::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _: Duration) -> serialport::Result<()> {
            Ok(())
        }

        fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }

        fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }

        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(true)
        }

        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(true)
        }

        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }

        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(true)
        }

        fn bytes_to_read(&self) -> serialport::Result<u32> {
            Ok(self.state.lock().unwrap().rx.len() as u32)
        }

        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }

        fn clear(&self, buffer: ClearBuffer) -> serialport::Result<()> {
            let mut state = self.state.lock().unwrap();
            if matches!(buffer, ClearBuffer::Input | ClearBuffer::All) {
                state.rx.clear();
            }
            state.clear_count += 1;
            Ok(())
        }

        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Err(Error::new(ErrorKind::Unknown, "clone is not supported"))
        }

        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }

        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    fn response(
        command: u8,
        index: u8,
        order_id: u8,
        inverse_order_id: u8,
        state: u8,
        payload: &[u8],
        with_crc: bool,
    ) -> Vec<u8> {
        let mut wire = vec![
            command,
            index,
            order_id,
            inverse_order_id,
            state,
            payload.len() as u8,
        ];
        wire.extend_from_slice(payload);
        if with_crc {
            wire.extend_from_slice(&crc32fast::hash(&wire).to_le_bytes());
        }
        wire
    }

    #[test]
    fn rejects_invalid_negotiated_transfer_sizes_without_io() {
        assert!(validate_transfer_size(10, 0)
            .unwrap_err()
            .to_string()
            .contains("zero-byte"));
        assert!(validate_transfer_size(10, 11)
            .unwrap_err()
            .to_string()
            .contains("only 10"));
        assert!(validate_transfer_size(
            (MAX_DATA_BLOCK_SIZE + 1) as u32,
            (MAX_DATA_BLOCK_SIZE + 1) as u32
        )
        .unwrap_err()
        .to_string()
        .contains("protocol maximum"));
        assert_eq!(validate_transfer_size(7, 7).unwrap(), 7);
    }

    #[test]
    fn serializes_representative_dlboot_request() {
        let mut command = Cmd::new(CMD_DOWNLOAD_DATA);
        command.index = 3;
        command.len = 2;
        let request = build_cmd_request(&command, &[0xAA, 0x55], true).unwrap();
        assert_eq!(
            request,
            vec![
                CMD_DOWNLOAD_DATA,
                3,
                DL_COMMAND_ID,
                DL_COMMAND_ID_INV,
                2,
                0,
                0,
                0,
                0xAA,
                0x55,
                0x35,
                0x02,
                0,
                0
            ]
        );
    }

    #[test]
    fn serializes_representative_agentboot_and_lpc_requests() {
        let agentboot = Cmd::new(CMD_GET_VERSION);
        assert_eq!(
            build_cmd_request(&agentboot, &[], false).unwrap(),
            [0x20, 0x00, 0xCD, 0x32, 0x00, 0x00, 0x00, 0x00, 0xB3, 0x5B, 0xC3, 0xEA]
        );

        let mut lpc = LpcCmd::new(0x10);
        lpc.len = 8;
        let mut payload = 0x1000u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&0x0080_0000u32.to_le_bytes());
        assert_eq!(
            super::build_lpc_request(&lpc, &payload).unwrap(),
            [
                0x10, 0x00, 0x4C, 0xB3, 0x08, 0x00, 0x00, 0x25, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00,
                0x80, 0x00, 0x9A, 0xC5, 0x58, 0x21
            ]
        );
    }

    #[test]
    fn formats_version_words_without_enforcing_compatibility() {
        assert_eq!(
            version_diagnostics(&[1, 0, 0, 0, 2, 0, 0, 0]),
            "8 byte(s), words=[0x00000001, 0x00000002]"
        );
    }

    #[test]
    fn exact_reader_accepts_fragmented_response_and_clears_stale_input() {
        let valid = response(
            CMD_GET_VERSION,
            0,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[1, 0, 0, 0],
            true,
        );
        let mut port = ScriptedPort::new([Some(valid)], 1).with_stale_input(&[0xFF, 0xEE]);
        let command = Cmd::new(CMD_GET_VERSION);

        let data = send_recv_cmd(&mut port, &command, &[], false).unwrap();

        assert_eq!(data, [1, 0, 0, 0]);
        assert_eq!(port.clear_count(), 1);
    }

    #[test]
    fn response_crc_mismatch_is_rejected() {
        let mut invalid = response(
            CMD_GET_VERSION,
            0,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[1, 2],
            true,
        );
        *invalid.last_mut().unwrap() ^= 0x80;
        let mut port = ScriptedPort::new([Some(invalid)], 64);
        let command = Cmd::new(CMD_GET_VERSION);

        let error = send_recv_cmd_once(&mut port, &command, &[], false).unwrap_err();

        assert!(error.to_string().contains("CRC32 mismatch"));
    }

    #[test]
    fn oversized_dl_response_is_rejected_before_payload_read() {
        let oversized = response(
            CMD_GET_VERSION,
            0,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[0; 13],
            false,
        );
        let mut port = ScriptedPort::new([Some(oversized)], 64);
        let command = Cmd::new(CMD_GET_VERSION);

        let error = send_recv_cmd_once(&mut port, &command, &[], true).unwrap_err();

        assert!(error.to_string().contains("exceeds protocol maximum 12"));
    }

    #[test]
    fn nak_state_is_rejected_with_command_context() {
        let nak = response(
            CMD_GET_VERSION,
            0,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            1,
            &[0x42],
            false,
        );
        let mut port = ScriptedPort::new([Some(nak)], 64);
        let command = Cmd::new(CMD_GET_VERSION);

        let error = send_recv_cmd_once(&mut port, &command, &[], true).unwrap_err();

        assert!(error.to_string().contains("command 0x20"));
        assert!(error.to_string().contains("NAK/error 1"));
    }

    #[test]
    fn mismatched_command_and_index_are_rejected() {
        let wrong_command = response(
            CMD_DOWNLOAD_DATA,
            0,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[],
            false,
        );
        let mut port = ScriptedPort::new([Some(wrong_command)], 64);
        let command = Cmd::new(CMD_GET_VERSION);
        assert!(send_recv_cmd_once(&mut port, &command, &[], true)
            .unwrap_err()
            .to_string()
            .contains("command mismatch"));

        let wrong_index = response(
            CMD_GET_VERSION,
            1,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[],
            false,
        );
        let mut port = ScriptedPort::new([Some(wrong_index)], 64);
        assert!(send_recv_cmd_once(&mut port, &command, &[], true)
            .unwrap_err()
            .to_string()
            .contains("index mismatch"));
    }

    #[test]
    fn download_data_requires_next_response_index() {
        let mut command = Cmd::new(CMD_DOWNLOAD_DATA);
        command.index = 7;
        command.len = 2;
        let valid = response(
            CMD_DOWNLOAD_DATA,
            8,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[],
            false,
        );
        let mut port = ScriptedPort::new([Some(valid)], 64);
        send_recv_cmd_once(&mut port, &command, &[1, 2], true).unwrap();
    }

    #[test]
    fn timeout_retries_identical_packet_then_succeeds() {
        let valid = response(
            CMD_DOWNLOAD_DATA,
            1,
            DL_COMMAND_ID,
            DL_COMMAND_ID_INV,
            0,
            &[],
            false,
        );
        let mut port = ScriptedPort::new([None, Some(valid)], 64);
        let mut command = Cmd::new(CMD_DOWNLOAD_DATA);
        command.len = 3;

        send_recv_cmd(&mut port, &command, &[1, 2, 3], true).unwrap();

        let writes = port.writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0], writes[1]);
    }

    #[test]
    fn retries_exhaust_after_ten_attempts_with_original_context() {
        let mut port = ScriptedPort::new(vec![None; MAX_COMMAND_ATTEMPTS], 64);
        let command = Cmd::new(CMD_GET_VERSION);

        let error = send_recv_cmd(&mut port, &command, &[], false).unwrap_err();
        let chain = format!("{error:#}");

        assert_eq!(port.writes().len(), MAX_COMMAND_ATTEMPTS);
        assert!(chain.contains("failed after 10 attempts"));
        assert!(chain.contains("timed out"));
    }

    #[test]
    fn local_length_error_is_not_retried() {
        let mut port = ScriptedPort::new([], 64);
        let mut command = Cmd::new(CMD_DOWNLOAD_DATA);
        command.len = 2;

        let error = send_recv_cmd(&mut port, &command, &[1], true).unwrap_err();

        assert!(error.to_string().contains("length field"));
        assert!(port.writes().is_empty());
    }

    #[test]
    fn lpc_response_uses_same_crc_and_framing_validation() {
        let command = LpcCmd::new(0x44);
        let valid = response(
            command.cmd,
            0,
            LPC_COMMAND_ID,
            LPC_COMMAND_ID_INV,
            0,
            &[0, 0, 0, 0],
            true,
        );
        let mut port = ScriptedPort::new([Some(valid)], 2);

        let data = send_recv_lpc_cmd_once(&mut port, &command, &[]).unwrap();

        assert_eq!(data, [0, 0, 0, 0]);
    }
}
