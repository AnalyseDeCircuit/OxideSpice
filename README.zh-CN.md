# OxideSpice

[English](README.md) · [简体中文](README.zh-CN.md)

OxideSpice 是一个独立的 Rust SPICE 客户端协议栈。经过边界检查的字节协议核心使用纯
Rust 实现，不使用 `spice-gtk`、`libspice-client-glib` 或其他 C/FFI SPICE 客户端。
项目同时提供有界的独立 helper 进程，让宿主应用能够隔离 SPICE 会话、编解码器、文件
系统和原生设备的所有权。

OxideSpice 使用 [Apache-2.0](LICENSE) 许可证。

## 主要能力

- 对 SPICE Link、能力协商、认证和消息帧进行边界检查，所有分配均有上限。
- 支持 TCP、Unix 域套接字、Ticket 认证、SASL 和由调用方配置的 rustls TLS。
- 管理 Main、Display、Cursor、Inputs、Agent、Playback、Record、Port、WebDAV、
  USBredir 和 Smartcard 通道。
- 支持原始与索引位图、Composite/A8、LZ、GLZ、zlib-GLZ、LZ4、JPEG、JPEG-alpha、
  QUIC、MJPEG、VP8、H.264、VP9 和 H.265 显示路径。
- 支持多显示器、多选择区和多种二进制格式的剪贴板、Agent 文件传输、音频状态以及
  显示器配置。
- 支持原始与 Opus Playback/Record、普通 Port 字节流、显式授权的 WebDAV 根目录、
  USB 重定向和 PC/SC 智能卡。
- 支持半无缝与无缝迁移，并按连接代次替换状态。
- 可复用客户端接口在 Linux Unix 套接字连接中支持 DMA-BUF scanout。
- 使用显式取消、有界队列和帧合并；编解码工作不会阻塞网络所有者任务。
- 协议行为依据 SPICE 规范、`spice-protocol` 定义以及 QEMU/spice-server 的可观察行为
  实现。

## Workspace 结构

| Crate | 职责 |
| --- | --- |
| `oxide-spice-protocol` | 依赖精简的协议常量、语义类型、安全解析器和编码器；不包含 I/O 运行时或原生依赖。 |
| `oxide-spice-codecs` | 有界的图像、视频和音频编解码实现及适配器。 |
| `oxide-spice-client` | 异步传输、认证、会话、通道、表面、迁移和取消；不依赖 UI 框架。 |
| `oxide-spice-helper` | 独立 stdio 进程，以及由宿主持有权限的 WebDAV、USB/libusb 和 PC/SC 集成。 |

OxideSpice 不依赖 UI 框架或宿主应用的专用类型。

## 原生依赖边界

SPICE 字节协议始终由 Rust 代码管理。部分生产级编解码、光栅、密码学和设备边界会
有意使用原生代码：

| 边界 | 依赖方式 |
| --- | --- |
| Composite/A8 | 通过 `pixman-sys` 和 `pkg-config` 动态使用系统 pixman。 |
| TLS | 启用 `tls-ring` 时由 `ring` 编译随包提供的 C 和汇编代码。 |
| SASL GSSAPI | 通过 `libgssapi-sys` 使用系统 Kerberos/GSSAPI。 |
| Opus | 通过 `opusic-sys` 和 CMake 编译随包提供、采用 BSD 许可证的 libopus。 |
| H.264 | 编译随包提供、采用 BSD 许可证的 OpenH264 C++ 和汇编代码。 |
| VP8/VP9 | 通过 `env-libvpx-sys`、`pkg-config` 和 bindgen 使用系统 libvpx。 |
| USB 重定向 | 通过 `usbredirhost` 动态使用 usbredir/libusb。 |
| 智能卡 | 通过 `pcsc-sys` 使用平台 PC/SC 服务。 |

项目不会链接任何原生 SPICE 客户端库。详细信息和许可证边界见
[依赖策略](docs/protocol-design.zh-CN.md#依赖策略)。

## 构建要求

- 支持 Rust 2024 edition 的稳定版 Rust 工具链。
- 用于随包原生编解码器和密码学实现的 C/C++ 工具链与 CMake。
- `pkg-config`，以及相关依赖需要的 libclang/bindgen 环境。
- 构建对应 crate 时，需要 pixman、libvpx、Kerberos/GSSAPI、usbredir/libusb 和 PC/SC
  开发包。

协议 crate 不需要上述原生软件包，可以独立构建：

```sh
cargo build -p oxide-spice-protocol
```

构建完整 workspace：

```sh
cargo build --workspace --all-features
```

## 客户端快速开始

```rust,no_run
use oxide_spice_client::{ConnectOptions, Session, TicketSecret};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = ConnectOptions::new(
        "127.0.0.1",
        5900,
        TicketSecret::new(std::env::var("OXIDE_SPICE_TICKET").unwrap_or_default()),
    );
    let mut session = Session::connect(options).await?;
    let frame = session.next_frame().await?;
    let snapshot = frame.surface.snapshot().await?;

    println!("received {}x{} RGBA frame", snapshot.width, snapshot.height);
    session.shutdown().await?;
    Ok(())
}
```

仓库提供了受控的首帧探针：

```sh
OXIDE_SPICE_TICKET='<ticket>' \
  cargo run -p oxide-spice-client --example first_frame -- \
  127.0.0.1 5900 first-frame.ppm
```

真实部署应使用权限受限的密钥来源，不要把 Ticket 放入进程参数或提交到仓库的配置中。

## 独立 helper

以子进程方式启动 helper：

```sh
cargo run -p oxide-spice-helper -- --stdio
```

父进程向标准输入写入请求，从标准输出读取事件。有界协议使用 JSON 头，并为帧、光标
形状、剪贴板、PCM、文件传输分块和 Port 数据传输原始二进制负载。它提供：

- 连接状态、服务器身份、拓扑、RGBA 帧区域和光标状态；
- 服务端确认的鼠标模式、键盘修饰状态、输入、剪贴板和显示器配置；
- Agent 状态、文件传输、音量和图形设备映射；
- Playback/Record 数据与设置，以及普通 Port 字节流；
- 显式 WebDAV 目录授权，以及由 helper 管理的 USB/PCSC 设备发现。

stdio helper 不启用 GL scanout，因为标准输入输出无法传递 DMA-BUF 文件描述符。需要
零复制 scanout 的应用可以直接使用客户端接口，或提供明确的 Unix 文件描述符旁路。

帧格式、限制、顺序和请求/事件语义见 [helper IPC 契约](docs/helper-ipc.zh-CN.md)。

## 检查与测试

检查所有 crate、目标和功能组合：

```sh
cargo check --workspace --all-targets --all-features
```

运行协议与状态测试：

```sh
cargo test --workspace --all-features
```

需要复现 QEMU 环境时，请参考[受控互操作流程](docs/qemu-interoperability.zh-CN.md)。

## 文档

- [协议设计、能力矩阵、所有权和依赖策略](docs/protocol-design.zh-CN.md)
- [独立 helper IPC 契约](docs/helper-ipc.zh-CN.md)
- [受控 QEMU 互操作流程](docs/qemu-interoperability.zh-CN.md)

## 参与贡献

贡献应保持范围集中；涉及协议解析、字节边界或状态转换的改动应同时提供相应测试。

提交改动前请运行：

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
```

互操作报告应提供 QEMU 与 spice-server 版本、guest 与图形设备、端点安全模式和脱敏后
的日志。请勿提交 Ticket、密码、私钥或认证令牌。

## 许可证

OxideSpice 源码采用 [Apache License 2.0](LICENSE)。第三方库和系统库保留各自许可证；
重新分发二进制文件前，请检查 Cargo 元数据和依赖策略。
