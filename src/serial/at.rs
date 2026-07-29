use anyhow::{Context, Result};
use std::time::Duration;

pub const ENTER_DOWNLOAD_COMMAND: &[u8] = b"AT+ECRST=delay,99\r\n";

/// Ask a running EigenComm firmware to reboot into its download handshake.
///
/// The AT port is deliberately never auto-detected. Callers must supply the
/// exact port that should receive the command.
pub fn enter_download_mode(port_name: &str, baud: u32) -> Result<()> {
    let mut port = serialport::new(port_name, baud)
        .timeout(Duration::from_millis(800))
        .open()
        .with_context(|| format!("Failed to open AT port {port_name}"))?;

    port.write_all(ENTER_DOWNLOAD_COMMAND)
        .with_context(|| format!("Failed to send AT+ECRST on {port_name}"))?;
    port.flush()
        .with_context(|| format!("Failed to flush AT port {port_name}"))?;

    log::info!(
        "Sent {} on {}",
        String::from_utf8_lossy(ENTER_DOWNLOAD_COMMAND).trim(),
        port_name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ENTER_DOWNLOAD_COMMAND;

    #[test]
    fn uses_the_ec_download_reset_command() {
        assert_eq!(ENTER_DOWNLOAD_COMMAND, b"AT+ECRST=delay,99\r\n");
    }
}
