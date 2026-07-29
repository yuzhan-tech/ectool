// Handshake sync values
pub const DLBOOT_HANDSHAKE: u32 = 0x2b02d300;
pub const AGBOOT_HANDSHAKE: u32 = 0x2b02d3aa;
pub const LPC_HANDSHAKE: u32 = 0x2b02d3cd;

// Image identifiers
pub const IMGH_IDENTIFIER: u32 = 0x54494d48;
pub const AGBT_IDENTIFIER: u32 = 0x4F424D49;
pub const AIMG_IDENTIFIER: u32 = 0x444B4249;
pub const CIMG_IDENTIFIER: u32 = 0x43504249;
pub const FLEX_IDENTIFIER: u32 = 0x464c5849;

// DL command framing
pub const DL_COMMAND_ID: u8 = 0xcd;
pub const DL_COMMAND_ID_INV: u8 = 0x32;

// LPC command framing
pub const LPC_COMMAND_ID: u8 = 0x4c;
pub const LPC_COMMAND_ID_INV: u8 = 0xb3;

// DL commands
pub const CMD_GET_VERSION: u8 = 0x20;
pub const CMD_SEL_IMAGE: u8 = 0x21;
pub const CMD_VERIFY_IMAGE: u8 = 0x22;
pub const CMD_DATA_HEAD: u8 = 0x31;
pub const CMD_DOWNLOAD_DATA: u8 = 0x32;
pub const CMD_DONE: u8 = 0x3a;

// LPC commands
pub const LPC_FLASH_ERASE: u8 = 0x10;
pub const LPC_READ_MEM: u8 = 0x21;
pub const LPC_BURN_ONE: u8 = 0x42;
pub const LPC_GET_BURN_STATUS: u8 = 0x44;
pub const LPC_SYS_RST: u8 = 0xaa;

// LPC response magic values
pub const LPC_BURN_STATUS_OK: &[u8] = &[0x00, 0x00, 0x00, 0x00];
pub const LPC_SYS_RESET_ACK: &[u8] = b"ZzZzZzZz";

// Response sizes
pub const FIXED_PROTOCOL_RSP_LEN: usize = 6;
pub const MAX_DL_RSP_DATA_LEN: usize = 12;
pub const MAX_LPC_RSP_DATA_LEN: usize = 0x100;
pub const MAX_COMMAND_ATTEMPTS: usize = 10;

// Storage types
pub const STYPE_AP_FLASH: u8 = 0x0;
pub const STYPE_CP_FLASH: u8 = 0x1;
pub const CP_FLASH_MARKER: u16 = 0xe101;

// Max data block size for transfers
pub const MAX_DATA_BLOCK_SIZE: usize = 0x10000; // 64KB
pub const MAX_READ_MEM_SIZE: usize = 240;

// Sync types
#[derive(Debug, Clone, Copy)]
pub enum SyncType {
    DlBoot,
    AgBoot,
    Lpc,
}

impl SyncType {
    pub fn handshake_value(&self) -> u32 {
        match self {
            SyncType::DlBoot => DLBOOT_HANDSHAKE,
            SyncType::AgBoot => AGBOOT_HANDSHAKE,
            SyncType::Lpc => LPC_HANDSHAKE,
        }
    }
}

// Burn image types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnImageType {
    Bootloader,
    Ap,
    Cp,
    FlexFile,
    Head,
    AgBoot,
}

impl BurnImageType {
    pub fn identifier(&self) -> u32 {
        match self {
            BurnImageType::Bootloader | BurnImageType::AgBoot => AGBT_IDENTIFIER,
            BurnImageType::Ap => AIMG_IDENTIFIER,
            BurnImageType::Cp => CIMG_IDENTIFIER,
            BurnImageType::FlexFile => FLEX_IDENTIFIER,
            BurnImageType::Head => IMGH_IDENTIFIER,
        }
    }
}
