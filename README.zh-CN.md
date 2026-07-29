# ectool

[English](README.md) | 简体中文

`ectool` 是一个面向 EigenComm 芯片模组的通用命令行烧录与 UniLog 解码工具。它直接接受 EigenComm `.binpkg` 固件包，不依赖模组厂商特有的封装、脚本、日志或运行时串口约定。

## 安全与设备选择

- 自动检测仅识别 EigenComm 下载端口 `17D1:0001`。
- AT 端口和 UniLog 端口不会被自动推断，必须显式指定。
- 所有需要加载 AgentBoot 的操作都必须传入 `--agentboot`。项目不包含 AgentBoot 二进制文件，因为它必须与芯片、BSP 版本和传输方式相匹配。

## 构建

```sh
cargo build --release
```

生成的可执行文件位于 `target/release/ectool`。

## 烧录 `.binpkg`

如果模组正在正常运行，请传入它的 AT 端口。工具会发送准确的 `AT+ECRST=delay,99` 命令，等待 `17D1:0001` 下载端口出现，加载指定的 USB AgentBoot，然后烧录固件包：

```sh
ectool flash firmware.binpkg \
  --agentboot /path/to/agentboot_usb.bin \
  --at-port /dev/cu.module-at
```

如果设备已经处于下载模式，则省略 `--at-port`：

```sh
ectool flash firmware.binpkg \
  --agentboot /path/to/agentboot_usb.bin
```

当连接了多个 `17D1:0001` 设备时，需要显式指定下载端口：

```sh
ectool flash firmware.binpkg \
  --port /dev/cu.download \
  --agentboot /path/to/agentboot_usb.bin
```

## 诊断操作

擦除和读取操作同样需要外部 AgentBoot：

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

UART 下载必须同时指定串口和与之匹配的 UART AgentBoot：

```sh
ectool flash firmware.binpkg \
  --transport uart \
  --port /dev/cu.usbserial \
  --agentboot /path/to/agentboot_uart.bin
```

当固件包包含与传输方式对应的 `_usb.baseini` 或 `_uart.baseini` 条目时，`ectool` 只使用与显式 `--transport` 选项匹配的条目。AgentBoot 波特率按以下优先级确定：`--agent-baud`、包内 `agbaud`、固定默认值 `921600`。如果可选控制项缺失，则保留现有的 AgentBoot `pullup_qspi=1` 行为，并禁用 dribble download。工具不会读取外部 `.baseini` 或板级配置文件。

## UniLog

实时采集 UniLog 时必须显式指定端口：

```sh
ectool unilog \
  --port /dev/cu.module-unilog \
  --comdb /path/to/comdb.txt
```

已采集的数据可以在不连接串口的情况下回放：

```sh
ectool unilog --file capture.bin --comdb /path/to/comdb.txt
```

使用 `--raw` 代替 `--comdb` 可以直接输出未解码的记录。

工具不提供独立的复位命令。固件包成功烧录后会按协议执行正常的最终复位。AgentBoot 启动后，诊断擦除或读取即使失败也会尝试执行最终复位，同时保留并报告原始操作错误。
