# OxideSpice helper IPC

[English](helper-ipc.md) · [简体中文](helper-ipc.zh-CN.md)

`oxide-spice-helper --stdio` owns one SPICE session. The parent writes requests to stdin and reads
events from stdout. Diagnostic text is written only to stderr. The contract contains SPICE domain
types and has no dependency on a UI framework or application-specific host types.

The first request must be `Connect`. EOF or `Close` begins orderly shutdown. The helper then stops
native integrations, shuts down every SPICE channel task, emits `closing` and `disconnected`
statuses, drains the event writer, and exits.

## Framing and limits

Ordinary messages are one UTF-8 JSON object followed by `\n`. Field and variant names use camel
case. A JSON line is limited to 1 MiB. Large frame, cursor, clipboard, and PCM values use a JSON
header followed immediately by exactly `payloadLen` raw bytes; there is no separator after the
payload. A binary payload is limited to 256 MiB. Readers must consume the declared byte count before
reading the next JSON line.

`FrameBinary` and `CursorBinary` carry tightly packed RGBA8 pixels. Their payload length must equal
`width * height * 4`. Playback and Record samples are interleaved signed 16-bit little-endian PCM.
The codec rejects arithmetic overflow, oversized values, truncated payloads, and metadata/payload
length mismatches.

Passwords and Tickets are redacted from Rust debug output and their owned allocations are cleared
when dropped. TLS accepts caller-supplied DER trust anchors and a required server name. SASL accepts
GSSAPI or password credentials; enabling GSSAPI uses the disclosed system GSSAPI boundary.

## Connection sequence

A plain TCP connection starts with:

```json
{"type":"connect","options":{"endpoint":{"type":"tcp","host":"127.0.0.1","port":5900},"ticket":"","transportSecurity":{"type":"plain"},"sasl":null}}
```

The helper emits `connecting`, then either a categorized `error` and `failed`, or these successful
events in order:

1. `connected`, including the session id and discovered Inputs, Cursor, Agent, Playback, Record,
   Port, USBredir, Smartcard, and WebDAV channel capabilities. Inputs also reports whether raw
   scan codes were negotiated.
2. `serverIdentity`, containing the optional server name and UUID.
3. `status` with `connected`.

Display topology and framebuffer events can follow immediately. Mouse-mode and keyboard-modifier
events tell the host whether to send absolute or relative pointer input and synchronize lock keys.
A frame includes connection and
graphics generations so the host can discard stale work across reset or migration. Dirty regions
are normally delivered without copying the full surface. If stdout backpressure replaces a queued
frame, the replacement is a full primary-surface snapshot.

Cursor shape bytes are sent only when the cursor epoch or shape id changes. Clipboard transfers use
selection and format values rather than assuming UTF-8 text. Guest clipboard requests carry a
request id which must be returned by `ClipboardProvideBinary`. Agent readiness and negotiated
features, guest audio-volume changes, and graphics-device mappings have structured events;
`SyncAgentAudioVolume` carries the reverse volume update. Playback packets include stream
generation, sequence, timestamp, format, and discontinuity state. Playback and Record state plus
volume, mute, and Playback latency use separate latest-state events. Record input is sent with
`RecordDataBinary` after `RecordBegin` and the corresponding `recordState` event.

Outgoing Agent file transfer is host-streamed and does not grant filesystem authority. The host
starts a transfer with its own unique `transferId`, sends at most 64 KiB per
`FileTransferDataBinary`, and may finish or cancel it. `fileTransferState` reports accepted bytes,
terminal state, and structured guest failure details. The helper owns the guest transfer id and a
four-command bounded queue; at most eight transfers are active. A non-empty transfer can complete
after its declared final chunk, while `FileTransferFinish` is required to send a zero-length file.

## Native authority

Native integrations are opt-in after `connected`, because their server-assigned channel ids do not
exist before channel discovery.

Ordinary SPICE Port channels do not grant local device or filesystem access, so the helper starts
a bounded byte bridge for each one automatically. `portState`, `PortDataBinary`, and `portBreak`
carry server-to-host state and data; `PortWriteBinary` and `PortBreak` carry the reverse direction.
The 256 KiB SPICE Port message bound is enforced before allocating a declared binary payload.

- `ListNativeDevices` returns `nativeDevices` with libusb identities and PC/SC display names.
- `StartWebDav` grants one advertised WebDAV channel access to one local directory and explicitly
  selects read-only or read-write methods.
- `StartUsbRedirection` pairs one advertised USBredir channel with one enumerated bus/address and
  vendor/product identity.
- `StartSmartcardRedirection` pairs one advertised Smartcard channel with one enumerated PC/SC
  reader name.

An unknown, duplicate, or already active channel id produces an error rather than selecting a
default device. Unconfigured channels retain transport ownership but receive no filesystem or
physical-device authority. Native work is bounded to 64 concurrent helper tasks and is cancelled
with the session.

The stdio process disables GL scanout capability because stdin/stdout cannot carry DMA-BUF file
descriptors. The reusable client still supports DMA-BUF over Linux Unix sockets. Zero-copy helper
integration requires an explicit Unix descriptor side channel; an integer file descriptor cannot
be encoded as a transferable JSON value.
