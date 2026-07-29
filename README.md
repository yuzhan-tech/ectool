# ectool

English | [简体中文](README.zh-CN.md)

`ectool` is a generic command-line flasher and UniLog decoder for modules based
on EigenComm chips. It accepts EigenComm `.binpkg` firmware packages without
module-vendor packaging, script, trace, or runtime-port conventions.

## Safety and device selection

- The only automatically detected USB device is the EigenComm download port
  `17D1:0001`.
- AT and UniLog ports are never inferred. Supply them explicitly.
- Every operation that loads an agent requires `--agentboot`. No agentboot
  binaries are included because they are chip-, revision-, and
  transport-specific.

## Build

```sh
cargo build --release
```

The executable is written to `target/release/ectool`.

## Use as a Rust library

`ectool` is also the reusable implementation of the EigenComm download workflow.
Its crate-root API accepts an already selected/open serial port and
caller-provided AgentBoot bytes, then owns DLBOOT/AgentBoot synchronization,
image transfer, read, erase, explicit reset, and recovery reset behavior.
Package planning maps only generic BL/AP/CP entries; LuatOS archives, LuaDB,
scripts, and vendor runtime-port behavior remain application-owned.

Until a downstream-ready release is tagged, consume the crate as a pinned Git
dependency using a full reviewed commit:

```toml
ectool_core = {
    package = "ectool",
    git = "https://github.com/yuzhan-tech/ectool.git",
    rev = "<full-release-commit>"
}
```

Use `default-features = false` for a flashing-only dependency that does not need
the command-line adapter or UniLog decoder. A local Cargo patch is suitable
while developing two repositories together, but a released consumer must not
require a sibling `../ectool` checkout.

The crate documentation contains a complete custom-image example using
`FlashSession`. A downstream package format may add its own metadata and
compatibility rules; resolve those before opening the download port and
starting the generic session.

## Flash a `.binpkg`

If the module is running normally, pass its AT port. The tool sends the exact
command `AT+ECRST=delay,99`, waits for `17D1:0001`, loads the supplied USB
agentboot, and flashes the package:

```sh
ectool flash firmware.binpkg \
  --agentboot /path/to/agentboot_usb.bin \
  --at-port /dev/cu.module-at
```

If the device is already in download mode, omit `--at-port`:

```sh
ectool flash firmware.binpkg \
  --agentboot /path/to/agentboot_usb.bin
```

Select an explicit download port when multiple `17D1:0001` devices are
connected:

```sh
ectool flash firmware.binpkg \
  --port /dev/cu.download \
  --agentboot /path/to/agentboot_usb.bin
```

## Diagnostic operations

Erase and read operations also require an external agentboot:

```sh
ectool erase \
  --address 0x00800000 \
  --size 0x00100000 \
  --agentboot /path/to/agentboot_usb.bin

ectool read \
  --address 0x00800000 \
  --size 0x1000 \
  --output dump.bin \
  --agentboot /path/to/agentboot_usb.bin
```

For UART download, both the port and matching UART agentboot must be supplied:

```sh
ectool flash firmware.binpkg \
  --transport uart \
  --port /dev/cu.usbserial \
  --agentboot /path/to/agentboot_uart.bin
```

When the package contains transport-specific `_usb.baseini` or `_uart.baseini`
entries, `ectool` uses only the entry matching the explicit `--transport`.
AgentBoot baud resolves as `--agent-baud`, bundled `agbaud`, then `921600`. If
optional controls are absent, the existing AgentBoot `pullup_qspi=1` behavior is
retained and dribble download is disabled. No external `.baseini` or board
profile is read.

A successful protocol exchange proves that the selected operation completed;
it does not by itself prove that the supplied AgentBoot, package, chip revision,
module, and board are compatible.

## UniLog

Live UniLog capture always requires an explicit port:

```sh
ectool unilog \
  --port /dev/cu.module-unilog \
  --comdb /path/to/comdb.txt
```

Captured data can be replayed without a port:

```sh
ectool unilog --file capture.bin --comdb /path/to/comdb.txt
```

Use `--raw` instead of `--comdb` to print undecoded records.

There is no standalone reset command. A successful package flash performs its
normal final reset. After AgentBoot starts, diagnostic erase and read operations
also attempt a final reset when the operation fails, while preserving the
original error.
