use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use serialport::SerialPort;

use super::commands::*;
use super::consts::*;
use super::lpc::*;
use super::protocol::{Cmd, ImageHeaderControls};
use super::sync::burn_sync;
use crate::serial::port::PortType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageTransferOptions {
    pub port_type: PortType,
    pub dribble_download: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ImageTarget<'a> {
    pub image_type: BurnImageType,
    pub storage_type: u8,
    pub address: u32,
    pub tag: &'a str,
}

impl ImageTransferOptions {
    fn is_usb(self) -> bool {
        self.port_type == PortType::Usb
    }
}

/// Download agent boot to device.
///
/// Sequence: select/verify HEAD -> image_head(AGBOOT) -> DLBOOT sync ->
/// select/verify BL -> download_data.
pub fn burn_agboot(
    port: &mut dyn SerialPort,
    agent_data: &[u8],
    baud: u32,
    pullup_qspi: bool,
) -> Result<i32> {
    log::info!("Burn agent boot start");

    package_base_info(port, BurnImageType::Head.identifier(), true)
        .context("Agent boot base_info(HEAD) failed")?;

    log::debug!("agentboot file size {}", agent_data.len());
    package_image_head(
        port,
        agent_data,
        BurnImageType::AgBoot,
        0,
        baud,
        true,
        ImageHeaderControls {
            pullup_qspi,
            ..ImageHeaderControls::default()
        },
    )
    .context("Agent boot image_head failed")?;

    burn_sync(port, SyncType::DlBoot, 2)?;

    package_base_info(port, BurnImageType::Bootloader.identifier(), true)
        .context("Agent boot base_info(BL) failed")?;

    let mut cmd = Cmd::new(CMD_DOWNLOAD_DATA);
    cmd.len = agent_data.len() as u32;
    package_data(port, &cmd, agent_data, true).context("Agent boot data download failed")?;

    log::info!("Agent boot download complete");
    Ok(0)
}

/// Burn a single image partition.
///
/// Sequence: LPC sync -> lpc_burn_one -> AGBOOT sync x2 -> base_info ->
///           image_head -> (loop: AGBOOT sync + 64KB chunks) -> lpc_get_burn_status
pub fn burn_img(
    port: &mut dyn SerialPort,
    data: &[u8],
    target: ImageTarget<'_>,
    options: ImageTransferOptions,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<i32> {
    let ImageTarget {
        image_type,
        storage_type,
        address,
        tag,
    } = target;
    if data.is_empty() {
        bail!("image data must not be empty for {tag}");
    }
    let image_size =
        u32::try_from(data.len()).with_context(|| format!("image is too large for {tag}"))?;
    address
        .checked_add(image_size - 1)
        .with_context(|| format!("image range overflows 32-bit address space for {tag}"))?;

    log::info!(
        "burn image {} {:?} stor={} addr={:08X}",
        tag,
        image_type,
        storage_type,
        address
    );

    // 1. LPC Sync
    burn_sync(port, SyncType::Lpc, 2)?;

    // 2. LPC burn one
    let ret = lpc_burn_one(port, image_type, storage_type)?;
    if ret != 0 {
        bail!("lpc_burn_one failed for {}", tag);
    }

    // 3. AGBOOT Sync x2
    burn_sync(port, SyncType::AgBoot, 2)?;
    burn_sync(port, SyncType::AgBoot, 2)?;

    // 4. Base info
    package_base_info(port, BurnImageType::Head.identifier(), false)
        .with_context(|| format!("package_base_info failed for {tag}"))?;

    // 5. Image header
    let controls = ImageHeaderControls::for_transfer(
        false,
        options.dribble_download,
        address,
        options.is_usb(),
    );
    package_image_head(port, data, image_type, address, 0, false, controls)
        .with_context(|| format!("package_image_head failed for {tag}"))?;

    // 6. Data transfer in 64KB blocks
    let mut remain = data.len();
    let mut data_offset: usize = 0;
    let mut first_block = true;

    let total = data.len() as u64;
    let pb = if progress.is_none() {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(&format!(
                    "  {{bar:40.cyan/blue}} {{percent:>3}}% {{pos:>7}}/{{len:7}} {}",
                    tag
                ))
                .unwrap()
                .progress_chars("##-"),
        );
        Some(pb)
    } else {
        None
    };

    log::debug!("start send file data ...");
    while remain > 0 {
        burn_sync(port, SyncType::AgBoot, 2)?;
        package_base_info(port, image_type.identifier(), false)
            .with_context(|| format!("block base_info failed for {tag}"))?;

        let current_addr = address
            .checked_add(data_offset as u32)
            .with_context(|| format!("image address overflow for {tag}"))?;
        let data_len = outer_block_len(
            remain,
            current_addr,
            options.port_type == PortType::Uart,
            first_block,
        );

        let mut cmd = Cmd::new(CMD_DOWNLOAD_DATA);
        cmd.len = data_len as u32;
        if let Err(error) = package_data(
            port,
            &cmd,
            &data[data_offset..data_offset + data_len],
            false,
        ) {
            if let Some(ref pb) = pb {
                pb.abandon_with_message(format!("{} FAILED", tag));
            }
            return Err(error).with_context(|| format!("package_data failed for {tag}"));
        }

        data_offset += data_len;
        remain -= data_len;
        first_block = false;
        if let Some(ref mut cb) = progress {
            cb(data_offset as u64, total);
        }
        if let Some(ref pb) = pb {
            pb.set_position(data_offset as u64);
        }
    }

    log::debug!("almost done burn_img");
    let ret = lpc_get_burn_status(port)?;
    if let Some(pb) = pb {
        pb.finish_with_message(format!("{} done", tag));
    }

    Ok(ret)
}

fn outer_block_len(remaining: usize, current_addr: u32, is_uart: bool, first_block: bool) -> usize {
    if is_uart && first_block {
        let offset = current_addr as usize % MAX_DATA_BLOCK_SIZE;
        if offset != 0 {
            let to_boundary = MAX_DATA_BLOCK_SIZE - offset;
            if remaining > to_boundary {
                return to_boundary;
            }
        }
    }
    remaining.min(MAX_DATA_BLOCK_SIZE)
}

/// Erase an AP flash range through the LPC agent.
///
/// The public erase address is the raw non-XIP flash address used by
/// FlashToolCLI's `flasherase` command. The LPC command itself expects the AP
/// flash/XIP address, so raw addresses below 0x800000 are biased before being
/// sent. This implementation deliberately retains its existing 1 KiB request
/// size until 64 KiB vendor-size requests are verified on both transports.
/// Taking `min(remaining, 1 KiB)` guarantees the final request never extends
/// outside the user-requested range.
pub fn erase_flash_range(
    port: &mut dyn SerialPort,
    addr: u32,
    size: u32,
    tag: &str,
) -> Result<i32> {
    if size == 0 {
        bail!("erase size must be greater than zero for {}", tag);
    }

    let mut lpc_addr = if addr < 0x800000 {
        addr + 0x800000
    } else {
        addr
    };
    let mut remain = size;

    log::info!(
        "erase {} raw=0x{:X} lpc=0x{:X} size=0x{:X}",
        tag,
        addr,
        lpc_addr,
        size
    );

    burn_sync(port, SyncType::Lpc, 2)?;

    let pb = ProgressBar::new(size as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "  {{bar:40.cyan/blue}} {{percent:>3}}% {{pos:>7}}/{{len:7}} erase {}",
                tag
            ))
            .unwrap()
            .progress_chars("##-"),
    );

    while remain > 0 {
        let chunk = remain.min(0x400);
        let ret = lpc_flash_erase(port, lpc_addr, chunk)?;
        if ret != 0 {
            pb.abandon_with_message(format!("erase {} FAILED", tag));
            return Ok(ret);
        }
        lpc_addr = lpc_addr.saturating_add(chunk);
        remain -= chunk;
        pb.set_position((size - remain) as u64);
    }

    pb.finish_with_message(format!("erase {} done", tag));
    Ok(0)
}

/// Read an arbitrary range in protocol-sized chunks.
pub fn read_memory_range(port: &mut dyn SerialPort, addr: u32, size: u32) -> Result<Vec<u8>> {
    if size == 0 {
        bail!("read size must be greater than zero");
    }

    burn_sync(port, SyncType::Lpc, 2)?;
    let mut output = Vec::with_capacity(size as usize);
    let mut offset = 0u32;

    while offset < size {
        let chunk = (size - offset).min(MAX_READ_MEM_SIZE as u32);
        output.extend_from_slice(&lpc_read_mem(port, addr.saturating_add(offset), chunk)?);
        offset += chunk;
    }

    Ok(output)
}

/// Reset the device via LPC command.
pub fn sys_reset(port: &mut dyn SerialPort) -> Result<i32> {
    burn_sync(port, SyncType::Lpc, 2)?;
    let ret = lpc_sys_reset(port)?;
    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::outer_block_len;
    use crate::flash::consts::MAX_DATA_BLOCK_SIZE;

    #[test]
    fn usb_blocks_ignore_address_alignment() {
        assert_eq!(
            outer_block_len(MAX_DATA_BLOCK_SIZE * 2, 0x1234, false, true),
            MAX_DATA_BLOCK_SIZE
        );
    }

    #[test]
    fn uart_first_block_ends_at_next_boundary() {
        assert_eq!(
            outer_block_len(MAX_DATA_BLOCK_SIZE, 0x8000, true, true),
            0x8000
        );
        assert_eq!(
            outer_block_len(MAX_DATA_BLOCK_SIZE * 2, 0x1234, true, true),
            0xEDCC
        );
    }

    #[test]
    fn aligned_and_zero_uart_addresses_use_full_blocks() {
        assert_eq!(
            outer_block_len(MAX_DATA_BLOCK_SIZE * 2, 0, true, true),
            MAX_DATA_BLOCK_SIZE
        );
        assert_eq!(
            outer_block_len(MAX_DATA_BLOCK_SIZE * 2, 0x20000, true, true),
            MAX_DATA_BLOCK_SIZE
        );
    }

    #[test]
    fn short_uart_images_are_not_padded_or_split() {
        assert_eq!(outer_block_len(0x1000, 0x1234, true, true), 0x1000);
    }

    #[test]
    fn only_the_first_uart_block_gets_special_alignment() {
        assert_eq!(
            outer_block_len(MAX_DATA_BLOCK_SIZE * 2, 0x1234, true, false),
            MAX_DATA_BLOCK_SIZE
        );
    }

    #[test]
    fn uart_images_crossing_multiple_boundaries_use_normal_later_blocks() {
        let first = outer_block_len(MAX_DATA_BLOCK_SIZE * 3, 0x1234, true, true);
        let second = outer_block_len(
            MAX_DATA_BLOCK_SIZE * 3 - first,
            0x1234 + first as u32,
            true,
            false,
        );
        assert_eq!(first, 0xEDCC);
        assert_eq!(second, MAX_DATA_BLOCK_SIZE);
    }
}
