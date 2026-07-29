mod cli;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{Cli, Commands, DownloadArgs, Transport};
use ectool::flash::burn::{
    burn_agboot, burn_img, erase_flash_range, read_memory_range, sys_reset, ImageTarget,
    ImageTransferOptions,
};
use ectool::flash::consts::{BurnImageType, SyncType, STYPE_AP_FLASH, STYPE_CP_FLASH};
use ectool::flash::sync::burn_sync;
use ectool::package::binpkg::{parse_binpkg, BinpkgEntry, BinpkgResult, BundledFlashConfig};
use ectool::serial::detect::{find_download_port_now, wait_for_download_port, DownloadPort};
use ectool::serial::port::{open_port, PortType};
use std::fs;
use std::path::Path;
use std::time::Duration;

fn parse_u32(value: &str, label: &str) -> Result<u32> {
    let value = value.trim();
    let (digits, radix) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map(|digits| (digits, 16))
        .unwrap_or((value, 10));
    u32::from_str_radix(digits, radix).with_context(|| format!("Invalid {label} value: {value}"))
}

fn port_type(transport: Transport) -> PortType {
    match transport {
        Transport::Usb => PortType::Usb,
        Transport::Uart => PortType::Uart,
    }
}

fn transport_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Usb => "usb",
        Transport::Uart => "uart",
    }
}

fn resolve_agent_baud(explicit: Option<u32>, bundled: BundledFlashConfig) -> u32 {
    explicit.or(bundled.agent_baud).unwrap_or(921_600)
}

fn load_package(path: &Path) -> Result<BinpkgResult> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| !extension.eq_ignore_ascii_case("binpkg"))
        .unwrap_or(true)
    {
        bail!("flash accepts an EigenComm .binpkg file");
    }
    let data = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    parse_binpkg(&data, true).with_context(|| format!("Failed to parse {}", path.display()))
}

fn prepare_download_port(
    args: &DownloadArgs,
    at_port: Option<(&str, u32)>,
) -> Result<DownloadPort> {
    if matches!(args.transport, Transport::Uart) && args.port == "auto" {
        bail!("UART download ports are never auto-detected; specify --port");
    }

    let existing = find_download_port_now(&args.port)?;
    let verified_usb_boot = existing
        .as_ref()
        .map(|port| port.verified_usb_download)
        .unwrap_or(false);

    if !verified_usb_boot {
        if let Some((at_port, at_baud)) = at_port {
            ectool::serial::at::enter_download_mode(at_port, at_baud)?;
        } else if existing.is_none() {
            log::info!(
                "Waiting for download mode. Use the BOOT pin, or pass --at-port to send AT+ECRST=delay,99"
            );
        }
    }

    wait_for_download_port(&args.port, Duration::from_secs(args.wait))
}

fn start_agent(
    args: &DownloadArgs,
    at_port: Option<(&str, u32)>,
    package: Option<&BinpkgResult>,
) -> Result<(Box<dyn serialport::SerialPort>, BundledFlashConfig)> {
    let agent = fs::read(&args.agentboot)
        .with_context(|| format!("Failed to read agentboot {}", args.agentboot.display()))?;
    if agent.is_empty() {
        bail!("agentboot {} is empty", args.agentboot.display());
    }

    let download_port = prepare_download_port(args, at_port)?;

    log::info!("Using download port {}", download_port.name);
    let mut port = open_port(&download_port.name, port_type(args.transport))?;
    burn_sync(port.as_mut(), SyncType::DlBoot, 2)?;

    let bundled_config = package
        .map(|package| package.flash_config(transport_name(args.transport)))
        .transpose()?
        .unwrap_or_default();
    let agent_baud = resolve_agent_baud(args.agent_baud, bundled_config);
    // Existing ectool behavior asserts pullup_qspi in the AgentBoot header.
    // A transport-specific bundled baseini may explicitly override it.
    let pullup_qspi = bundled_config.pullup_qspi.unwrap_or(true);
    log::info!(
        "Loading agentboot {} at {} baud",
        args.agentboot.display(),
        agent_baud
    );
    burn_agboot(port.as_mut(), &agent, agent_baud, pullup_qspi)?;
    if matches!(args.transport, Transport::Uart) {
        port.set_baud_rate(agent_baud)
            .with_context(|| format!("Failed to switch UART to agent baud {agent_baud}"))?;
    }
    Ok((port, bundled_config))
}

fn selected(only: &[String], name: &str) -> bool {
    only.is_empty() || only.iter().any(|item| item.eq_ignore_ascii_case(name))
}

fn ap_flash_offset(address: u32) -> u32 {
    if address >= 0x800000 {
        address - 0x800000
    } else {
        address
    }
}

fn validate_only(only: &[String]) -> Result<()> {
    for item in only {
        if !["bl", "ap", "cp"]
            .iter()
            .any(|allowed| item.eq_ignore_ascii_case(allowed))
        {
            bail!("Unknown --only image class {item:?}; expected bl, ap, or cp");
        }
    }
    Ok(())
}

fn package_entry_target(
    entry: &BinpkgEntry,
    only: &[String],
    is_ec7xx: bool,
) -> Option<(BurnImageType, u8, u32, &'static str)> {
    match entry.image_type.to_ascii_uppercase().as_str() {
        "BL" if selected(only, "bl") => Some((BurnImageType::Bootloader, STYPE_AP_FLASH, 0, "BL")),
        "AP" if selected(only, "ap") => Some((
            BurnImageType::Ap,
            STYPE_AP_FLASH,
            ap_flash_offset(entry.addr),
            "AP",
        )),
        "CP" if selected(only, "cp") && is_ec7xx => Some((
            BurnImageType::Cp,
            STYPE_AP_FLASH,
            ap_flash_offset(entry.addr),
            "CP",
        )),
        "CP" if selected(only, "cp") => Some((BurnImageType::Cp, STYPE_CP_FLASH, 0, "CP")),
        _ => None,
    }
}

fn flash_package(
    file: &Path,
    args: &DownloadArgs,
    at_port: Option<(&str, u32)>,
    only: &[String],
) -> Result<()> {
    validate_only(only)?;
    let package = load_package(file)?;
    log::info!("Package product: {}", package.product_name);
    let (mut port, bundled_config) = start_agent(args, at_port, Some(&package))?;
    let transfer_options = ImageTransferOptions {
        port_type: port_type(args.transport),
        // Disabled is the safe fallback used when the optional key is absent.
        dribble_download: bundled_config.dribble_download.unwrap_or(false),
    };
    let is_ec7xx = {
        let product = package.product_name.trim().to_ascii_uppercase();
        product.contains("EC7") || product.contains("YCOM_7")
    };
    let mut flashed = 0usize;

    for entry in &package.entries {
        let Some(data) = entry.data.as_deref() else {
            continue;
        };
        let result = package_entry_target(entry, only, is_ec7xx);

        if let Some((kind, storage, address, tag)) = result {
            let ret = burn_img(
                port.as_mut(),
                data,
                ImageTarget {
                    image_type: kind,
                    storage_type: storage,
                    address,
                    tag,
                },
                transfer_options,
                None,
            )?;
            if ret != 0 {
                bail!("Flashing {} failed ({})", entry.name, ret);
            }
            flashed += 1;
        }
    }

    if flashed == 0 {
        bail!(
            "No selected BL/AP/CP images were present in {}",
            file.display()
        );
    }

    let ret = sys_reset(port.as_mut())?;
    if ret != 0 {
        bail!("Images were written, but the final device reset failed ({ret})");
    }
    log::info!("Flash complete: {} image(s)", flashed);
    Ok(())
}

fn complete_diagnostic<T>(
    port: &mut dyn serialport::SerialPort,
    operation: Result<T>,
    label: &str,
) -> Result<T> {
    let reset = sys_reset(port);
    match (operation, reset) {
        (Ok(value), Ok(0)) => Ok(value),
        (Ok(_), Ok(ret)) => bail!("{label} completed, but the final device reset failed ({ret})"),
        (Ok(_), Err(reset_error)) => Err(reset_error)
            .with_context(|| format!("{label} completed, but the final device reset failed")),
        (Err(operation_error), Ok(0)) => Err(operation_error)
            .with_context(|| format!("{label} failed; device reset after the failure")),
        (Err(operation_error), Ok(ret)) => Err(operation_error).with_context(|| {
            format!("{label} failed; the recovery device reset also failed ({ret})")
        }),
        (Err(operation_error), Err(reset_error)) => Err(operation_error).with_context(|| {
            format!("{label} failed; the recovery device reset also failed: {reset_error:#}")
        }),
    }
}

fn erase(address: &str, size: &str, args: &DownloadArgs) -> Result<()> {
    let address = parse_u32(address, "address")?;
    let size = parse_u32(size, "size")?;
    let (mut port, _) = start_agent(args, None, None)?;
    let operation = erase_flash_range(port.as_mut(), address, size, "range").and_then(|ret| {
        if ret != 0 {
            bail!("Erase failed ({ret})");
        }
        Ok(())
    });
    complete_diagnostic(port.as_mut(), operation, "Erase")
}

fn read(address: &str, size: &str, output: &Path, args: &DownloadArgs) -> Result<()> {
    let address = parse_u32(address, "address")?;
    let size = parse_u32(size, "size")?;
    let (mut port, _) = start_agent(args, None, None)?;
    let operation = (|| {
        let data = read_memory_range(port.as_mut(), address, size)?;
        fs::write(output, data).with_context(|| format!("Failed to write {}", output.display()))?;
        log::info!("Wrote {} bytes to {}", size, output.display());
        Ok(())
    })();
    complete_diagnostic(port.as_mut(), operation, "Read")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let log_level = if cli.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .format_target(false)
        .format_timestamp(None)
        .init();

    match cli.command {
        Commands::Flash {
            file,
            download,
            at_port,
            at_baud,
            only,
        } => flash_package(
            &file,
            &download,
            at_port.as_deref().map(|port| (port, at_baud)),
            &only,
        ),
        Commands::Erase {
            address,
            size,
            download,
        } => erase(&address, &size, &download),
        Commands::Read {
            address,
            size,
            output,
            download,
        } => read(&address, &size, &output, &download),
        Commands::Unilog {
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
        } => ectool::unilog::run(ectool::unilog::UnilogArgs {
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
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ap_flash_addresses_accept_xip_and_raw_forms() {
        assert_eq!(ap_flash_offset(0x882000), 0x082000);
        assert_eq!(ap_flash_offset(0x082000), 0x082000);
    }

    #[test]
    fn agent_baud_precedence_is_explicit_then_bundled_then_default() {
        assert_eq!(
            resolve_agent_baud(
                Some(115_200),
                BundledFlashConfig {
                    agent_baud: Some(460_800),
                    ..BundledFlashConfig::default()
                }
            ),
            115_200
        );
        assert_eq!(
            resolve_agent_baud(
                None,
                BundledFlashConfig {
                    agent_baud: Some(460_800),
                    ..BundledFlashConfig::default()
                }
            ),
            460_800
        );
        assert_eq!(
            resolve_agent_baud(None, BundledFlashConfig::default()),
            921_600
        );
    }

    #[test]
    fn package_metadata_drives_image_address_and_storage() {
        let ap = fixture_entry("AP", 0x0088_2000, 123);
        assert_eq!(
            package_entry_target(&ap, &[], true),
            Some((BurnImageType::Ap, STYPE_AP_FLASH, 0x0008_2000, "AP"))
        );

        let cp = fixture_entry("CP", 0x0089_0000, 456);
        assert_eq!(
            package_entry_target(&cp, &[], true),
            Some((BurnImageType::Cp, STYPE_AP_FLASH, 0x0009_0000, "CP"))
        );
        assert_eq!(
            package_entry_target(&cp, &[], false),
            Some((BurnImageType::Cp, STYPE_CP_FLASH, 0, "CP"))
        );
        assert_eq!(cp.image_size, 456);
    }

    fn fixture_entry(image_type: &str, address: u32, size: u32) -> BinpkgEntry {
        BinpkgEntry {
            name: format!("{image_type}.bin"),
            addr: address,
            flash_size: size,
            offset: 0,
            image_size: size,
            hash: String::new(),
            image_type: image_type.to_string(),
            vt: 0,
            vtsize: 0,
            rsvd: 0,
            pdata: 0,
            data: Some(vec![0; size as usize]),
        }
    }
}
