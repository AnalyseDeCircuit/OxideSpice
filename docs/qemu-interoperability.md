# Controlled QEMU interoperability run

[English](qemu-interoperability.md) · [简体中文](qemu-interoperability.zh-CN.md)

The first vertical path requires a QEMU build with SPICE support and a guest that produces QXL
display traffic. Compression and video remain disabled in that fixture so the initial raw-bitmap
result stays independently observable. A second matrix run should use LZ and must observe both
`LZ_RGB` and `LZ_PLT` before marking LZ interoperability complete.

Use the equivalent of these QEMU SPICE options in the fixture:

```text
port=<PORT>,addr=127.0.0.1,disable-ticketing=off,password-secret=spice-ticket,\
image-compression=off,jpeg-wan-compression=never,\
zlib-glz-wan-compression=never,streaming-video=off
```

Create QEMU's `spice-ticket` secret object from a permission-restricted file; do not place the
Ticket itself in a checked-in script or command history. Export the same value only for the probe
process:

```text
-object secret,id=spice-ticket,file=/path/to/restricted-ticket-file
-spice <the comma-separated options above>
```

```sh
OXIDE_SPICE_TICKET='<ticket>' \
  cargo run -p oxide-spice-client --example first_frame -- \
  127.0.0.1 <PORT> first-frame.ppm
```

The Display run is accepted when:

1. QEMU accepts both the Main and Display Ticket handshakes.
2. Main Init supplies a non-zero session id and Channels List advertises Display.
3. The probe writes a non-empty PPM with the expected guest dimensions and visible pixels.
4. The probe exits without a forced timeout, and QEMU reports both client channels disconnected.

The interactive extension additionally requires QEMU to advertise Inputs and the Cursor channel
matching the selected Display id. Acceptance requires an alpha/color16/color32 cursor update, an
absolute move after confirmed client mouse mode, relative motion while server mode is confirmed,
key and button edges, motion ACK recovery, and clean closure of all four transports.

For TLS, compile with `oxide-spice-client/tls-ring` and supply a rustls `ClientConfig` whose roots or
custom verifier enforce the intended QEMU identity. The feature intentionally compiles ring's C and
assembly at the transport boundary. It does not add OpenSSL or any C SPICE implementation.

## Automated test coverage

The repository test suite exercises Main, two Display channels, Inputs, Cursor control-before-init,
monitor topology, reset epochs, 16/24/32-bit direct-color parsing, padded cursor data, raw indexed
palette rendering, `LZ_RGB32`, `LZ_PLT8`, `GLZ_RGB32`, `ZLIB_GLZ_RGB`, palette invalidation,
pointer-before-button ordering, raw Playback/Record, bidirectional Port bytes, usbredir Hello and
generic packet framing, and nine-task shutdown.
Codec tests cover LZ and GLZ row
orientation, split alpha, overlap and cross-image references, extended lengths, malformed
references, output limits, and cancellation boundaries. The fake peer also exercises Main Agent
token negotiation, clipboard exchange, monitor layout, file success/cancel, and Agent reconnect
replay. It sends a GLZ reference on
Display 0 before its zlib-wrapped base image on Display 1, verifying that wrapper decoding publishes
to the shared dictionary and that the wait releases the decode slot. It also blocks Display 0 on a
Display 1 serial barrier before releasing the dependent stream.
