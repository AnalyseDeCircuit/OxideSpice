# Third-party notices

OxideSpice is distributed under Apache-2.0. A packaged helper also contains or links third-party
components under their own terms. The SBOM shipped beside the executable is the authoritative
version inventory for that artifact.

The full helper statically links Pixman (MIT), libvpx (BSD-3-Clause), OpenH264 (BSD-2-Clause),
libopus (BSD-3-Clause), and ring (Apache-2.0 and ISC). It dynamically links usbredir and
libusb (LGPL-2.1-or-later) so that recipients can replace those libraries. Linux artifacts also
carry the MIT Kerberos GSSAPI runtime and the BSD-licensed PCSC-Lite client library; macOS uses the
system GSS framework and Windows uses SSPI. Smartcard access still uses the operating system PC/SC
daemon or service.

Copyright notices and complete license texts for bundled native libraries are included in the
`licenses` directory of each artifact. Rust package license expressions and source coordinates are
listed in `oxide-spice-helper.cdx.json`.
