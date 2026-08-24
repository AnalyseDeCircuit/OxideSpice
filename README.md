# OxideSpice

[English](README.md) · [简体中文](README.zh-CN.md)

OxideSpice is a standalone SPICE client protocol stack for Rust. Its checked wire-protocol core is
pure Rust, and it does not use `spice-gtk`, `libspice-client-glib`, or another C/FFI SPICE client.
The project also provides a bounded helper process for applications that want process isolation
from SPICE session, codec, filesystem, and native-device ownership.

OxideSpice is licensed under [Apache-2.0](LICENSE).

## Highlights

- Checked SPICE Link, capability, authentication, and message framing with bounded allocations.
- TCP and Unix-domain transports, Ticket authentication, SASL, and caller-configured rustls TLS.
- Main, Display, Cursor, Inputs, Agent, Playback, Record, Port, WebDAV, USBredir, and Smartcard
  channel ownership.
- Raw and indexed bitmaps, Composite/A8 rendering, LZ, GLZ, zlib-GLZ, LZ4, JPEG, JPEG-alpha, QUIC,
  MJPEG, VP8, H.264, VP9, and H.265 display paths.
- Multiple displays, clipboard selections and binary formats, outgoing Agent file transfer, audio
  state, and monitor configuration.
- Raw and Opus Playback/Record, ordinary Port byte streams, authorized WebDAV roots, USB
  redirection, and PC/SC smartcards.
- Semi-seamless and seamless migration with generation-aware state replacement.
- Linux Unix-socket DMA-BUF scanout in the reusable client API.
- Explicit cancellation, bounded queues, coalesced frames, and no blocking codec work on network
  owner tasks.
- Protocol behavior derived from the SPICE specification, `spice-protocol` definitions, and
  observable QEMU/spice-server behavior.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `oxide-spice-protocol` | Dependency-light wire constants, semantic types, checked parsers, and encoders. No I/O runtime or native dependency. |
| `oxide-spice-codecs` | Bounded image, video, and audio codec implementations and adapters. |
| `oxide-spice-client` | Async transports, authentication, sessions, channels, surfaces, migration, and cancellation. No UI framework dependency. |
| `oxide-spice-helper` | Standalone stdio process plus host-owned WebDAV, USB/libusb, and PC/SC integrations. |

OxideSpice does not depend on a UI framework or application-specific host types.

## Native dependency boundaries

The SPICE wire protocol remains Rust-owned. Some production codec, raster, cryptography, and device
boundaries intentionally use native code:

| Boundary | Dependency model |
| --- | --- |
| Composite/A8 | System `pixman` through `pixman-sys` and `pkg-config`. |
| TLS | `ring`, which builds bundled C and assembly when `tls-ring` is enabled. |
| SASL GSSAPI | System Kerberos/GSSAPI through `libgssapi-sys`. |
| Opus | Bundled BSD-licensed libopus through `opusic-sys` and CMake. |
| H.264 | Bundled BSD OpenH264 C++ and assembly. |
| VP8/VP9 | System libvpx through `env-libvpx-sys`, `pkg-config`, and bindgen. |
| USB redirection | Dynamic usbredir/libusb through `usbredirhost`. |
| Smartcard | Platform PC/SC through `pcsc-sys`. |

No native SPICE client library is linked. See the
[dependency policy](docs/protocol-design.md#dependency-policy) for details and license boundaries.

## Build requirements

- A stable Rust toolchain with Rust 2024 edition support.
- A C/C++ toolchain and CMake for bundled native codecs and cryptography.
- `pkg-config` and libclang/bindgen support where required.
- Development packages for pixman, libvpx, Kerberos/GSSAPI, usbredir/libusb, and PC/SC when building
  the crates that use those integrations.

The protocol crate can be built independently without those native packages:

```sh
cargo build -p oxide-spice-protocol
```

Build the complete workspace with:

```sh
cargo build --workspace --all-features
```

## Library quick start

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

The repository includes a controlled first-frame probe:

```sh
OXIDE_SPICE_TICKET='<ticket>' \
  cargo run -p oxide-spice-client --example first_frame -- \
  127.0.0.1 5900 first-frame.ppm
```

Use a permission-restricted secret source in real deployments; avoid placing Tickets in process
arguments or committed configuration.

## Standalone helper

Run the helper as a child process:

```sh
cargo run -p oxide-spice-helper -- --stdio
```

The parent writes requests to stdin and reads events from stdout. The bounded protocol uses JSON
headers and raw binary payloads for frames, cursor shapes, clipboard data, PCM, file-transfer
chunks, and Port data. It exposes:

- connection status, server identity, topology, RGBA frame regions, and cursor state;
- confirmed mouse mode, keyboard modifiers, input, clipboard, and monitor configuration;
- Agent state, file transfer, audio volume, and graphics-device mappings;
- Playback/Record data and settings plus ordinary Port byte streams;
- explicit WebDAV directory authorization and helper-owned USB/PCSC discovery.

The stdio helper disables GL scanout because stdin/stdout cannot carry DMA-BUF file descriptors.
Applications that need zero-copy scanout can use the client API directly or provide an explicit
Unix descriptor side channel.

See the [helper IPC contract](docs/helper-ipc.md) for framing, limits, ordering, and request/event
semantics.

## Verification

Source-check every crate, target, and feature combination:

```sh
cargo check --workspace --all-targets --all-features
```

Run the protocol and state tests:

```sh
cargo test --workspace --all-features
```

For a reproducible QEMU setup, see the
[controlled interoperability procedure](docs/qemu-interoperability.md).

## Documentation

- [Protocol design, capability matrix, ownership, and dependency policy](docs/protocol-design.md)
- [Standalone helper IPC contract](docs/helper-ipc.md)
- [Controlled QEMU interoperability procedure](docs/qemu-interoperability.md)

## Contributing

Contributions should remain focused and include tests when they change protocol parsing, wire
boundaries, or state transitions.

Before opening a change, run:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
```

Interoperability reports should include the QEMU and spice-server versions, guest and graphics
device, endpoint security mode, and sanitized logs. Never include Tickets, passwords, private keys,
or authentication tokens.

## License

OxideSpice source code is licensed under the [Apache License 2.0](LICENSE). Third-party and system
libraries retain their own licenses; consult Cargo metadata and the dependency policy before
redistributing binaries.
