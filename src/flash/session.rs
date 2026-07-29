//! High-level ownership of an active EigenComm download session.

use anyhow::{bail, Context, Result};
use serialport::SerialPort;

use super::burn::{
    burn_agboot, burn_img, erase_flash_range_with_progress, read_memory_range, sys_reset,
    ImageTarget, ImageTransferOptions,
};
use super::consts::SyncType;
use super::sync::burn_sync;
use crate::package::binpkg::BinpkgResult;
use crate::serial::port::PortType;

/// Caller-provided AgentBoot image and its explicit boot controls.
#[derive(Debug, Clone, Copy)]
pub struct AgentBootConfig<'a> {
    /// AgentBoot bytes selected and validated by the caller.
    pub data: &'a [u8],
    /// Baud requested in the AgentBoot image header.
    pub baud: u32,
    /// Compatibility control placed in the AgentBoot image header.
    pub pullup_qspi: bool,
}

/// Transfer controls retained for every image written by a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferConfig {
    /// Explicitly selected USB or UART download transport.
    pub port_type: PortType,
    /// Whether image headers enable vendor dribble-download controls.
    pub dribble_download: bool,
}

/// Optional caller overrides used when resolving generic package transfer
/// settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferOverrides {
    /// Explicit AgentBoot baud, if supplied by the caller.
    pub agent_baud: Option<u32>,
    /// Explicit QSPI pull-up override.
    pub pullup_qspi: Option<bool>,
    /// Explicit dribble-download override.
    pub dribble_download: Option<bool>,
}

/// Fully resolved boot and transfer settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTransferConfig {
    /// Final AgentBoot baud after applying precedence.
    pub agent_baud: u32,
    /// Final QSPI pull-up control.
    pub pullup_qspi: bool,
    /// Final per-image transfer controls.
    pub transfer: TransferConfig,
}

/// Resolve transport-specific bundled settings and caller overrides.
///
/// Precedence is caller override, matching bundled `_usb.baseini` or
/// `_uart.baseini`, then the generic compatibility defaults. Only the
/// explicitly selected transport is inspected. Recognized malformed bundled
/// values are returned as errors.
pub fn resolve_transfer_config(
    package: Option<&BinpkgResult>,
    port_type: PortType,
    overrides: TransferOverrides,
) -> Result<ResolvedTransferConfig> {
    let transport = match port_type {
        PortType::Usb => "usb",
        PortType::Uart => "uart",
    };
    let bundled = package
        .map(|package| package.flash_config(transport))
        .transpose()?
        .unwrap_or_default();

    let agent_baud = overrides
        .agent_baud
        .or(bundled.agent_baud)
        .unwrap_or(921_600);
    if agent_baud == 0 {
        bail!("AgentBoot baud must be greater than zero");
    }

    Ok(ResolvedTransferConfig {
        agent_baud,
        pullup_qspi: overrides
            .pullup_qspi
            .or(bundled.pullup_qspi)
            .unwrap_or(true),
        transfer: TransferConfig {
            port_type,
            dribble_download: overrides
                .dribble_download
                .or(bundled.dribble_download)
                .unwrap_or(false),
        },
    })
}

/// An active AgentBoot session over a caller-selected serial port.
///
/// The session performs no serial I/O when dropped. Call [`Self::finish_reset`]
/// explicitly after successful work. Failed diagnostic reads and erases
/// attempt a recovery reset while retaining the original operation error.
pub struct FlashSession {
    port: Box<dyn SerialPort>,
    transfer: TransferConfig,
}

impl FlashSession {
    /// Synchronize with DLBOOT, load caller-provided AgentBoot bytes, and switch
    /// a UART transport to the negotiated AgentBoot baud.
    pub fn start(
        mut port: Box<dyn SerialPort>,
        agent: AgentBootConfig<'_>,
        transfer: TransferConfig,
    ) -> Result<Self> {
        if agent.data.is_empty() {
            bail!("AgentBoot data must not be empty");
        }
        u32::try_from(agent.data.len()).context("AgentBoot image is too large")?;
        if agent.baud == 0 {
            bail!("AgentBoot baud must be greater than zero");
        }

        burn_sync(port.as_mut(), SyncType::DlBoot, 2)
            .context("initial DLBOOT synchronization failed")?;
        let ret = burn_agboot(port.as_mut(), agent.data, agent.baud, agent.pullup_qspi)
            .context("AgentBoot transfer failed")?;
        if ret != 0 {
            bail!("AgentBoot transfer returned device code {ret}");
        }

        if transfer.port_type == PortType::Uart {
            port.set_baud_rate(agent.baud).with_context(|| {
                format!("failed to switch UART to AgentBoot baud {}", agent.baud)
            })?;
        }

        Ok(Self { port, transfer })
    }

    /// Flash one caller-defined image target.
    pub fn flash_image(
        &mut self,
        target: ImageTarget<'_>,
        data: &[u8],
        progress: Option<&mut dyn FnMut(u64, u64)>,
    ) -> Result<()> {
        let tag = target.tag.to_string();
        let ret = burn_img(
            self.port.as_mut(),
            data,
            target,
            ImageTransferOptions {
                port_type: self.transfer.port_type,
                dribble_download: self.transfer.dribble_download,
            },
            progress,
        )
        .with_context(|| format!("failed to flash {tag}"))?;
        if ret != 0 {
            bail!("{tag} flash returned device code {ret}");
        }
        Ok(())
    }

    /// Erase a raw AP-flash range.
    ///
    /// A failed operation triggers one recovery reset attempt. A successful
    /// operation leaves the session active so the caller can explicitly reset
    /// with [`Self::finish_reset`].
    pub fn erase(&mut self, address: u32, size: u32) -> Result<()> {
        self.erase_with_progress(address, size, None)
    }

    /// Erase a raw AP-flash range with caller-owned progress reporting.
    pub fn erase_with_progress(
        &mut self,
        address: u32,
        size: u32,
        progress: Option<&mut dyn FnMut(u64, u64)>,
    ) -> Result<()> {
        let operation =
            erase_flash_range_with_progress(self.port.as_mut(), address, size, "range", progress)
                .and_then(|ret| {
                    if ret != 0 {
                        bail!("erase returned device code {ret}");
                    }
                    Ok(())
                });
        self.recover_failed_diagnostic(operation, "erase")
    }

    /// Read a raw memory range.
    ///
    /// A failed operation triggers one recovery reset attempt. A successful
    /// operation leaves the session active so the caller can explicitly reset
    /// with [`Self::finish_reset`].
    pub fn read(&mut self, address: u32, size: u32) -> Result<Vec<u8>> {
        let operation = read_memory_range(self.port.as_mut(), address, size);
        self.recover_failed_diagnostic(operation, "read")
    }

    /// Explicitly reset the device after successful work and consume the
    /// session.
    pub fn finish_reset(mut self) -> Result<()> {
        reset_port(self.port.as_mut()).context("final device reset failed")
    }

    fn recover_failed_diagnostic<T>(&mut self, operation: Result<T>, label: &str) -> Result<T> {
        let operation_error = match operation {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        match reset_port(self.port.as_mut()) {
            Ok(()) => Err(operation_error)
                .with_context(|| format!("{label} failed; device reset after the failure")),
            Err(reset_error) => Err(operation_error).with_context(|| {
                format!("{label} failed; the recovery device reset also failed: {reset_error:#}")
            }),
        }
    }
}

fn reset_port(port: &mut dyn SerialPort) -> Result<()> {
    let ret = sys_reset(port)?;
    if ret != 0 {
        bail!("device reset returned device code {ret}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::consts::{
        AGBT_IDENTIFIER, AIMG_IDENTIFIER, CMD_DATA_HEAD, CMD_DONE, CMD_DOWNLOAD_DATA,
        CMD_GET_VERSION, CMD_SEL_IMAGE, CMD_VERIFY_IMAGE, DLBOOT_HANDSHAKE, IMGH_IDENTIFIER,
        LPC_BURN_ONE, LPC_FLASH_ERASE, LPC_GET_BURN_STATUS, LPC_HANDSHAKE, LPC_READ_MEM,
        LPC_SYS_RST,
    };
    use crate::package::binpkg::{BinpkgEntry, BinpkgResult};
    use serialport::{ClearBuffer, DataBits, Error, ErrorKind, FlowControl, Parity, StopBits};
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct MockState {
        rx: VecDeque<u8>,
        tx: Vec<u8>,
        pending_handshake: Option<u32>,
        handshakes: Vec<u32>,
        commands: Vec<u8>,
        lpc_commands: Vec<u8>,
        selected_images: VecDeque<u32>,
        dlboot_mode: bool,
        dlboot_done_count: usize,
        burn_status_ok: bool,
        reset_acks: VecDeque<bool>,
        reset_count: usize,
        baud_changes: Vec<u32>,
    }

    struct ProtocolMockPort {
        state: Arc<Mutex<MockState>>,
    }

    impl ProtocolMockPort {
        fn new(
            selected_images: impl IntoIterator<Item = u32>,
            burn_status_ok: bool,
            reset_acks: impl IntoIterator<Item = bool>,
        ) -> (Self, Arc<Mutex<MockState>>) {
            let state = Arc::new(Mutex::new(MockState {
                selected_images: selected_images.into_iter().collect(),
                dlboot_mode: true,
                burn_status_ok,
                reset_acks: reset_acks.into_iter().collect(),
                ..MockState::default()
            }));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl Read for ProtocolMockPort {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();
            if state.rx.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "mock timeout"));
            }
            let count = output.len().min(state.rx.len());
            for byte in output.iter_mut().take(count) {
                *byte = state.rx.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    impl Write for ProtocolMockPort {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            let mut state = self.state.lock().unwrap();

            if state.tx.is_empty() && input.len() == 4 {
                let value = u32::from_le_bytes(input.try_into().unwrap());
                if matches!(
                    value,
                    crate::flash::consts::DLBOOT_HANDSHAKE
                        | crate::flash::consts::AGBOOT_HANDSHAKE
                        | crate::flash::consts::LPC_HANDSHAKE
                ) {
                    state.handshakes.push(value);
                    if state.pending_handshake == Some(value) {
                        state.rx.extend(value.to_le_bytes());
                        state.pending_handshake = None;
                    } else {
                        state.pending_handshake = Some(value);
                    }
                    return Ok(input.len());
                }
            }

            state.tx.extend_from_slice(input);
            loop {
                if state.tx.len() < 8 {
                    break;
                }
                let command = state.tx[0];
                let wire_length = u32::from_le_bytes(state.tx[4..8].try_into().unwrap());
                let payload_length = (wire_length & 0x00ff_ffff) as usize;
                let request_is_dlboot = state.dlboot_mode;
                let trailer_length = if request_is_dlboot {
                    usize::from(command == CMD_DOWNLOAD_DATA) * 4
                } else {
                    4
                };
                let frame_length = 8 + payload_length + trailer_length;
                if state.tx.len() < frame_length {
                    break;
                }

                let frame = state.tx.drain(..frame_length).collect::<Vec<_>>();
                respond_to_frame(&mut state, &frame, request_is_dlboot);
            }

            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn respond_to_frame(state: &mut MockState, frame: &[u8], dlboot: bool) {
        let command = frame[0];
        let index = frame[1];
        let order_id = frame[2];
        let inverse_order_id = frame[3];
        let payload_length =
            (u32::from_le_bytes(frame[4..8].try_into().unwrap()) & 0x00ff_ffff) as usize;
        let payload = &frame[8..8 + payload_length];
        state.commands.push(command);
        if order_id == crate::flash::consts::LPC_COMMAND_ID {
            state.lpc_commands.push(command);
        }

        let response_payload = if order_id == crate::flash::consts::LPC_COMMAND_ID {
            match command {
                LPC_BURN_ONE | LPC_FLASH_ERASE => Vec::new(),
                LPC_GET_BURN_STATUS => {
                    if state.burn_status_ok {
                        vec![0, 0, 0, 0]
                    } else {
                        vec![1, 0, 0, 0]
                    }
                }
                LPC_READ_MEM => {
                    let size = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
                    let address = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                    (0..size)
                        .map(|offset| address.wrapping_add(offset as u32) as u8)
                        .collect()
                }
                LPC_SYS_RST => {
                    state.reset_count += 1;
                    if state.reset_acks.pop_front().unwrap_or(true) {
                        b"ZzZzZzZz".to_vec()
                    } else {
                        b"badreset".to_vec()
                    }
                }
                other => panic!("unexpected mock LPC command 0x{other:02X}"),
            }
        } else {
            match command {
                CMD_GET_VERSION => vec![1, 0, 0, 0],
                CMD_SEL_IMAGE => state
                    .selected_images
                    .pop_front()
                    .expect("mock SEL_IMAGE response was not configured")
                    .to_le_bytes()
                    .to_vec(),
                CMD_VERIFY_IMAGE | CMD_DOWNLOAD_DATA => Vec::new(),
                CMD_DATA_HEAD => payload.to_vec(),
                CMD_DONE => {
                    if dlboot {
                        state.dlboot_done_count += 1;
                        if state.dlboot_done_count == 2 {
                            state.dlboot_mode = false;
                        }
                    }
                    Vec::new()
                }
                other => panic!("unexpected mock command 0x{other:02X}"),
            }
        };

        let response_index = if command == CMD_DOWNLOAD_DATA {
            index.wrapping_add(1)
        } else {
            index
        };
        let mut response = vec![
            command,
            response_index,
            order_id,
            inverse_order_id,
            0,
            response_payload.len() as u8,
        ];
        response.extend_from_slice(&response_payload);
        if !dlboot {
            response.extend_from_slice(&crc32fast::hash(&response).to_le_bytes());
        }
        state.rx.extend(response);
    }

    impl SerialPort for ProtocolMockPort {
        fn name(&self) -> Option<String> {
            Some("protocol-mock".to_string())
        }

        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(115_200)
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

        fn set_baud_rate(&mut self, baud_rate: u32) -> serialport::Result<()> {
            self.state.lock().unwrap().baud_changes.push(baud_rate);
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
            if matches!(buffer, ClearBuffer::Input | ClearBuffer::All) {
                self.state.lock().unwrap().rx.clear();
            }
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

    fn start_mock(
        port_type: PortType,
        additional_selected_images: impl IntoIterator<Item = u32>,
        burn_status_ok: bool,
        reset_acks: impl IntoIterator<Item = bool>,
    ) -> (FlashSession, Arc<Mutex<MockState>>) {
        let selected_images = [IMGH_IDENTIFIER, AGBT_IDENTIFIER]
            .into_iter()
            .chain(additional_selected_images);
        let (port, state) = ProtocolMockPort::new(selected_images, burn_status_ok, reset_acks);
        let session = FlashSession::start(
            Box::new(port),
            AgentBootConfig {
                data: &[0x11, 0x22, 0x33, 0x44],
                baud: 460_800,
                pullup_qspi: true,
            },
            TransferConfig {
                port_type,
                dribble_download: false,
            },
        )
        .unwrap();
        (session, state)
    }

    fn entry(name: &str, data: &[u8]) -> BinpkgEntry {
        BinpkgEntry {
            name: name.to_string(),
            addr: 0,
            flash_size: data.len() as u32,
            offset: 0,
            image_size: data.len() as u32,
            hash: String::new(),
            image_type: "CFG".to_string(),
            vt: 0,
            vtsize: 0,
            rsvd: 0,
            pdata: 0,
            data: Some(data.to_vec()),
        }
    }

    fn package(entries: Vec<BinpkgEntry>) -> BinpkgResult {
        BinpkgResult {
            product_name: "fixture".to_string(),
            raw_header: vec![0; 0x34],
            entries,
        }
    }

    #[test]
    fn transfer_settings_use_override_then_transport_bundle_then_defaults() {
        let package = package(vec![
            entry(
                "board_usb.baseini",
                b"agbaud=3000000\npullup_qspi=0\ndribble_dld_en=1\n",
            ),
            entry(
                "board_uart.baseini",
                b"agbaud=460800\npullup_qspi=1\ndribble_dld_en=0\n",
            ),
        ]);

        let usb = resolve_transfer_config(
            Some(&package),
            PortType::Usb,
            TransferOverrides {
                agent_baud: Some(115_200),
                pullup_qspi: None,
                dribble_download: Some(false),
            },
        )
        .unwrap();
        assert_eq!(
            usb,
            ResolvedTransferConfig {
                agent_baud: 115_200,
                pullup_qspi: false,
                transfer: TransferConfig {
                    port_type: PortType::Usb,
                    dribble_download: false,
                },
            }
        );

        let uart =
            resolve_transfer_config(Some(&package), PortType::Uart, TransferOverrides::default())
                .unwrap();
        assert_eq!(uart.agent_baud, 460_800);
        assert!(uart.pullup_qspi);
        assert!(!uart.transfer.dribble_download);

        let defaults =
            resolve_transfer_config(None, PortType::Usb, TransferOverrides::default()).unwrap();
        assert_eq!(defaults.agent_baud, 921_600);
        assert!(defaults.pullup_qspi);
        assert!(!defaults.transfer.dribble_download);
    }

    #[test]
    fn malformed_matching_configuration_fails() {
        let package = package(vec![entry("board_uart.baseini", b"agbaud=not-a-number\n")]);

        let error =
            resolve_transfer_config(Some(&package), PortType::Uart, TransferOverrides::default())
                .unwrap_err();
        assert!(format!("{error:#}").contains("agbaud"));
    }

    #[test]
    fn start_runs_dlboot_agentboot_sequence_and_switches_uart_baud() {
        let (session, state) =
            start_mock(PortType::Uart, std::iter::empty(), true, std::iter::empty());

        {
            let state = state.lock().unwrap();
            assert_eq!(&state.handshakes[..2], [DLBOOT_HANDSHAKE, DLBOOT_HANDSHAKE]);
            assert_eq!(state.dlboot_done_count, 2);
            assert_eq!(state.baud_changes, [460_800]);
            assert!(state.commands.starts_with(&[
                CMD_GET_VERSION,
                CMD_SEL_IMAGE,
                CMD_VERIFY_IMAGE,
                CMD_DATA_HEAD,
            ]));
            assert_eq!(state.reset_count, 0);
        }

        drop(session);
        assert_eq!(state.lock().unwrap().reset_count, 0);
    }

    #[test]
    fn flash_image_reports_progress_and_converts_device_status_to_error() {
        let (mut success, success_state) = start_mock(
            PortType::Usb,
            [IMGH_IDENTIFIER, AIMG_IDENTIFIER],
            true,
            [true],
        );
        let target = ImageTarget {
            image_type: crate::flash::burn::ImageKind::Ap,
            storage: crate::flash::burn::FlashStorage::ApFlash,
            address: 0x0008_2000,
            tag: "AP",
        };
        let mut events = Vec::new();
        let mut progress = |completed, total| events.push((completed, total));

        success
            .flash_image(target, &[1, 2, 3, 4], Some(&mut progress))
            .unwrap();
        success.finish_reset().unwrap();

        assert_eq!(events.last(), Some(&(4, 4)));
        assert_eq!(success_state.lock().unwrap().reset_count, 1);

        let (mut failed, _) = start_mock(
            PortType::Usb,
            [IMGH_IDENTIFIER, AIMG_IDENTIFIER],
            false,
            std::iter::empty(),
        );
        let error = failed.flash_image(target, &[1, 2, 3, 4], None).unwrap_err();
        assert!(format!("{error:#}").contains("device code -1"));
    }

    #[test]
    fn read_and_erase_remain_active_until_explicit_reset() {
        let (mut session, state) = start_mock(PortType::Usb, std::iter::empty(), true, [true]);
        let data = session.read(0x0000_1234, 241).unwrap();
        let mut erase_events = Vec::new();
        let mut progress = |completed, total| erase_events.push((completed, total));
        session
            .erase_with_progress(0x0000_2000, 0x401, Some(&mut progress))
            .unwrap();

        assert_eq!(data.len(), 241);
        assert_eq!(data[0], 0x34);
        assert_eq!(erase_events, [(0x400, 0x401), (0x401, 0x401)]);
        assert_eq!(state.lock().unwrap().reset_count, 0);

        session.finish_reset().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.reset_count, 1);
        assert_eq!(
            state
                .lpc_commands
                .iter()
                .filter(|command| **command == LPC_READ_MEM)
                .count(),
            2
        );
        assert_eq!(
            state
                .lpc_commands
                .iter()
                .filter(|command| **command == LPC_FLASH_ERASE)
                .count(),
            2
        );
        assert!(state.handshakes.contains(&LPC_HANDSHAKE));
    }

    #[test]
    fn failed_diagnostic_preserves_error_after_recovery_reset() {
        let (mut session, state) = start_mock(PortType::Usb, std::iter::empty(), true, [true]);

        let error = session.erase(0, 0).unwrap_err();
        let chain = format!("{error:#}");

        assert!(chain.contains("erase failed; device reset after the failure"));
        assert!(chain.contains("erase size must be greater than zero"));
        assert_eq!(state.lock().unwrap().reset_count, 1);
    }

    #[test]
    fn failed_diagnostic_reports_failed_recovery_without_hiding_original() {
        let (mut session, state) = start_mock(PortType::Usb, std::iter::empty(), true, [false]);

        let error = session.read(0, 0).unwrap_err();
        let chain = format!("{error:#}");

        assert!(chain.contains("read failed; the recovery device reset also failed"));
        assert!(chain.contains("read size must be greater than zero"));
        assert!(chain.contains("device code -1"));
        assert_eq!(state.lock().unwrap().reset_count, 1);
    }
}
