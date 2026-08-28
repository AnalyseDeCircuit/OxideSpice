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
- The classic Display Canvas command set: Fill, Opaque, Copy/Blend, Blackness, Whiteness, Invers,
  ROP3, Stroke, raster Text, Transparent, and Alpha Blend, with bounded clip, mask, brush, scaling,
  path, and glyph handling.
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
| `oxide-spice-helper-protocol` | Versioned, bounded helper IPC types and codecs shared with host applications. Pure Rust with no native dependency. |
| `oxide-spice-helper` | Standalone stdio process plus host-owned WebDAV, USB/libusb, and PC/SC integrations. |

OxideSpice does not depend on a UI framework or application-specific host types.

## Native dependency boundaries

The SPICE wire protocol remains Rust-owned. Some production codec, raster, cryptography, and device
boundaries intentionally use native code:

| Boundary | Dependency model |
| --- | --- |
| Draw Composite | `pixman` through `pixman-sys`; official helper artifacts build the pinned source statically. |
| TLS | `ring`, which builds bundled C and assembly when `tls-ring` is enabled. |
| SASL GSSAPI | MIT/Heimdal GSSAPI on Linux, the system GSS framework on macOS, and native SSPI Kerberos on Windows. |
| Opus | Bundled BSD-licensed libopus through `opusic-sys` and CMake. |
| H.264 | Bundled BSD OpenH264 C++ and assembly. |
| VP8/VP9 | Pinned libvpx through `env-libvpx-sys` and bindgen; official helper artifacts link it statically. |
| USB redirection | Dynamic usbredir/libusb through `usbredirhost`. |
| Smartcard | `pcsc-sys`; Linux artifacts bundle the pinned PCSC-Lite client library while the daemon remains a system service. macOS and Windows use their platform PC/SC APIs. |

No native SPICE client library is linked. See the
[dependency policy](docs/protocol-design.md#dependency-policy) for details and license boundaries.

## Build requirements

- Rust 1.94.1 as selected by `rust-toolchain.toml`.
- A C/C++ toolchain and CMake for bundled native codecs and cryptography.
- `pkg-config` and libclang/bindgen support where required.
- Development packages for pixman, libvpx, Kerberos/GSSAPI, usbredir/libusb, and PC/SC for ordinary
  local builds, or the pinned native-source pipeline in `scripts/` for artifact builds.

`oxide-spice-client` enables `composite-pixman`, `audio-opus`, `sasl-gssapi`, `video-h264`,
`video-h265`, and `video-vpx` by default. Each boundary can be disabled independently. A
no-default-features build retains the Rust wire stack, password-based SASL, classic Canvas, image
codecs, raw audio, and MJPEG without linking pixman, GSSAPI, libopus, OpenH264, libvpx, or the H.265
decoder.

`oxide-spice-helper` defaults to the complete integration set. Official artifacts require that
default set and reject a binary whose Hello capability list is missing TLS, Kerberos, Pixman, media,
clipboard, file transfer, WebDAV, USBredir, smartcard, or multi-display support. Feature-reduced
builds are development tools and are not release artifacts.

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

The first request is a credential-free `Hello`. The helper flushes `HelloAck` with its IPC version,
helper version, target triple, and complete compiled capability list before it accepts `Connect` or
reads Ticket and SASL credentials. Host adapters should depend on `oxide-spice-helper-protocol`
rather than duplicating the wire structures.

The stdio helper disables GL scanout because stdin/stdout cannot carry DMA-BUF file descriptors.
Applications that need zero-copy scanout can use the client API directly or provide an explicit
Unix descriptor side channel.

See the [helper IPC contract](docs/helper-ipc.md) for framing, limits, ordering, and request/event
semantics.

## Precompiled helper artifacts

`Full helper artifacts` builds macOS, Linux, and Windows for x86-64 and ARM64 from the pinned native
source manifest. Each archive contains the helper, replaceable usbredir/libusb libraries, the Linux
PCSC-Lite client library where applicable,
`helper-metadata.json`, a CycloneDX SBOM, license texts, and third-party notices. Linux and macOS
artifacts use relative runtime paths; Windows DLLs are placed beside the executable. Manually
dispatched branch builds remain unsigned temporary candidates.

Permanent releases use an existing `v<workspace-version>` tag that points to a commit contained in
`main`. Dispatch `Full helper artifacts` with that tag selected as the workflow ref. The workflow
rebuilds all six targets from the tagged commit, signs every SHA-256 file in the protected
`helper-signing` environment, validates the complete asset and metadata contract, and creates a
GitHub Release without replacing an existing release or asset. The workflow never creates tags.

Release signing requires the following repository settings:

- `helper-signing` environment secret `MINISIGN_SECRET_KEY`, containing an unencrypted Minisign
  secret key generated with `minisign -G -W`;
- repository Actions variable `MINISIGN_PUBLIC_KEY`, containing the single `RW...` public-key line;
- a `helper-signing` deployment rule that permits only version tags such as `v*`; required reviewers
  are recommended for the signing environment.

## Verification

Source-check every crate, target, and feature combination:

```sh
cargo check --workspace --all-targets --all-features
```

Run the protocol and state tests:

```sh
cargo test --workspace --all-features
```

Check the client without optional raster or media backends:

```sh
cargo check -p oxide-spice-client --no-default-features --all-targets
```

With `cargo-fuzz` and LLVM/libFuzzer installed, exercise the bounded wire parsers with:

```sh
cargo fuzz run protocol_boundaries
```

`libfuzzer-sys` is confined to the standalone `fuzz` workspace and is not a library or release
dependency.

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
