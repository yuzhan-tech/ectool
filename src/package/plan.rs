//! Deterministic planning of generic BL/AP/CP images from an EigenComm
//! `.binpkg`.

use anyhow::{bail, Context, Result};

use crate::flash::burn::{FlashStorage, ImageKind, ImageTarget};

use super::binpkg::{BinpkgEntry, BinpkgResult};

const AP_XIP_BASE: u32 = 0x0080_0000;

/// Typed selection of the generic image classes understood by `ectool`.
///
/// Package-specific entries such as LuatOS scripts are deliberately outside
/// this selection and are ignored by the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageSelection {
    /// Include package entries whose image type is `BL`.
    pub bootloader: bool,
    /// Include package entries whose image type is `AP`.
    pub ap: bool,
    /// Include package entries whose image type is `CP`.
    pub cp: bool,
}

impl PackageSelection {
    /// Select all generic package image classes.
    pub const fn all() -> Self {
        Self {
            bootloader: true,
            ap: true,
            cp: true,
        }
    }

    /// Return whether at least one image class is selected.
    pub const fn is_empty(self) -> bool {
        !self.bootloader && !self.ap && !self.cp
    }
}

impl Default for PackageSelection {
    fn default() -> Self {
        Self::all()
    }
}

/// One package entry paired with the generic image-transfer target derived
/// from its metadata.
#[derive(Debug, Clone, Copy)]
pub struct PlannedImage<'a> {
    /// Original package entry, including retained image data.
    pub entry: &'a BinpkgEntry,
    /// Generic transfer target derived from package metadata.
    pub target: ImageTarget<'a>,
}

fn ap_flash_offset(address: u32) -> u32 {
    address.checked_sub(AP_XIP_BASE).unwrap_or(address)
}

fn cp_uses_ap_flash(product_name: &str) -> bool {
    let product = product_name.trim().to_ascii_uppercase();
    product.contains("EC7") || product.contains("YCOM_7")
}

fn selected_target<'a>(
    entry: &'a BinpkgEntry,
    selection: PackageSelection,
    ec7xx: bool,
) -> Option<ImageTarget<'a>> {
    match entry.image_type.trim().to_ascii_uppercase().as_str() {
        "BL" if selection.bootloader => Some(ImageTarget {
            image_type: ImageKind::Bootloader,
            storage: FlashStorage::ApFlash,
            address: 0,
            tag: entry.image_type.as_str(),
        }),
        "AP" if selection.ap => Some(ImageTarget {
            image_type: ImageKind::Ap,
            storage: FlashStorage::ApFlash,
            address: ap_flash_offset(entry.addr),
            tag: entry.image_type.as_str(),
        }),
        "CP" if selection.cp && ec7xx => Some(ImageTarget {
            image_type: ImageKind::Cp,
            storage: FlashStorage::ApFlash,
            address: ap_flash_offset(entry.addr),
            tag: entry.image_type.as_str(),
        }),
        "CP" if selection.cp => Some(ImageTarget {
            image_type: ImageKind::Cp,
            storage: FlashStorage::CpFlash,
            address: 0,
            tag: entry.image_type.as_str(),
        }),
        _ => None,
    }
}

/// Plan the selected generic BL/AP/CP images in their original package order.
///
/// The package must have been parsed with entry data retained. Empty
/// selections, absent selected images, empty images, metadata/data size
/// mismatches, and overflowing target ranges are rejected before a caller
/// opens or modifies a device.
pub fn plan_binpkg_images(
    package: &BinpkgResult,
    selection: PackageSelection,
) -> Result<Vec<PlannedImage<'_>>> {
    if selection.is_empty() {
        bail!("package image selection must include BL, AP, or CP");
    }

    let ec7xx = cp_uses_ap_flash(&package.product_name);
    let mut planned = Vec::new();

    for entry in &package.entries {
        let Some(target) = selected_target(entry, selection, ec7xx) else {
            continue;
        };
        let data = entry.data.as_deref().with_context(|| {
            format!(
                "selected package entry {} has no data; parse the package with data retained",
                entry.name
            )
        })?;
        if data.is_empty() {
            bail!("selected package entry {} is empty", entry.name);
        }
        if entry.image_size as usize != data.len() {
            bail!(
                "selected package entry {} declares {} bytes but contains {}",
                entry.name,
                entry.image_size,
                data.len()
            );
        }
        let image_size = u32::try_from(data.len())
            .with_context(|| format!("selected package entry {} is too large", entry.name))?;
        target
            .address
            .checked_add(image_size - 1)
            .with_context(|| {
                format!(
                    "selected package entry {} address range overflows",
                    entry.name
                )
            })?;
        planned.push(PlannedImage { entry, target });
    }

    if planned.is_empty() {
        bail!(
            "package contains no selected BL/AP/CP images for product {}",
            package.product_name
        );
    }

    Ok(planned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::binpkg::{BinpkgEntry, UNKNOWN_PRODUCT};

    fn entry(image_type: &str, address: u32, size: usize) -> BinpkgEntry {
        BinpkgEntry {
            name: format!("{image_type}.bin"),
            addr: address,
            flash_size: size as u32,
            offset: 0,
            image_size: size as u32,
            hash: String::new(),
            image_type: image_type.to_string(),
            vt: 0,
            vtsize: 0,
            rsvd: 0,
            pdata: 0,
            data: Some(vec![0xA5; size]),
        }
    }

    fn package(product_name: &str, entries: Vec<BinpkgEntry>) -> BinpkgResult {
        BinpkgResult {
            product_name: product_name.to_string(),
            raw_header: vec![0; 0x34],
            entries,
        }
    }

    #[test]
    fn bootloader_always_targets_ap_flash_at_zero() {
        let package = package("EC718P", vec![entry("BL", 0x00AB_CDEF, 1)]);
        let plan = plan_binpkg_images(
            &package,
            PackageSelection {
                bootloader: true,
                ap: false,
                cp: false,
            },
        )
        .unwrap();

        assert_eq!(plan[0].target.image_type, ImageKind::Bootloader);
        assert_eq!(plan[0].target.storage, FlashStorage::ApFlash);
        assert_eq!(plan[0].target.address, 0);
    }

    #[test]
    fn ap_accepts_xip_biased_and_raw_addresses() {
        let package = package(
            "EC718P",
            vec![entry("AP", 0x0088_2000, 1), entry("AP", 0x0008_2000, 1)],
        );
        let plan = plan_binpkg_images(
            &package,
            PackageSelection {
                bootloader: false,
                ap: true,
                cp: false,
            },
        )
        .unwrap();

        assert_eq!(plan[0].target.address, 0x0008_2000);
        assert_eq!(plan[1].target.address, 0x0008_2000);
    }

    #[test]
    fn ec7xx_cp_targets_ap_flash_at_package_address() {
        let package = package("YCOM_718PM", vec![entry("CP", 0x0089_0000, 1)]);
        let plan = plan_binpkg_images(
            &package,
            PackageSelection {
                bootloader: false,
                ap: false,
                cp: true,
            },
        )
        .unwrap();

        assert_eq!(plan[0].target.storage, FlashStorage::ApFlash);
        assert_eq!(plan[0].target.address, 0x0009_0000);
    }

    #[test]
    fn non_ec7xx_cp_targets_cp_flash_at_zero() {
        let package = package(UNKNOWN_PRODUCT, vec![entry("CP", 0x0089_0000, 1)]);
        let plan = plan_binpkg_images(
            &package,
            PackageSelection {
                bootloader: false,
                ap: false,
                cp: true,
            },
        )
        .unwrap();

        assert_eq!(plan[0].target.storage, FlashStorage::CpFlash);
        assert_eq!(plan[0].target.address, 0);
    }

    #[test]
    fn planner_preserves_order_and_ignores_script_entries() {
        let package = package(
            "EC718P",
            vec![
                entry("script", 0, 1),
                entry("CP", 0x0089_0000, 1),
                entry("FlexFile", 0, 1),
                entry("AP", 0x0088_0000, 1),
                entry("BL", 0, 1),
            ],
        );
        let plan = plan_binpkg_images(&package, PackageSelection::all()).unwrap();

        assert_eq!(
            plan.iter()
                .map(|image| image.entry.image_type.as_str())
                .collect::<Vec<_>>(),
            ["CP", "AP", "BL"]
        );
    }

    #[test]
    fn empty_or_absent_selection_fails() {
        let package = package("EC718P", vec![entry("script", 0, 1)]);
        let empty = PackageSelection {
            bootloader: false,
            ap: false,
            cp: false,
        };

        assert!(plan_binpkg_images(&package, empty)
            .unwrap_err()
            .to_string()
            .contains("must include"));
        assert!(plan_binpkg_images(&package, PackageSelection::all())
            .unwrap_err()
            .to_string()
            .contains("no selected"));
    }
}
