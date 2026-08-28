# OxideSpice protocol and implementation design

[English](protocol-design.md) · [简体中文](protocol-design.zh-CN.md)

## Scope and evidence

OxideSpice is a general SPICE client stack. The protocol and client crates do not depend on
a UI framework, application-specific host types, a process helper, or a C SPICE client
implementation. The normative inputs are:

- [SPICE protocol overview](https://www.spice-space.org/spice-protocol.html)
- [spice-protocol wire constants](https://gitlab.freedesktop.org/spice/spice-protocol/-/blob/master/spice/protocol.h)
- [spice-protocol enums](https://gitlab.freedesktop.org/spice/spice-protocol/-/blob/master/spice/enums.h)
- [spice-common wire schema](https://gitlab.freedesktop.org/spice/spice-common/-/blob/master/spice.proto)
- [spice-server link behavior](https://gitlab.freedesktop.org/spice/spice/-/blob/master/server/reds.cpp)
- [QEMU SPICE options](https://qemu-project.gitlab.io/qemu/system/invocation.html)

## Connection flow

Each SPICE channel owns one independent TCP or TLS stream. TLS, when selected, completes before
the SPICE link handshake. There is no in-protocol StartTLS transition.

1. The client sends a 16-byte little-endian Link Header: `REDQ`, major version 2, minor version 2,
   and the byte length of the following link message.
2. The client sends the packed link message: connection id, channel type, channel id, common
   capability word count, channel capability word count, capability offset, then the two word
   arrays. A new Main channel uses connection id zero. Other channels use the session id returned
   by Main Init.
3. The server returns its Link Header and Link Reply. The fixed reply is 178 bytes and includes a
   162-byte DER RSA public key plus its capability arrays. All counts, offsets, additions, and
   allocations are checked against local limits before reading or allocating.
4. If both peers advertise `AUTH_SELECTION`, the client selects Ticket authentication with the
   four-byte mechanism value 1 unless the caller configured SASL and the peer advertises it. SASL
   supports GSSAPI, SCRAM-SHA-512/256/1, PLAIN, and LOGIN. Plain TCP requires a negotiated SASL
   security layer; TLS and Unix transports may rely on their external protection.
5. Ticket authentication encrypts the NUL-terminated password with the supplied 1024-bit RSA key,
   PKCS#1 OAEP using SHA-1, MGF1, and an empty label. The encrypted ticket is exactly 128 bytes.
   The client then reads the four-byte link result.
6. Normal channel framing is fixed for the life of the stream. If both peers advertise
   `MINI_HEADER`, messages use a six-byte type/size header. Otherwise they use the 18-byte full
   header containing serial, type, size, and sub-message-list offset. A decoder never guesses the
   framing per message.
7. Main Init supplies the session id, display hint, mouse modes, agent state, multimedia time, and
   RAM hint. The client sends Attach Channels, then accepts Channels List. A list entry is a
   `(channel type, channel id)` pair. Lists may be repeated and a type may have multiple ids.
8. Each selected child channel performs the full link and authentication flow on a new transport.
   Display sends Display Init before consuming display traffic. Inputs and the Cursor channel id
   paired with the selected Display are optional; an advertised channel that fails Link is a
   terminal connection error rather than a silent downgrade.

Link errors are terminal for that channel. `NEED_SECURED` and `NEED_UNSECURED` are transport-policy
results, not permission to silently retry with weaker identity verification.

## Common channel behavior

The read loop performs `exact header -> validated bounded body -> dispatch`; it never reads to EOF
or grows an unbounded staging buffer. Unknown non-stateful messages can be skipped after their
bounded body is read. Unknown messages that may mutate display caches, surfaces, agent flow
control, or migration state terminate that channel with a protocol error.

- Set Ack installs `(generation, window)`. The client immediately sends Ack Sync and sends Ack for
  every consumed window. A zero window disables periodic Ack.
- Ping is answered with Pong carrying the same id and timestamp.
- Wait For Channels uses a fixed session registry keyed by `(channel type, channel id)`. Each owner
  publishes only after completing the message state transition; waiters sleep on the target's
  monotonic serial and remain cancellation-aware. Mini headers use the locally derived incoming
  serial, while full-header serials may skip values but may never regress.
- Disconnecting stops new work, records the remote reason, performs bounded cooperative cleanup,
  and closes the channel.
- Full-header sub-message lists and mini-header `SPICE_MSG_LIST` envelopes are completely validated
  before dispatch. Sub-messages run in list order before the main message, while ACK accounting and
  cross-channel progress advance once for the containing wire envelope. All offsets remain relative
  to the bounded current body and are never treated as host pointers.

## Capability matrix

The advertised set always reflects implemented behavior. Merely parsing a capability does not
make the feature supported.

| Area | Protocol capabilities and dependency | Delivery policy |
| --- | --- | --- |
| Common | 0 auth selection, 1 Ticket, 2 SASL, 3 mini header | Always advertises auth selection, Ticket, and mini headers. SASL is advertised only when the caller supplies a SASL policy. |
| Main | 0 semi-seamless migration, 1 name/UUID, 2 agent connected tokens, 3 seamless migration | Advertises all four. Target channels are prelinked under the source session id, queued by migration generation, and activated only by channel migration or Main migration completion. |
| Display | 0 sized stream, 1 monitors, 2 composite, 3 A8 surface, 4 stream report, 5 LZ4, 6 preferred compression, 7 GL scanout, 8 multi-codec, 9 MJPEG, 10 VP8, 11 H.264, 12 preferred video codec, 13 VP9, 14 H.265, 15 GL scanout 2 | Advertises Composite, A8, LZ4, stream and codec paths. GL scanout bits are advertised only for a Linux Unix-socket endpoint that can receive DMA-BUF descriptors through `SCM_RIGHTS`. |
| Cursor | No channel-specific bits; independent shape cache and set/move/hide/trail messages | Implements alpha, mono, color4, color8, color16, color24, and color32 shapes, cache/invalidation, reset/init ordering, and latest complete state. Destination-invert pixels use the established checker fallback because a static RGBA hardware cursor cannot express framebuffer inversion. |
| Inputs | Bit 0 key scancode; legacy key and relative/absolute mouse messages remain available | Implemented with negotiated raw scancodes, legacy keys, both confirmed mouse modes, discrete buttons, modifier state, and motion ACK flow control. |
| Agent | Main-channel tunnel with its own capability words and 2,048-byte data fragments; token controlled | Implements sparse monitor layouts, selection-aware multi-format clipboard data, WebDAV file-list payloads, bidirectional audio volume, graphics-device mappings, and detailed file-transfer errors with generation isolation. |
| Playback | CELT, volume, latency, Opus; raw S16 exists without a codec bit | Advertises Opus, volume, and latency; decodes bundled-libopus packets into bounded interleaved S16LE delivery and publishes latest gain, mute, and latency settings. Raw remains supported; obsolete CELT stays unadvertised. |
| Record | CELT, volume, Opus; raw S16 exists | Advertises Opus and volume, selects Opus only when the server and requested stereo rate permit it, buffers exact 480-sample frames, encodes through bundled libopus, and publishes latest gain and mute settings. Raw capture remains available. |
| USB redirection | SpiceVMC framing plus optional LZ4; payload is the separate usbredir protocol | Advertises SpiceVMC LZ4 and exposes a bounded reliable raw stream so exactly one backend owns Hello and device state. The helper integrates dynamic usbredirhost and libusb with a dedicated event worker. The pure-Rust packet parser remains available in the protocol crate. |
| Smartcard | VSC messages, conditionally built by spice-server | Links and supervises typed bounded VSC messages. The helper integrates PC/SC reader discovery, ReaderAdd/ATR, APDU transfer, Flush, and error responses. |
| Port | SpiceVMC bytes plus port name/open/close/break state | Exposed as a bounded bidirectional byte stream without interpreting the application protocol. LZ4 is negotiated and used in both directions when it reduces the wire size. |
| WebDAV | Port subtype for `org.spice-space.webdav.0` | The client remains an opaque bounded byte stream. The helper bridges it to an HTTP/1 WebDAV handler and maps only the caller-authorized root, with explicit read-only or read-write methods. |
| Multiple displays | Display Monitors Config plus Agent monitor configuration; multiple Display channel ids are legal | Identity is `(display channel id, monitor id)`. Desired guest layouts are coalesced and replayed after Agent reconnect; unsupported physical-size fields are omitted. |
| Clipboard | Agent on-demand, selection, serial, re-grab and maximum-size features | UTF-8, PNG, BMP, TIFF, JPEG, and file-list ownership use one generic eight-MiB path. File lists validate `copy`/`cut` plus NUL-terminated WebDAV-absolute UTF-8 paths. |
| File transfer | Agent Start/Status/Data with token flow control; clipboard file lists instead use WebDAV | Outgoing basename-only transfers are implemented with at most eight active identities, 64-KiB chunks, one fully fragmented chunk in flight per owner, terminal status tracking, and explicit cancellation. Filesystem I/O remains outside the client crate. |

### Image and stream formats

The classic Canvas path implements messages 302 through 313. A shared renderer applies inline
rectangle clips, positioned QMask bitmaps, solid and repeating pattern brushes, nearest or
interpolated scaling, binary ROP descriptors, and arbitrary ROP3 truth tables. Stroke consumes
bounded fixed28.4 paths with cosmetic line, dash, close, and Bezier handling. Text consumes bounded
A1/A4/A8 raster glyphs. The client advertises a zero-byte pixmap cache, so cache-reference image
types are not part of the negotiated stream; palette and GLZ caches remain independently bounded.

| Format | Negotiation reality | Policy |
| --- | --- | --- |
| Bitmap | Baseline image type; no opt-out capability | Supports checked direct-color RGB555, BGR24, xRGB32, and ARGB32 plus big-endian 1/4-bit and 8-bit indexed updates. Inline and cached palettes are bounded and obey single/all invalidation. |
| LZ / GLZ | Baseline image types; no opt-out capability. GLZ has shared history and cross-channel/cache ordering | LZ 1.1 is implemented in Rust for all header formats. GLZ supports RGB16/24/32 and split-alpha RGBA with a bounded session dictionary, cross-image references, out-of-order multi-Display waits, and contiguous-id eviction. Palette GLZ is not emitted by the `GLZ_RGB` outer image form. |
| zlib-GLZ | Baseline image type | Implemented with bounded streaming `miniz_oxide`, exact declared GLZ length, checksum validation, cancellation between 64-KiB output chunks, and no `libz-sys`. |
| LZ4 | Server may send only when Display bit 5 is advertised | Advertised and decoded with checked dictionary-linked row blocks, exact dimensions, bounded working memory, cancellation, RGB16/24/32, and RGBA conversion. `lz4_flex` is safe Rust and has no `-sys` dependency. |
| JPEG / JPEG-alpha | Baseline image types; no opt-out capability | Baseline JPEG uses the pure-Rust `zune-jpeg` path with strict dimensions and cooperative cancellation. Progressive Huffman DCT uses bounded pure-Rust `jpeg-decoder` and expands RGB to RGBA in place. JPEG-alpha validates its LZ `XXXA` plane before merging alpha. |
| QUIC image | SPICE's SFALIC-family image codec, unrelated to the IETF transport protocol | Implemented in Rust for RGB16, RGB24, RGB32, and RGBA, including bounded Golomb escape codes, adaptive models, 2,048-pixel model transitions, cross-row prediction, and MEL runs. Grayscale is rejected because the SPICE canvas display path does not accept it. |
| Video streams | Multi-codec changes capability semantics; without it servers assume legacy MJPEG support | Sized and fixed geometry, clip replacement, destruction, report windows, and preferred codec order are implemented. Each of at most 16 streams owns a one-command decoder worker so native or CPU-heavy decode never blocks the network task. |
| GL scanout | Unix descriptor and DMA-BUF path | Linux Unix sockets receive bounded `SCM_RIGHTS` descriptors for one- or multi-plane scanout. The host receives an owned frame and `GL_DRAW_DONE` is sent only after completion or drop. TCP/TLS sessions do not advertise these bits. |

The retained QUIC tests use streams produced and decoded byte-for-byte by `spice-common` commit
`71e45706981973014eaab3d4b533d35d79e19ffa`: RGB32 1x1, RGB24 cross-row prediction, RGB32 MEL
run, RGBA split alpha, and RGB16 5-bpc cross-row cases. A generated RGB32 4097x2 case also passed
exact pixel comparison across the 2,048 and 4,096 model boundaries; its SHA-256 is
`e9a5664fe283beee3b2ef8f62241e77f2dea3d55459a2d4b6f127e27e5d573fe`. The official encoder is
used only to generate research vectors and is not compiled by Cargo or linked into OxideSpice.

The first QEMU interoperability fixture deliberately uses
`image-compression=off,jpeg-wan-compression=never,zlib-glz-wan-compression=never,streaming-video=off`.
Passing that fixture proves the vertical path, not general production display compatibility.

## State machines and ownership

The session supervisor owns the attempt generation, Main owner, child-channel registry, cancellation
source, and every task handle. Each channel task exclusively owns its transport. No socket is shared
behind a mutex.

```text
Idle
  -> ConnectingMain
  -> LinkingMain
  -> AuthenticatingMain
  -> AwaitingMainInit
  -> DiscoveringChannels
  -> Running
       -> PreparingMigration -> ConnectingTarget -> Switching -> Running
       -> Reconnecting -> ConnectingMain
  -> Closing
  -> Closed
```

Any transition can enter `Failed` with a typed terminal cause. A new connection or migration
attempt increments the generation. Events from an older generation are ignored after ownership
moves to the new attempt.

Each channel independently moves through `Transport -> Link -> Auth -> Active -> Draining ->
Closed`. Cancellation stops command acceptance, closes the channel's transport, and joins the task.
The supervisor listens to cancellation independently of socket reads and writes. A channel gets a
two-second cooperative cleanup window; a task that remains stuck is aborted and still awaited.
`Session::shutdown` completes only after all channel tasks have been reaped.

Migration is session-owned. The destination Main and every existing child identity are linked with
the source session id before the source is acknowledged. Display and Record target transports send
their required initialization while pending. Per-channel `MIGRATE_FLUSH_MARK` and opaque
`MIGRATE_DATA` are ordered before replacement traffic, and effective receive serials remain
monotonic across transport-local serial restarts. Seamless migration preserves surfaces, caches,
streams, Agent state, and device protocol generations after the destination ACK. Destination NACK
falls back to the semi-seamless completion path. Semi-seamless activation resets server-owned
display, input, cursor, audio, Port, Agent, USBredir, and Smartcard state; queued device input is
labeled with a transport generation so a host parser cannot join bytes from different servers.
Cancellation invalidates the target generation before any queued replacement can activate.
Legacy `MIGRATE_SWITCH_HOST` performs a fresh Main link with connection id zero, bootstraps the new
session, requires the managed child-channel topology to match, updates the observable session id,
and immediately replaces every old transport without waiting for source EOF.

The guest Agent is subordinate to the Main owner and moves through `Disconnected -> Negotiating ->
Ready`. `AGENT_CONNECTED_TOKENS` replaces outbound credit, while later Agent Token messages add
credit. Each receive token covers one Main AgentData fragment, not one logical Agent message.
Disconnect or reconnect resets partial reassembly, clipboard serials, pending reads, held credits,
and file transfers. Desired monitor layout and local clipboard ownership are replayed in the new
Agent generation; file transfers are not resumed.

## Bounded resources

Limits are named configuration values with conservative defaults and protocol-derived validation.

- Link bodies: at most 4 KiB, matching current spice-server behavior.
- Capability counts: bounded before multiplication and allocation.
- Normal message body, sub-message count, image dimensions, surface bytes, decoder output, cache
  bytes, agent message, clipboard item, file-transfer window, and port bytes: independently bounded.
- Agent logical messages are limited to 16 MiB and reassembled directly into their declared body.
  The reusable completion list and 2,048-byte outbound fragment buffer avoid a fresh allocation for
  each Main AgentData fragment. Clipboard type arrays are capped at 64 entries.
- The Agent starts with ten receive-fragment credits. A reliable host clipboard request retains the
  corresponding credit until its event lease is dropped, applying protocol backpressure without an
  unbounded event queue. Credit generation changes and pending-credit reset are atomic, so a late
  event from a disconnected Agent cannot replenish the new stream.
- Outgoing file transfer metadata contains a validated guest basename and declared size, never a
  host path. Chunks are limited to 64 KiB and the streaming owner cannot submit the next chunk until
  the current logical message is completely fragmented onto Main. Agent disconnect marks every
  active transfer terminal and stale generation commands cannot enter the replacement stream.
- Raw Playback accepts at most 32 interleaved channels, a 384-kHz sample rate, and 256 KiB per
  packet. The network task uses a 16-item nonblocking queue; overflow drops real-time packets and
  marks the next delivered packet as discontinuous instead of stalling SPICE control traffic.
- Raw Record sends Mode before every other client Record message. Start Mark is emitted only after
  a server Start and before PCM Data. Capture submissions are limited to 256 KiB, checked against
  the requested interleaved frame width, and serialized through a 16-item command queue. The host
  may use the session monotonic timestamp or supply its capture backend timestamp explicitly.
- Port names, pointed offsets, terminators, events, and 256-KiB Data messages are checked before
  ownership transfer. Host writes and break events use a 16-item bounded command queue. Port and
  WebDAV application bytes remain opaque, and no filesystem is owned by the client crate.
- usbredir transport chunks are bounded to one MiB and use a 16-item reliable queue. The client
  does not inject a competing Hello; the selected backend owns the complete usbredir stream. The
  helper's bounded worker drives usbredirhost/libusb while the protocol crate retains checked
  Hello, 32/64-bit header, and packet parsing for Rust backends.
- Network tasks never wait on UI presentation. Control and graphics delivery use separate bounded
  paths. Saturated graphics delivery coalesces dirty regions where semantics allow it; otherwise it
  requests a base-frame recovery. Key/button edges and state transitions are never dropped.
- Ordered input edges use a 128-entry queue with both async backpressure and explicit nonblocking
  failure. Absolute positions use a latest-only slot; relative deltas are accumulated. Pointer
  messages stop at two protocol ACK bunches in flight, while a button edge first flushes the current
  pointer state so clicks cannot land at an older coordinate.
- Cursor images have checked dimensions, a 256-entry protocol cache, and a four-MiB live byte
  budget. The byte charge follows host-retained `Arc` shapes after invalidation and is returned only
  when the final owner releases the image.
- Each Display channel has a 256-entry, 256-KiB palette cache. Palette pointers are resolved only
  inside the current bounded message; direct-color images cannot smuggle palette union flags, and a
  cache miss terminates the channel instead of rendering with guessed colors.
- SPICE LZ input, dimensions, stride, output bytes, literals, match lengths, and back-reference
  distances are checked before mutation. Decode work runs outside the socket task and polls session
  cancellation during long extension, copy, and pixel-conversion runs. All Display channels share
  one cancellation-aware decode slot, acquired before copying compressed input, so the per-image
  bound also remains a session-wide transient-memory bound.
- Every Display advertises the same GLZ dictionary id and 16-MiB window. A decoder missing an older
  image releases the shared decode slot before waiting for another Display transport to publish it,
  preventing dependency inversion. Eviction advances only across contiguous global GLZ ids, so an
  early image from another transport cannot discard data across an unresolved id gap.
- Zlib-wrapped GLZ is inflated under the same session decode slot. The wrapper rejects zero or
  oversized declared output, short expansion, expansion beyond the declaration, checksum failure,
  and trailing compressed bytes before the inner GLZ image can enter the dictionary.
- JPEG input is split from its optional alpha stream before allocation. Only baseline frames enter
  the pure-Rust decoder; descriptor dimensions, configured dimensions, exact RGBA output size, and
  cancellation are checked. JPEG-alpha requires a same-sized `XXXA` LZ plane with a matching row
  direction before any pixels reach a surface.
- QUIC input uses a checked little-endian word reader with most-significant-bit-first consumption
  inside each word. Header and descriptor dimensions are matched before allocation; Golomb escape
  values and MEL runs are bounded before indexing or copying. Decoder output is normalized to
  top-down RGBA without a second full-frame conversion copy.
- Frame payloads remain in owned surface storage. Notifications carry generation, surface identity,
  and dirty metadata rather than a full-frame copy. The latest-only notifier marks a full refresh
  as required when an earlier dirty notification may have been replaced.
- Display reset, surface recreation, migration, and reconnect increment a graphics epoch so stale
  updates cannot mutate a new frame.
- At most 16 Display transports are linked by default. They share one 256-MiB live surface budget;
  each frame and topology event carries `(connection generation, Display channel id, graphics
  epoch)` so equal surface ids on different channels cannot collide.

## Error model

Errors preserve a stable category and structured context while avoiding credentials and frame data:

- configuration and unsupported feature;
- DNS/network/transport timeout;
- TLS identity or policy;
- Ticket or SASL authentication;
- link negotiation and remote link result;
- malformed wire data or unsupported stateful message;
- decoder failure or unsupported image/stream format;
- local resource limit;
- remote disconnect;
- migration or reconnect attempt;
- local cancellation and shutdown timeout;
- internal task termination.

Unsupported stateful Display data is a terminal protocol/feature error. Continuing after silently
skipping it would produce a plausible but corrupted framebuffer.

## Crate boundary

The workspace contains five crates:

- `oxide-spice-protocol`: dependency-light byte types, constants, checked parsers, and encoders. It
  contains no I/O runtime, UI, filesystem, or codec implementation.
- `oxide-spice-codecs`: runtime-independent, bounded pure-Rust image decoding. It owns SPICE LZ
  1.1, GLZ, zlib wrapping, baseline JPEG, and SPICE QUIC, and provides cooperative cancellation
  without depending on Tokio or client state.
- `oxide-spice-client`: async transports, link authentication, session/channel ownership, ACK and
  cancellation state, surfaces, and bounded delivery.
- `oxide-spice-helper-protocol`: the pure-Rust, versioned helper IPC schema, bounded JSON/binary
  codec, pre-credential Hello negotiation, and artifact metadata types. Both the helper and an
  external host adapter use this crate.
- `oxide-spice-helper`: the standalone bounded stdio process plus host integrations for USB,
  PC/SC, and local WebDAV filesystem mapping. It exposes only SPICE semantics and remains
  independent of UI frameworks and application-specific host types. The host receives advertised
  channel ids before granting directory or device authority, and native device discovery remains
  inside this process.

## Dependency policy

The protocol, Ticket, LZ, GLZ, zlib, LZ4, JPEG, QUIC, and H.265 implementations are Rust. Baseline
JPEG uses `zune-jpeg`; progressive JPEG uses `jpeg-decoder`; H.265 uses `rust_h265`. Tokio and the
WebDAV local filesystem backend use Rust OS bindings such as `libc`, but no C SPICE client is linked.

Composite rendering uses the MIT-licensed `pixman`/`pixman-sys` binding. This native raster boundary implements the
operation, transform, filter, repeat, component-alpha, clip, and A8 semantics of Draw Composite;
no SPICE client library is linked. Official helper artifacts build the pinned Pixman source and
link it statically. Unix descriptor receipt uses safe `rustix` APIs and keeps all project crates
under `unsafe_code = "forbid"`.

The client features `composite-pixman`, `audio-opus`, `sasl-gssapi`, `video-h264`, `video-h265`,
and `video-vpx` select native raster, authentication, and media boundaries. They are enabled by
default for the complete client and can be disabled independently. Capability advertisement is
derived from the compiled feature set; a disabled codec or Composite backend is never advertised
to the server. The helper forwards these switches and separately gates `tls-ring`, `usbredir`,
`smartcard`, and `webdav`. Its IPC schema does not vary with the feature set; a request for a
backend omitted at build time returns an explicit action error.

SASL password mechanisms are provided by Rust `rsasl`. The Rust-owned RFC 4752 state machine uses
`cross-krb5`: Linux calls MIT or Heimdal GSSAPI through `libgssapi-sys` and official Linux artifacts
carry pinned MIT Kerberos; macOS calls the system GSS framework; Windows calls native SSPI Kerberos
without loading libgssapi. The native boundary is confined to Kerberos context, wrap, and unwrap
operations; SPICE SASL framing, layer selection, bounds, and record framing remain owned by the
Rust client.

TLS is an explicit `oxide-spice-client/tls-ring` feature. It disables `tokio-rustls` defaults and
selects `ring` plus TLS 1.2 support. `ring` compiles bundled C and assembly; that native code is
accepted at the transport-cryptography boundary and does not enter the protocol or decoder crates.
The available all-Rust RustCrypto provider is explicitly pre-production and is not used. The caller
owns the rustls certificate/verifier configuration, so enabling TLS never weakens identity checks.
Migration normally verifies the destination host name. A source-provided certificate subject is
accepted only through an explicit `MigrationTlsPolicy` that returns the caller's target-specific
server name and rustls configuration; without that policy the migration fails before connecting.
Native dependencies are isolated and explicit. `opus 0.4.0` uses `opusic-sys`, which compiles the
BSD-licensed bundled libopus with CMake and may include platform assembly. The helper uses
`usbredirhost 0.4.1`, transitively linking `usbredirparser-sys` and the system usbredir/libusb
libraries dynamically when its `usbredir` feature is enabled, and `pcsc 2.9.0`, which uses
`pcsc-sys` and the platform PC/SC service when `smartcard` is enabled. Linux artifacts build and
carry the pinned PCSC-Lite client and real delegate libraries; the pcscd socket and daemon remain a
system service. macOS and Windows use their platform PC/SC implementations.
usbredir/libusb are dynamically linked LGPL libraries and retain their own distribution terms.
`oxide-spice-protocol` has none of these dependencies. Display and SpiceVMC LZ4 use safe-Rust
`lz4_flex`, not `lz4-sys`.
VP8/VP9 use `vpx-rs -> env-libvpx-sys`; official artifacts build and statically link the pinned BSD
libvpx source, and the Rust build uses bindgen/libclang. H.264 uses
`openh264 -> openh264-sys2`, compiling bundled BSD OpenH264 C++ and assembly. Neither backend enters
`oxide-spice-protocol`. The helper's Apache-2.0 `dav-server` local filesystem feature uses the Rust
`libc` crate for platform filesystem calls; it does not bundle a native WebDAV implementation.

Native archive versions, download URLs, hashes, licenses, and linkage policy are recorded in
`native/dependencies.toml`. The six-target artifact workflow verifies every archive before
extraction, requires the complete helper capability contract, audits dynamic dependencies, fixes
relative runtime paths, and packages metadata, license texts, third-party notices, and a CycloneDX
SBOM. usbredir/libusb remain dynamically replaceable in accordance with their LGPL terms.
