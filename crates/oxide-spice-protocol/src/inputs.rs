use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Inputs server-to-client message identifiers.
pub mod inputs_server {
    pub const INIT: u16 = 101;
    pub const KEY_MODIFIERS: u16 = 102;
    pub const MOUSE_MOTION_ACK: u16 = 111;
}

/// Inputs client-to-server message identifiers.
pub mod inputs_client {
    pub const KEY_DOWN: u16 = 101;
    pub const KEY_UP: u16 = 102;
    pub const KEY_MODIFIERS: u16 = 103;
    pub const KEY_SCANCODE: u16 = 104;
    pub const MOUSE_MOTION: u16 = 111;
    pub const MOUSE_POSITION: u16 = 112;
    pub const MOUSE_PRESS: u16 = 113;
    pub const MOUSE_RELEASE: u16 = 114;
}

/// Inputs channel capability bits.
pub mod inputs_capability {
    pub const KEY_SCANCODE: u32 = 0;
}

/// Number of pointer messages acknowledged by one server motion ACK.
pub const INPUT_MOTION_ACK_BUNCH: u32 = 4;

/// Keyboard lock flags reported by the Inputs channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardModifiers(u16);

impl KeyboardModifiers {
    pub const SCROLL_LOCK: Self = Self(1 << 0);
    pub const NUM_LOCK: Self = Self(1 << 1);
    pub const CAPS_LOCK: Self = Self(1 << 2);
    const KNOWN_BITS: u16 = Self::SCROLL_LOCK.0 | Self::NUM_LOCK.0 | Self::CAPS_LOCK.0;

    /// Creates a validated modifier set from its wire representation.
    pub fn from_bits(bits: u16) -> Result<Self, DecodeError> {
        if bits & !Self::KNOWN_BITS != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "keyboard modifier flags",
            ));
        }
        Ok(Self(bits))
    }

    /// Returns the exact flags used by SPICE wire messages.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Decodes the fixed two-byte server modifier body.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 2 {
            return Err(fixed_size_error(body.len(), 2, "keyboard modifiers"));
        }
        let mut reader = Reader::new(body);
        Self::from_bits(reader.u16("keyboard modifiers")?)
    }
}

/// Mouse button state flags carried alongside every pointer message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseButtons(u16);

impl MouseButtons {
    pub const LEFT: Self = Self(1 << 0);
    pub const MIDDLE: Self = Self(1 << 1);
    pub const RIGHT: Self = Self(1 << 2);
    pub const WHEEL_UP: Self = Self(1 << 3);
    pub const WHEEL_DOWN: Self = Self(1 << 4);
    pub const SIDE: Self = Self(1 << 5);
    pub const EXTRA: Self = Self(1 << 6);
    const KNOWN_BITS: u16 = (1 << 7) - 1;

    /// Creates a validated button state from protocol flags.
    pub const fn from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::KNOWN_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns the exact flags used by SPICE wire messages.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// Button identity used by discrete press and release messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Middle = 2,
    Right = 3,
    WheelUp = 4,
    WheelDown = 5,
    Side = 6,
    Extra = 7,
}

/// Encodes a legacy key press or release body.
pub const fn encode_key_code(code: u32) -> [u8; 4] {
    code.to_le_bytes()
}

/// Encodes absolute pointer coordinates in client mouse mode.
pub fn encode_mouse_position(x: u32, y: u32, buttons: MouseButtons, display_id: u8) -> [u8; 11] {
    let mut body = [0; 11];
    body[..4].copy_from_slice(&x.to_le_bytes());
    body[4..8].copy_from_slice(&y.to_le_bytes());
    body[8..10].copy_from_slice(&buttons.bits().to_le_bytes());
    body[10] = display_id;
    body
}

/// Encodes relative pointer motion in server mouse mode.
pub fn encode_mouse_motion(dx: i32, dy: i32, buttons: MouseButtons) -> [u8; 10] {
    let mut body = [0; 10];
    body[..4].copy_from_slice(&dx.to_le_bytes());
    body[4..8].copy_from_slice(&dy.to_le_bytes());
    body[8..].copy_from_slice(&buttons.bits().to_le_bytes());
    body
}

/// Encodes one discrete mouse button edge.
pub fn encode_mouse_button(button: MouseButton, buttons: MouseButtons) -> [u8; 3] {
    let mut body = [0; 3];
    body[0] = button as u8;
    body[1..].copy_from_slice(&buttons.bits().to_le_bytes());
    body
}

fn fixed_size_error(actual: usize, expected: usize, context: &'static str) -> DecodeError {
    DecodeError::new(
        if actual < expected {
            DecodeErrorKind::Truncated
        } else {
            DecodeErrorKind::InvalidValue
        },
        actual,
        context,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_position_has_packed_protocol_layout() {
        let buttons =
            MouseButtons::from_bits(MouseButtons::LEFT.bits() | MouseButtons::RIGHT.bits())
                .expect("known mouse flags");
        let body = encode_mouse_position(0x1122_3344, 0x5566_7788, buttons, 3);

        assert_eq!(&body[..4], &0x1122_3344_u32.to_le_bytes());
        assert_eq!(&body[4..8], &0x5566_7788_u32.to_le_bytes());
        assert_eq!(&body[8..10], &5_u16.to_le_bytes());
        assert_eq!(body[10], 3);
    }

    #[test]
    fn unknown_modifier_bits_are_rejected() {
        let error = KeyboardModifiers::decode(&(1_u16 << 15).to_le_bytes())
            .expect_err("unknown modifier must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }
}
