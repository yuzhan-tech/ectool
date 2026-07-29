/// Low-level image-transfer primitives. Most applications should use
/// [`session::FlashSession`].
pub mod burn;
/// Low-level command encoders and exchanges for protocol tests and advanced
/// tooling.
pub mod commands;
/// Wire constants and image identifiers for protocol tests and advanced
/// tooling.
pub mod consts;
/// Low-level LPC operations for protocol tests and advanced tooling.
pub mod lpc;
/// Packet structures and header encoders for protocol tests and advanced
/// tooling.
pub mod protocol;
/// High-level reusable download-session lifecycle.
pub mod session;
/// Low-level handshake synchronization for protocol tests and advanced
/// tooling.
pub mod sync;
