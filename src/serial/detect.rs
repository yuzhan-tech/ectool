use anyhow::{bail, Context, Result};
use serialport::{SerialPortInfo, SerialPortType};
use std::time::{Duration, Instant};

/// EigenComm's USB download port. This is the only VID/PID ectool
/// auto-detects.
pub const DOWNLOAD_VID: u16 = 0x17D1;
pub const DOWNLOAD_PID: u16 = 0x0001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPort {
    pub name: String,
    /// True only when the enumerated port matched 17D1:0001.
    pub verified_usb_download: bool,
}

fn as_download_port(info: &SerialPortInfo) -> Option<DownloadPort> {
    match &info.port_type {
        SerialPortType::UsbPort(usb) if usb.vid == DOWNLOAD_VID && usb.pid == DOWNLOAD_PID => {
            Some(DownloadPort {
                name: info.port_name.clone(),
                verified_usb_download: true,
            })
        }
        _ => None,
    }
}

fn select_auto_download_port(ports: &[SerialPortInfo]) -> Result<Option<DownloadPort>> {
    let mut matches = ports
        .iter()
        .filter_map(as_download_port)
        .collect::<Vec<_>>();

    // macOS publishes one USB CDC interface under paired callout and dial-in
    // paths. Prefer /dev/cu.* and do not count its /dev/tty.* alias as another
    // physical download device.
    let callout_suffixes = matches
        .iter()
        .filter_map(|port| port.name.strip_prefix("/dev/cu."))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    matches.retain(|port| {
        port.name
            .strip_prefix("/dev/tty.")
            .map(|suffix| !callout_suffixes.iter().any(|callout| callout == suffix))
            .unwrap_or(true)
    });

    match matches.as_slice() {
        [] => Ok(None),
        [port] => Ok(Some(port.clone())),
        _ => bail!(
            "Multiple EigenComm download ports ({:04X}:{:04X}) found: {}. \
             Specify one with --port",
            DOWNLOAD_VID,
            DOWNLOAD_PID,
            matches
                .iter()
                .map(|port| port.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve a port without waiting.
///
/// `auto` considers only 17D1:0001. An explicit port is accepted as supplied.
pub fn find_download_port_now(requested: &str) -> Result<Option<DownloadPort>> {
    let ports = serialport::available_ports().context("Failed to list serial ports")?;
    if requested == "auto" {
        return select_auto_download_port(&ports);
    }

    Ok(ports
        .iter()
        .find(|port| port.port_name == requested)
        .map(|port| DownloadPort {
            name: requested.to_string(),
            verified_usb_download: as_download_port(port).is_some(),
        }))
}

/// Wait for a selected download port.
///
/// Automatic selection uses only 17D1:0001. Explicit names wait for that exact
/// OS port and never fall back to VID/PID discovery.
pub fn wait_for_download_port(requested: &str, timeout: Duration) -> Result<DownloadPort> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(port) = find_download_port_now(requested)? {
            return Ok(port);
        }
        if Instant::now() >= deadline {
            if requested == "auto" {
                bail!(
                    "Timed out waiting for EigenComm download port {:04X}:{:04X}",
                    DOWNLOAD_VID,
                    DOWNLOAD_PID
                );
            }
            bail!("Timed out waiting for download port {requested}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_port(name: &str, vid: u16, pid: u16) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: SerialPortType::UsbPort(serialport::UsbPortInfo {
                vid,
                pid,
                serial_number: None,
                manufacturer: None,
                product: None,
                interface: None,
            }),
        }
    }

    #[test]
    fn auto_selection_accepts_only_the_download_vid_pid() {
        let ports = vec![
            usb_port("runtime", 0x1234, 0x5678),
            usb_port("boot", DOWNLOAD_VID, DOWNLOAD_PID),
            usb_port("other", 0x0403, 0x6001),
        ];

        let selected = select_auto_download_port(&ports).unwrap().unwrap();
        assert_eq!(selected.name, "boot");
    }

    #[test]
    fn multiple_download_ports_require_explicit_selection() {
        let ports = vec![
            usb_port("boot0", DOWNLOAD_VID, DOWNLOAD_PID),
            usb_port("boot1", DOWNLOAD_VID, DOWNLOAD_PID),
        ];

        assert!(select_auto_download_port(&ports)
            .unwrap_err()
            .to_string()
            .contains("--port"));
    }

    #[test]
    fn macos_callout_and_dialin_paths_are_one_port() {
        let ports = vec![
            usb_port("/dev/cu.usbmodem0000000000011", DOWNLOAD_VID, DOWNLOAD_PID),
            usb_port("/dev/tty.usbmodem0000000000011", DOWNLOAD_VID, DOWNLOAD_PID),
        ];

        let selected = select_auto_download_port(&ports).unwrap().unwrap();

        assert_eq!(selected.name, "/dev/cu.usbmodem0000000000011");
    }
}
