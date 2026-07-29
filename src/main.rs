mod cli;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::{Cli, Commands, DownloadArgs, Transport};
use ectool::{
    find_download_port_now, open_port, parse_binpkg, plan_binpkg_images, resolve_transfer_config,
    wait_for_download_port, AgentBootConfig, BinpkgResult, DownloadPort, FlashSession,
    PackageSelection, PortType, TransferOverrides,
};
use indicatif::{ProgressBar, ProgressStyle};
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

fn start_session(
    args: &DownloadArgs,
    at_port: Option<(&str, u32)>,
    package: Option<&BinpkgResult>,
) -> Result<FlashSession> {
    let resolved = resolve_transfer_config(
        package,
        port_type(args.transport),
        TransferOverrides {
            agent_baud: args.agent_baud,
            ..TransferOverrides::default()
        },
    )?;
    let agent = fs::read(&args.agentboot)
        .with_context(|| format!("Failed to read agentboot {}", args.agentboot.display()))?;
    if agent.is_empty() {
        bail!("agentboot {} is empty", args.agentboot.display());
    }

    let download_port = prepare_download_port(args, at_port)?;

    log::info!("Using download port {}", download_port.name);
    let port = open_port(&download_port.name, port_type(args.transport))?;
    log::info!(
        "Loading agentboot {} at {} baud",
        args.agentboot.display(),
        resolved.agent_baud
    );
    FlashSession::start(
        port,
        AgentBootConfig {
            data: &agent,
            baud: resolved.agent_baud,
            pullup_qspi: resolved.pullup_qspi,
        },
        resolved.transfer,
    )
}

fn package_selection(only: &[String]) -> Result<PackageSelection> {
    if only.is_empty() {
        return Ok(PackageSelection::all());
    }

    let mut selection = PackageSelection {
        bootloader: false,
        ap: false,
        cp: false,
    };
    for item in only {
        match item.trim().to_ascii_lowercase().as_str() {
            "bl" => selection.bootloader = true,
            "ap" => selection.ap = true,
            "cp" => selection.cp = true,
            _ => bail!("Unknown --only image class {item:?}; expected bl, ap, or cp"),
        }
    }
    Ok(selection)
}

fn progress_bar(total: u64, label: &str) -> ProgressBar {
    let progress = ProgressBar::new(total);
    progress.set_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "  {{bar:40.cyan/blue}} {{percent:>3}}% {{pos:>7}}/{{len:7}} {label}"
            ))
            .expect("static progress template is valid")
            .progress_chars("##-"),
    );
    progress
}

fn flash_package(
    file: &Path,
    args: &DownloadArgs,
    at_port: Option<(&str, u32)>,
    only: &[String],
) -> Result<()> {
    let package = load_package(file)?;
    let selection = package_selection(only)?;
    let plan = plan_binpkg_images(&package, selection)?;
    log::info!("Package product: {}", package.product_name);
    let mut session = start_session(args, at_port, Some(&package))?;

    for image in &plan {
        let data = image
            .entry
            .data
            .as_deref()
            .expect("the package planner validates retained entry data");
        let progress = progress_bar(data.len() as u64, image.target.tag);
        let mut update = |completed, _total| progress.set_position(completed);
        if let Err(error) = session.flash_image(image.target, data, Some(&mut update)) {
            progress.abandon_with_message(format!("{} FAILED", image.target.tag));
            return Err(error).with_context(|| format!("Failed to flash {}", image.entry.name));
        }
        progress.finish_with_message(format!("{} done", image.target.tag));
    }

    session
        .finish_reset()
        .context("Images were written, but the final device reset failed")?;
    log::info!("Flash complete: {} image(s)", plan.len());
    Ok(())
}

fn erase(address: &str, size: &str, args: &DownloadArgs) -> Result<()> {
    let address = parse_u32(address, "address")?;
    let size = parse_u32(size, "size")?;
    let mut session = start_session(args, None, None)?;
    let progress = progress_bar(size as u64, "erase range");
    let mut update = |completed, _total| progress.set_position(completed);
    if let Err(error) = session.erase_with_progress(address, size, Some(&mut update)) {
        progress.abandon_with_message("erase range FAILED");
        return Err(error);
    }
    progress.finish_with_message("erase range done");
    session
        .finish_reset()
        .context("Erase completed, but the final device reset failed")
}

fn read(address: &str, size: &str, output: &Path, args: &DownloadArgs) -> Result<()> {
    let address = parse_u32(address, "address")?;
    let size = parse_u32(size, "size")?;
    let mut session = start_session(args, None, None)?;
    let data = session.read(address, size)?;
    if let Err(write_error) =
        fs::write(output, data).with_context(|| format!("Failed to write {}", output.display()))
    {
        return match session.finish_reset() {
            Ok(()) => Err(write_error).context("Read succeeded, but writing the output failed"),
            Err(reset_error) => Err(write_error).context(format!(
                "Read succeeded, but writing the output failed; the recovery device reset also failed: {reset_error:#}"
            )),
        };
    }
    session
        .finish_reset()
        .context("Read completed, but the final device reset failed")?;
    log::info!("Wrote {} bytes to {}", size, output.display());
    Ok(())
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
    fn cli_image_selection_is_typed_and_validated() {
        assert_eq!(package_selection(&[]).unwrap(), PackageSelection::all());
        assert_eq!(
            package_selection(&["ap".to_string(), "CP".to_string()]).unwrap(),
            PackageSelection {
                bootloader: false,
                ap: true,
                cp: true,
            }
        );
        assert!(package_selection(&["script".to_string()]).is_err());
    }
}
