use super::consts::*;

/// DL command structure (8 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub cmd: u8,
    pub index: u8,
    pub order_id: u8,
    pub norder_id: u8,
    pub len: u32,
}

impl Cmd {
    pub fn new(cmd_id: u8) -> Self {
        Cmd {
            cmd: cmd_id,
            index: 0,
            order_id: DL_COMMAND_ID,
            norder_id: DL_COMMAND_ID_INV,
            len: 0,
        }
    }

    pub fn pack(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        buf.push(self.cmd);
        buf.push(self.index);
        buf.push(self.order_id);
        buf.push(self.norder_id);
        buf.extend_from_slice(&self.len.to_le_bytes());
        buf
    }
}

/// DL response structure (6 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rsp {
    pub cmd: u8,
    pub index: u8,
    pub order_id: u8,
    pub norder_id: u8,
    pub state: u8,
    pub len: u8,
}

impl Rsp {
    pub fn unpack(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() != FIXED_PROTOCOL_RSP_LEN {
            anyhow::bail!(
                "response header length is {}, expected {}",
                data.len(),
                FIXED_PROTOCOL_RSP_LEN
            );
        }
        Ok(Rsp {
            cmd: data[0],
            index: data[1],
            order_id: data[2],
            norder_id: data[3],
            state: data[4],
            len: data[5],
        })
    }
}

/// LPC command structure (8 bytes, same layout as Cmd but different IDs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LpcCmd {
    pub cmd: u8,
    pub index: u8,
    pub order_id: u8,
    pub norder_id: u8,
    pub len: u32,
}

impl LpcCmd {
    pub fn new(cmd_id: u8) -> Self {
        LpcCmd {
            cmd: cmd_id,
            index: 0,
            order_id: LPC_COMMAND_ID,
            norder_id: LPC_COMMAND_ID_INV,
            len: 0,
        }
    }

    pub fn pack(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        buf.push(self.cmd);
        buf.push(self.index);
        buf.push(self.order_id);
        buf.push(self.norder_id);
        buf.extend_from_slice(&self.len.to_le_bytes());
        buf
    }
}

/// Image header structure for firmware download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageHeaderControls {
    pub pullup_qspi: bool,
    pub dribble_download: bool,
    pub current_address_valid: bool,
}

impl ImageHeaderControls {
    pub fn for_transfer(
        pullup_qspi: bool,
        dribble_download: bool,
        current_address: u32,
        is_usb: bool,
    ) -> Self {
        let mut controls = Self {
            pullup_qspi,
            ..Self::default()
        };

        // The vendor protocol leaves both dribble bits clear for USB and for
        // address zero. For nonzero UART addresses it always marks the current
        // address valid when dribble mode is disabled. In dribble mode the
        // address is only needed when it is not already 64 KiB aligned.
        if !is_usb && current_address != 0 {
            controls.dribble_download = dribble_download;
            controls.current_address_valid = !dribble_download || current_address & 0xffff != 0;
        }

        controls
    }

    fn pack(self) -> u32 {
        u32::from(self.pullup_qspi)
            | (u32::from(self.dribble_download) << 1)
            | (u32::from(self.current_address_valid) << 2)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentBootControl {
    bytes: [u8; 4],
}

impl AgentBootControl {
    /// Create a valid agent-download control only when all flash-size fields
    /// come from trusted package metadata. Callers currently use the
    /// zero-valued default because `.binpkg` does not expose those fields.
    pub fn from_flash_sizes(control_magic: u8, cp_flash_mb: u8, ap_flash_mb: u8) -> Self {
        let mut bytes = [0u8; 4];
        bytes[0] = (control_magic & 0x0f) | ((cp_flash_mb & 0x0f) << 4);
        bytes[1] = ap_flash_mb;
        bytes[3] = bytes[0].wrapping_add(bytes[1]).wrapping_add(bytes[2]);
        Self { bytes }
    }
}

pub struct ImgHead {
    data: Vec<u8>,
}

impl Default for ImgHead {
    fn default() -> Self {
        Self::new()
    }
}

impl ImgHead {
    /// Size of the packed image header.
    // Image version header(16) + imgnum(4) + CtlInfo(4) + rsvd0(4) + rsvd1(4) +
    // hashih(32) + ImgBody(4+4+4+4+16+32+64+64=192) + ReservedArea(4+4+8=16) = 272
    pub const SIZE: usize = 272;

    /// Create a new image header with default values.
    pub fn new() -> Self {
        let mut data = vec![0u8; Self::SIZE];

        // Image-header version value
        data[0..4].copy_from_slice(&0x10000001u32.to_le_bytes());
        // Image-header identifier
        data[4..8].copy_from_slice(&IMGH_IDENTIFIER.to_le_bytes());
        // Image-header format date
        data[8..12].copy_from_slice(&0x20180507u32.to_le_bytes());
        // imgnum = 1
        data[16..20].copy_from_slice(&1u32.to_le_bytes());
        // ctlinfo.hashtype = 0xee
        data[20] = 0xee;
        // ImgBody.id = AGBT_IDENTIFIER (at offset 64: after verinfo+imgnum+ctlinfo+rsvd0+rsvd1+hashih)
        let body_offset = 16 + 4 + 4 + 4 + 4 + 32; // = 64
        data[body_offset..body_offset + 4].copy_from_slice(&AGBT_IDENTIFIER.to_le_bytes());
        // ImgBody.ldloc = 0x04010000
        data[body_offset + 8..body_offset + 12].copy_from_slice(&0x04010000u32.to_le_bytes());

        ImgHead { data }
    }

    // Field offsets
    const CTLINFO_OFFSET: usize = 20; // After VersionInfo(16) + imgnum(4)
    const RSVD0_OFFSET: usize = 24; // After CtlInfo(4)
    const HASHIH_OFFSET: usize = 32; // After rsvd0(4) + rsvd1(4)
    const BODY_OFFSET: usize = 64; // After hashih(32)
    const AGENT_CONTROL_OFFSET: usize = Self::BODY_OFFSET + 16;

    pub fn set_body_id(&mut self, id: u32) {
        self.data[Self::BODY_OFFSET..Self::BODY_OFFSET + 4].copy_from_slice(&id.to_le_bytes());
    }

    pub fn set_burn_addr(&mut self, addr: u32) {
        let off = Self::BODY_OFFSET + 4;
        self.data[off..off + 4].copy_from_slice(&addr.to_le_bytes());
    }

    pub fn set_img_size(&mut self, size: u32) {
        let off = Self::BODY_OFFSET + 12;
        self.data[off..off + 4].copy_from_slice(&size.to_le_bytes());
    }

    pub fn set_hashv(&mut self, hash: &[u8; 32]) {
        let off = Self::BODY_OFFSET + 32; // After id(4) + burnaddr(4) + ldloc(4) + img_size(4) + reserve(16)
        self.data[off..off + 32].copy_from_slice(hash);
    }

    pub fn set_baudrate_ctrl(&mut self, baud: u32) {
        let ctrl = if baud != 0 {
            ((baud / 100) + 0x8000) as u16
        } else {
            0
        };
        let off = Self::CTLINFO_OFFSET + 2; // baudratectrl is at offset 2 within CtlInfo
        self.data[off..off + 2].copy_from_slice(&ctrl.to_le_bytes());
    }

    pub fn set_hashtype(&mut self, hashtype: u8) {
        self.data[Self::CTLINFO_OFFSET] = hashtype;
    }

    pub fn set_header_controls(&mut self, controls: ImageHeaderControls) {
        self.data[Self::RSVD0_OFFSET..Self::RSVD0_OFFSET + 4]
            .copy_from_slice(&controls.pack().to_le_bytes());
    }

    pub fn set_agentboot_control(&mut self, control: AgentBootControl) {
        self.data[Self::AGENT_CONTROL_OFFSET..Self::AGENT_CONTROL_OFFSET + 4]
            .copy_from_slice(&control.bytes);
    }

    pub fn set_hashih(&mut self, hash: &[u8; 32]) {
        self.data[Self::HASHIH_OFFSET..Self::HASHIH_OFFSET + 32].copy_from_slice(hash);
    }

    /// Compute and set the header's own hash (hashih field).
    pub fn finalize_hash(&mut self) {
        use sha2::{Digest, Sha256};
        let hash: [u8; 32] = Sha256::digest(&self.data).into();
        self.set_hashih(&hash);
    }

    pub fn pack(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentBootControl, ImageHeaderControls, ImgHead};

    #[test]
    fn image_header_controls_serialize_byte_for_byte() {
        let mut absent = ImgHead::new();
        absent.set_header_controls(ImageHeaderControls::default());
        assert_eq!(&absent.pack()[24..28], &[0, 0, 0, 0]);

        let controls = ImageHeaderControls::for_transfer(true, true, 0x1234, false);
        let mut present = ImgHead::new();
        present.set_header_controls(controls);
        assert_eq!(&present.pack()[24..28], &[0b111, 0, 0, 0]);
    }

    #[test]
    fn agentboot_control_is_zero_unless_explicitly_constructed() {
        let mut header = ImgHead::new();
        header.set_agentboot_control(AgentBootControl::default());
        assert_eq!(&header.pack()[80..84], &[0, 0, 0, 0]);

        header.set_agentboot_control(AgentBootControl::from_flash_sizes(5, 2, 8));
        assert_eq!(&header.pack()[80..84], &[0x25, 0x08, 0x00, 0x2d]);
    }

    #[test]
    fn dribble_flags_follow_transport_and_address() {
        assert_eq!(
            ImageHeaderControls::for_transfer(false, true, 0x1234, true),
            ImageHeaderControls::default()
        );
        assert_eq!(
            ImageHeaderControls::for_transfer(false, true, 0, false),
            ImageHeaderControls::default()
        );
        assert_eq!(
            ImageHeaderControls::for_transfer(false, true, 0x10000, false),
            ImageHeaderControls {
                dribble_download: true,
                ..ImageHeaderControls::default()
            }
        );
        assert_eq!(
            ImageHeaderControls::for_transfer(false, false, 0x10000, false),
            ImageHeaderControls {
                current_address_valid: true,
                ..ImageHeaderControls::default()
            }
        );
    }
}
