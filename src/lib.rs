//! Reusable EigenComm flashing, package, serial, and optional UniLog support.
//!
//! Most downstream tools should use the crate-root re-exports instead of
//! sequencing packet-level modules directly.
//!
//! ```no_run
//! use ectool::{
//!     AgentBootConfig, FlashSession, FlashStorage, ImageKind, ImageTarget, PortType,
//!     TransferConfig,
//! };
//!
//! # fn main() -> anyhow::Result<()> {
//! let agentboot = std::fs::read("agentboot_usb.bin")?;
//! let application = std::fs::read("ap.bin")?;
//! let port = serialport::new("/dev/cu.eigencomm-download", PortType::Usb.baudrate())
//!     .open()?;
//!
//! let mut session = FlashSession::start(
//!     port,
//!     AgentBootConfig {
//!         data: &agentboot,
//!         baud: 921_600,
//!         pullup_qspi: true,
//!     },
//!     TransferConfig {
//!         port_type: PortType::Usb,
//!         dribble_download: false,
//!     },
//! )?;
//! session.flash_image(
//!     ImageTarget {
//!         image_type: ImageKind::Ap,
//!         storage: FlashStorage::ApFlash,
//!         address: 0x0008_2000,
//!         tag: "AP",
//!     },
//!     &application,
//!     None,
//! )?;
//! session.finish_reset()?;
//! # Ok(())
//! # }
//! ```

pub mod flash;
pub mod package;
pub mod serial;
#[cfg(feature = "unilog")]
pub mod unilog;
pub mod util;

pub use flash::burn::{FlashStorage, ImageKind, ImageTarget};
pub use flash::session::{
    resolve_transfer_config, AgentBootConfig, FlashSession, ResolvedTransferConfig, TransferConfig,
    TransferOverrides,
};
pub use package::binpkg::{
    parse_binpkg, rehash_entry, serialize_binpkg, BinpkgEntry, BinpkgResult, BundledFlashConfig,
};
pub use package::plan::{plan_binpkg_images, PackageSelection, PlannedImage};
pub use serial::detect::{
    find_download_port_now, wait_for_download_port, DownloadPort, DOWNLOAD_PID, DOWNLOAD_VID,
};
pub use serial::port::{open_port, PortType};
