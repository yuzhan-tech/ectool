use anyhow::Result;
use serialport::SerialPort;

use super::commands::send_recv_lpc_cmd;
use super::consts::*;
use super::protocol::LpcCmd;

/// LPC burn one: tell the agent to prepare for a specific image type.
pub fn lpc_burn_one(
    port: &mut dyn SerialPort,
    img_type: BurnImageType,
    stor_type: u8,
) -> Result<i32> {
    let mut cmd = LpcCmd::new(LPC_BURN_ONE);
    let img_id = img_type.identifier();

    let data = if stor_type == STYPE_CP_FLASH {
        cmd.len = 6;
        let mut d = img_id.to_le_bytes().to_vec();
        d.extend_from_slice(&CP_FLASH_MARKER.to_le_bytes());
        d
    } else {
        cmd.len = 4;
        img_id.to_le_bytes().to_vec()
    };

    log::debug!("lpc burn one {:?} len={}", img_type, cmd.len);
    send_recv_lpc_cmd(port, &cmd, &data)?;
    log::debug!("lpc_burn_one success");
    Ok(0)
}

/// LPC get burn status: check if the previous burn completed successfully.
/// Expects response data of b'\0\0\0\0'.
pub fn lpc_get_burn_status(port: &mut dyn SerialPort) -> Result<i32> {
    let cmd = LpcCmd::new(LPC_GET_BURN_STATUS);
    let data = send_recv_lpc_cmd(port, &cmd, &[])?;
    log::debug!("lpc_get_burn_status response {}", hex::encode(&data));
    if data == LPC_BURN_STATUS_OK {
        return Ok(0);
    }
    Ok(-1)
}

/// LPC flash erase at a given address and size.
pub fn lpc_flash_erase(port: &mut dyn SerialPort, addr: u32, size: u32) -> Result<i32> {
    let mut cmd = LpcCmd::new(LPC_FLASH_ERASE);
    cmd.len = 8;
    let mut data = size.to_le_bytes().to_vec();
    data.extend_from_slice(&addr.to_le_bytes());
    send_recv_lpc_cmd(port, &cmd, &data)?;
    Ok(0)
}

/// Read at most 240 bytes from an address through the LPC agent.
pub fn lpc_read_mem(port: &mut dyn SerialPort, addr: u32, size: u32) -> Result<Vec<u8>> {
    if size as usize > MAX_READ_MEM_SIZE {
        anyhow::bail!(
            "LPC read size {} exceeds protocol maximum {}",
            size,
            MAX_READ_MEM_SIZE
        );
    }

    let mut cmd = LpcCmd::new(LPC_READ_MEM);
    cmd.len = 8;
    let mut data = size.to_le_bytes().to_vec();
    data.extend_from_slice(&addr.to_le_bytes());
    let response = send_recv_lpc_cmd(port, &cmd, &data)?;
    if response.len() != size as usize {
        anyhow::bail!(
            "LPC read at 0x{addr:08X} returned {} bytes, expected {}",
            response.len(),
            size
        );
    }
    Ok(response)
}

/// LPC system reset. Expects response data of b'ZzZzZzZz'.
pub fn lpc_sys_reset(port: &mut dyn SerialPort) -> Result<i32> {
    let cmd = LpcCmd::new(LPC_SYS_RST);
    let data = send_recv_lpc_cmd(port, &cmd, &[])?;
    log::debug!("lpc_sys_reset response {}", hex::encode(&data));
    if data == LPC_SYS_RESET_ACK {
        return Ok(0);
    }
    Ok(-1)
}
