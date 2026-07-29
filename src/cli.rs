use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ectool",
    version,
    about = "Generic flashing and UniLog tools for EigenComm chips"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    pub debug: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Transport {
    Usb,
    Uart,
}

#[derive(Args)]
pub struct DownloadArgs {
    /// Download port, or "auto" to find exactly VID:PID 17D1:0001
    #[arg(short, long, default_value = "auto")]
    pub port: String,

    /// Download transport
    #[arg(short = 't', long, value_enum, default_value_t = Transport::Usb)]
    pub transport: Transport,

    /// Vendor agentboot.bin matching this chip and transport
    #[arg(long)]
    pub agentboot: PathBuf,

    /// Baud rate requested when starting agentboot
    #[arg(long)]
    pub agent_baud: Option<u32>,

    /// Seconds to wait for the download port
    #[arg(long, default_value_t = 120)]
    pub wait: u64,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Flash all or selected images from an EigenComm .binpkg
    Flash {
        /// Firmware package
        file: PathBuf,

        #[command(flatten)]
        download: DownloadArgs,

        /// Explicit AT port used to send AT+ECRST=delay,99 when needed
        #[arg(long)]
        at_port: Option<String>,

        /// Baud rate for --at-port
        #[arg(long, default_value_t = 115200)]
        at_baud: u32,

        /// Flash only selected image classes (bl, ap, cp)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },

    /// Erase a raw AP-flash range
    Erase {
        #[arg(long)]
        address: String,

        #[arg(long)]
        size: String,

        #[command(flatten)]
        download: DownloadArgs,
    },

    /// Read a raw memory range through agentboot
    Read {
        #[arg(long)]
        address: String,

        #[arg(long)]
        size: String,

        #[arg(short, long)]
        output: PathBuf,

        #[command(flatten)]
        download: DownloadArgs,
    },

    /// Capture or replay EigenComm UniLog records
    Unilog {
        /// Explicit UniLog serial port; required for live capture
        #[arg(short, long)]
        port: Option<String>,

        /// comdb.txt used for decoding
        #[arg(short, long)]
        comdb: Option<PathBuf>,

        /// Print raw records without a comdb
        #[arg(long)]
        raw: bool,

        /// Show undecodable PHY records
        #[arg(long)]
        phy: bool,

        #[arg(long, value_delimiter = ',')]
        owner: Vec<String>,

        #[arg(long, value_delimiter = ',')]
        module: Vec<String>,

        #[arg(long, value_delimiter = ',')]
        sub: Vec<String>,

        #[arg(long, value_delimiter = ',')]
        level: Vec<String>,

        /// Replay a captured byte file instead of opening a live port
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,

        /// Write captured bytes to this file
        #[arg(short, long)]
        out: Option<PathBuf>,

        #[arg(long)]
        append: bool,

        #[arg(long)]
        version_check: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn public_cli_is_package_only() {
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(|subcommand| subcommand.get_name())
            .collect::<Vec<_>>();
        assert!(names.contains(&"flash"));
        assert!(!names.contains(&"flash-image"));
        assert!(!names.contains(&"unpack"));
    }
}
