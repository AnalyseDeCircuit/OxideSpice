//! Bounded keyboard and pointer delivery for one Inputs channel.

use std::sync::Arc;

use oxide_spice_protocol::{
    INPUT_MOTION_ACK_BUNCH, KeyboardModifiers, MouseButton, MouseButtons, MouseMode,
    encode_key_code, encode_mouse_button, encode_mouse_motion, encode_mouse_position,
    inputs_client, inputs_server,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, Notify, mpsc, watch};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingEnvelope, ProgressRegistry,
    handle_channel_wait,
};

/// Ordered input edges retained independently from coalescible pointer motion.
const INPUT_EDGE_QUEUE_CAPACITY: usize = 128;
/// Maximum bytes accepted in one KEY_SCANCODE message.
const MAX_SCANCODE_SEQUENCE_BYTES: usize = 32;
/// The server ACKs four motions at a time; one extra bunch keeps motion smooth across latency.
const MAX_IN_FLIGHT_POINTER_MESSAGES: u32 = INPUT_MOTION_ACK_BUNCH * 2;

/// Absolute guest pointer state used in SPICE client mouse mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerPosition {
    pub x: u32,
    pub y: u32,
    pub buttons: MouseButtons,
    pub display_id: u8,
}

/// A nonblocking input submission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputSendError {
    #[error("ordered input queue is full")]
    QueueFull,
    #[error("Inputs channel is closed")]
    ChannelClosed,
    #[error("scancode sequence exceeds the per-message limit")]
    ScancodeSequenceTooLong,
    #[error("server did not negotiate raw scancode messages")]
    RawScancodesUnsupported,
    #[error("pointer operation does not match the server-confirmed mouse mode")]
    WrongMouseMode,
    #[error("coalesced relative pointer motion overflowed")]
    RelativeMotionOverflow,
}

/// Cloneable host API whose ordered queue and pointer slot are both explicitly bounded.
#[derive(Clone)]
pub struct InputsHandle {
    edge_sender: mpsc::Sender<InputEdge>,
    pointer_sender: watch::Sender<Option<PointerPosition>>,
    modifiers_receiver: watch::Receiver<KeyboardModifiers>,
    raw_scancodes_supported: bool,
    mouse_mode_receiver: watch::Receiver<MouseMode>,
    relative_motion: Arc<RelativeMotionSlot>,
}

impl std::fmt::Debug for InputsHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputsHandle")
            .field("modifiers", &*self.modifiers_receiver.borrow())
            .field("raw_scancodes_supported", &self.raw_scancodes_supported)
            .field("mouse_mode", &*self.mouse_mode_receiver.borrow())
            .finish_non_exhaustive()
    }
}

impl InputsHandle {
    pub fn mouse_mode(&self) -> MouseMode {
        *self.mouse_mode_receiver.borrow()
    }

    /// Waits for the next server-confirmed pointer mode.
    pub async fn mouse_mode_changed(&mut self) -> Result<MouseMode, InputSendError> {
        self.mouse_mode_receiver
            .changed()
            .await
            .map_err(|_| InputSendError::ChannelClosed)?;
        Ok(*self.mouse_mode_receiver.borrow_and_update())
    }

    pub fn modifiers_state(&self) -> KeyboardModifiers {
        *self.modifiers_receiver.borrow()
    }

    /// Waits for the next guest keyboard-lock state.
    pub async fn modifiers_state_changed(&mut self) -> Result<KeyboardModifiers, InputSendError> {
        self.modifiers_receiver
            .changed()
            .await
            .map_err(|_| InputSendError::ChannelClosed)?;
        Ok(*self.modifiers_receiver.borrow_and_update())
    }

    pub const fn raw_scancodes_supported(&self) -> bool {
        self.raw_scancodes_supported
    }

    /// Waits for bounded queue capacity and sends one legacy key-down code.
    pub async fn key_down(&self, code: u32) -> Result<(), InputSendError> {
        self.send_edge(InputEdge::KeyDown(code)).await
    }

    /// Sends one legacy key-down code without waiting for queue space.
    pub fn try_key_down(&self, code: u32) -> Result<(), InputSendError> {
        self.try_edge(InputEdge::KeyDown(code))
    }

    /// Waits for bounded queue capacity and sends one legacy key-up code.
    pub async fn key_up(&self, code: u32) -> Result<(), InputSendError> {
        self.send_edge(InputEdge::KeyUp(code)).await
    }

    /// Sends one legacy key-up code without waiting for queue space.
    pub fn try_key_up(&self, code: u32) -> Result<(), InputSendError> {
        self.try_edge(InputEdge::KeyUp(code))
    }

    /// Waits for capacity and sends a negotiated raw set-1 scancode sequence.
    pub async fn scancodes(&self, bytes: &[u8]) -> Result<(), InputSendError> {
        self.validate_scancodes(bytes)?;
        self.send_edge(InputEdge::Scancodes(bytes.to_vec().into_boxed_slice()))
            .await
    }

    /// Sends a complete set-1 scancode sequence when the negotiated capability permits it.
    pub fn try_scancodes(&self, bytes: &[u8]) -> Result<(), InputSendError> {
        self.validate_scancodes(bytes)?;
        self.try_edge(InputEdge::Scancodes(bytes.to_vec().into_boxed_slice()))
    }

    /// Waits for capacity and synchronizes keyboard lock state.
    pub async fn modifiers(&self, modifiers: KeyboardModifiers) -> Result<(), InputSendError> {
        self.send_edge(InputEdge::Modifiers(modifiers)).await
    }

    /// Synchronizes keyboard lock state.
    pub fn try_modifiers(&self, modifiers: KeyboardModifiers) -> Result<(), InputSendError> {
        self.try_edge(InputEdge::Modifiers(modifiers))
    }

    /// Waits for capacity and sends one button press with the resulting state.
    pub async fn button_press(
        &self,
        button: MouseButton,
        buttons: MouseButtons,
    ) -> Result<(), InputSendError> {
        self.send_edge(InputEdge::ButtonPress(button, buttons))
            .await
    }

    /// Sends one button press while carrying the resulting complete button state.
    pub fn try_button_press(
        &self,
        button: MouseButton,
        buttons: MouseButtons,
    ) -> Result<(), InputSendError> {
        self.try_edge(InputEdge::ButtonPress(button, buttons))
    }

    /// Waits for capacity and sends one button release with the resulting state.
    pub async fn button_release(
        &self,
        button: MouseButton,
        buttons: MouseButtons,
    ) -> Result<(), InputSendError> {
        self.send_edge(InputEdge::ButtonRelease(button, buttons))
            .await
    }

    /// Sends one button release while carrying the resulting complete button state.
    pub fn try_button_release(
        &self,
        button: MouseButton,
        buttons: MouseButtons,
    ) -> Result<(), InputSendError> {
        self.try_edge(InputEdge::ButtonRelease(button, buttons))
    }

    /// Replaces any unsent pointer position instead of growing a motion queue.
    pub fn set_pointer_position(&self, position: PointerPosition) -> Result<(), InputSendError> {
        if *self.mouse_mode_receiver.borrow() != MouseMode::Client {
            return Err(InputSendError::WrongMouseMode);
        }
        self.pointer_sender
            .send(Some(position))
            .map_err(|_| InputSendError::ChannelClosed)
    }

    /// Accumulates relative movement without allocating one queue item per host event.
    pub async fn move_pointer(
        &self,
        dx: i32,
        dy: i32,
        buttons: MouseButtons,
    ) -> Result<(), InputSendError> {
        if *self.mouse_mode_receiver.borrow() != MouseMode::Server {
            return Err(InputSendError::WrongMouseMode);
        }
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let mut pending = self.relative_motion.pending.lock().await;
        match pending.as_mut() {
            Some(motion) => {
                motion.dx = motion
                    .dx
                    .checked_add(dx)
                    .ok_or(InputSendError::RelativeMotionOverflow)?;
                motion.dy = motion
                    .dy
                    .checked_add(dy)
                    .ok_or(InputSendError::RelativeMotionOverflow)?;
                motion.buttons = buttons;
            }
            None => *pending = Some(RelativeMotion { dx, dy, buttons }),
        }
        drop(pending);
        self.relative_motion.notify.notify_one();
        Ok(())
    }

    /// Returns the latest server-reported lock state.
    pub fn keyboard_modifiers(&self) -> KeyboardModifiers {
        *self.modifiers_receiver.borrow()
    }

    fn try_edge(&self, edge: InputEdge) -> Result<(), InputSendError> {
        self.edge_sender
            .try_send(edge)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InputSendError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => InputSendError::ChannelClosed,
            })
    }

    async fn send_edge(&self, edge: InputEdge) -> Result<(), InputSendError> {
        self.edge_sender
            .send(edge)
            .await
            .map_err(|_| InputSendError::ChannelClosed)
    }

    fn validate_scancodes(&self, bytes: &[u8]) -> Result<(), InputSendError> {
        if !self.raw_scancodes_supported {
            return Err(InputSendError::RawScancodesUnsupported);
        }
        if bytes.is_empty() || bytes.len() > MAX_SCANCODE_SEQUENCE_BYTES {
            return Err(InputSendError::ScancodeSequenceTooLong);
        }
        Ok(())
    }
}

enum InputEdge {
    KeyDown(u32),
    KeyUp(u32),
    Scancodes(Box<[u8]>),
    Modifiers(KeyboardModifiers),
    ButtonPress(MouseButton, MouseButtons),
    ButtonRelease(MouseButton, MouseButtons),
}

impl InputEdge {
    fn requires_pointer_flush(&self) -> bool {
        matches!(self, Self::ButtonPress(..) | Self::ButtonRelease(..))
    }
}

#[derive(Debug, Clone, Copy)]
struct RelativeMotion {
    dx: i32,
    dy: i32,
    buttons: MouseButtons,
}

struct RelativeMotionSlot {
    pending: Mutex<Option<RelativeMotion>>,
    notify: Notify,
}

struct MotionWindow {
    in_flight: u32,
}

impl MotionWindow {
    const fn new() -> Self {
        Self { in_flight: 0 }
    }

    const fn can_send_regular_motion(&self) -> bool {
        self.in_flight < MAX_IN_FLIGHT_POINTER_MESSAGES
    }

    fn record_sent(&mut self) -> Result<(), ClientError> {
        self.in_flight = self
            .in_flight
            .checked_add(1)
            .ok_or_else(|| protocol_value_error("pointer in-flight counter"))?;
        Ok(())
    }

    fn acknowledge_bunch(&mut self) -> Result<(), ClientError> {
        self.in_flight = self
            .in_flight
            .checked_sub(INPUT_MOTION_ACK_BUNCH)
            .ok_or_else(|| protocol_value_error("unexpected mouse motion ACK"))?;
        Ok(())
    }
}

/// Receivers owned exclusively by the Inputs socket task.
pub(crate) struct InputTaskPaths {
    edges: mpsc::Receiver<InputEdge>,
    pointer: watch::Receiver<Option<PointerPosition>>,
    modifiers: watch::Sender<KeyboardModifiers>,
    relative_motion: Arc<RelativeMotionSlot>,
}

/// Creates the host handle and task-owned receivers without exposing channel internals.
pub(crate) fn input_paths(
    raw_scancodes_supported: bool,
    mouse_mode_receiver: watch::Receiver<MouseMode>,
) -> (InputsHandle, InputTaskPaths) {
    let (edge_sender, edge_receiver) = mpsc::channel(INPUT_EDGE_QUEUE_CAPACITY);
    let (pointer_sender, pointer_receiver) = watch::channel(None);
    let (modifiers_sender, modifiers_receiver) = watch::channel(KeyboardModifiers::default());
    let relative_motion = Arc::new(RelativeMotionSlot {
        pending: Mutex::new(None),
        notify: Notify::new(),
    });
    (
        InputsHandle {
            edge_sender,
            pointer_sender,
            modifiers_receiver,
            raw_scancodes_supported,
            mouse_mode_receiver,
            relative_motion: relative_motion.clone(),
        },
        InputTaskPaths {
            edges: edge_receiver,
            pointer: pointer_receiver,
            modifiers: modifiers_sender,
            relative_motion,
        },
    )
}

/// Owns the Inputs transport and arbitrates incoming control, ordered edges, and latest motion.
pub(crate) async fn run_inputs<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    paths: InputTaskPaths,
    mut mouse_mode: watch::Receiver<MouseMode>,
    progress: ProgressRegistry,
    channel_id: u8,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut control = ControlState::new();
    let InputTaskPaths {
        mut edges,
        mut pointer,
        modifiers,
        relative_motion,
    } = paths;
    let mut message_body = Vec::new();
    let mut motion_window = MotionWindow::new();
    let mut received_init = false;
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::Inputs,
        channel_id,
    };
    let mut observed_migration_activation = channel.migration_activation_count();
    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
            }
            incoming = channel.read_message(&mut message_body) => {
                let header = incoming?;
                let envelope = IncomingEnvelope::decode(header, &message_body)?;
                let counts_for_ack = envelope.counts_for_ack();
                let serial = channel.received_serial();
                if let Some(seamless) =
                    channel.observe_migration_activation(&mut observed_migration_activation)
                    && !seamless
                {
                    received_init = false;
                    motion_window = MotionWindow::new();
                    *relative_motion.pending.lock().await = None;
                    modifiers.send_replace(KeyboardModifiers::default());
                }
                for message in envelope.messages() {
                    if control.handle_without_ack(&mut channel, &message).await?
                        == ControlDisposition::Consumed
                    {
                        continue;
                    }
                    if handle_channel_wait(&progress, identity, serial, &mut cancel, &message).await? {
                        continue;
                    }
                    match message.header.message_type {
                        inputs_server::INIT => {
                            if received_init {
                                return Err(protocol_value_error("repeated Inputs Init"));
                            }
                            received_init = true;
                            modifiers.send_replace(KeyboardModifiers::decode(message.body)?);
                        }
                        inputs_server::KEY_MODIFIERS => {
                            if !received_init {
                                return Err(protocol_value_error("Inputs message before Init"));
                            }
                            modifiers.send_replace(KeyboardModifiers::decode(message.body)?);
                        }
                        inputs_server::MOUSE_MOTION_ACK => {
                            if !received_init {
                                return Err(protocol_value_error("Inputs message before Init"));
                            }
                            if !message.body.is_empty() {
                                return Err(protocol_value_error("mouse motion ACK body"));
                            }
                            motion_window.acknowledge_bunch()?;
                        }
                        message_type => return Err(ClientError::UnsupportedMessage {
                            channel: "inputs",
                            message_type,
                        }),
                    }
                }
                if counts_for_ack {
                    control.acknowledge_envelope(&mut channel).await?;
                }
                progress.complete(identity, serial)?;
            }
            edge = edges.recv(), if received_init => {
                let Some(edge) = edge else {
                    let _ = channel.shutdown().await;
                    return Ok(());
                };
                if edge.requires_pointer_flush() {
                    let confirmed_mode = *mouse_mode.borrow();
                    match confirmed_mode {
                        MouseMode::Client if pointer.has_changed().unwrap_or(false) => {
                            send_latest_pointer(
                                &mut channel,
                                &mut pointer,
                                &mut motion_window,
                            )
                            .await?;
                        }
                        MouseMode::Server => {
                            send_relative_motion(
                                &mut channel,
                                &relative_motion,
                                &mut motion_window,
                            )
                            .await?;
                        }
                        MouseMode::Client => {}
                    }
                }
                write_edge(&mut channel, edge).await?;
            }
            changed = mouse_mode.changed() => {
                if changed.is_err() {
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
                let current = *mouse_mode.borrow_and_update();
                match current {
                    MouseMode::Client => {
                        *relative_motion.pending.lock().await = None;
                    }
                    MouseMode::Server => {
                        pointer.borrow_and_update();
                    }
                }
            }
            changed = pointer.changed(), if received_init
                && *mouse_mode.borrow() == MouseMode::Client
                && motion_window.can_send_regular_motion() => {
                if changed.is_err() {
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
                send_latest_pointer(
                    &mut channel,
                    &mut pointer,
                    &mut motion_window,
                )
                .await?;
            }
            _ = relative_motion.notify.notified(), if received_init
                && *mouse_mode.borrow() == MouseMode::Server
                && motion_window.can_send_regular_motion() => {
                send_relative_motion(
                    &mut channel,
                    &relative_motion,
                    &mut motion_window,
                )
                .await?;
            }
        }
    }
}

async fn send_relative_motion<S>(
    channel: &mut Channel<S>,
    relative_motion: &RelativeMotionSlot,
    motion_window: &mut MotionWindow,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let motion = relative_motion.pending.lock().await.take();
    if let Some(motion) = motion {
        channel
            .write_message(
                inputs_client::MOUSE_MOTION,
                &encode_mouse_motion(motion.dx, motion.dy, motion.buttons),
            )
            .await?;
        motion_window.record_sent()?;
    }
    Ok(())
}

async fn send_latest_pointer<S>(
    channel: &mut Channel<S>,
    pointer: &mut watch::Receiver<Option<PointerPosition>>,
    motion_window: &mut MotionWindow,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let position = *pointer.borrow_and_update();
    if let Some(position) = position {
        let body = encode_mouse_position(
            position.x,
            position.y,
            position.buttons,
            position.display_id,
        );
        channel
            .write_message(inputs_client::MOUSE_POSITION, &body)
            .await?;
        motion_window.record_sent()?;
    }
    Ok(())
}

async fn write_edge<S>(channel: &mut Channel<S>, edge: InputEdge) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match edge {
        InputEdge::KeyDown(code) => {
            channel
                .write_message(inputs_client::KEY_DOWN, &encode_key_code(code))
                .await
        }
        InputEdge::KeyUp(code) => {
            channel
                .write_message(inputs_client::KEY_UP, &encode_key_code(code))
                .await
        }
        InputEdge::Scancodes(bytes) => {
            channel
                .write_message(inputs_client::KEY_SCANCODE, &bytes)
                .await
        }
        InputEdge::Modifiers(modifiers) => {
            channel
                .write_message(
                    inputs_client::KEY_MODIFIERS,
                    &modifiers.bits().to_le_bytes(),
                )
                .await
        }
        InputEdge::ButtonPress(button, buttons) => {
            channel
                .write_message(
                    inputs_client::MOUSE_PRESS,
                    &encode_mouse_button(button, buttons),
                )
                .await
        }
        InputEdge::ButtonRelease(button, buttons) => {
            channel
                .write_message(
                    inputs_client::MOUSE_RELEASE,
                    &encode_mouse_button(button, buttons),
                )
                .await
        }
    }
}

fn protocol_value_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_window_stops_at_two_bunches_and_reopens_on_ack() {
        let mut window = MotionWindow::new();
        for _ in 0..MAX_IN_FLIGHT_POINTER_MESSAGES {
            assert!(window.can_send_regular_motion());
            window.record_sent().expect("motion count");
        }
        assert!(!window.can_send_regular_motion());

        window.acknowledge_bunch().expect("valid ACK bunch");
        assert!(window.can_send_regular_motion());
        assert_eq!(window.in_flight, INPUT_MOTION_ACK_BUNCH);
    }

    #[test]
    fn motion_ack_cannot_release_unsent_messages() {
        let error = MotionWindow::new()
            .acknowledge_bunch()
            .expect_err("underflow ACK must fail");
        assert_eq!(error.category(), crate::ErrorCategory::Protocol);
    }

    #[tokio::test]
    async fn confirmed_mode_gates_and_coalesces_the_matching_motion_form() {
        let (mode_sender, mode_receiver) = watch::channel(MouseMode::Server);
        let (handle, paths) = input_paths(false, mode_receiver);
        handle
            .move_pointer(4, -2, MouseButtons::LEFT)
            .await
            .expect("first relative motion");
        handle
            .move_pointer(3, 5, MouseButtons::RIGHT)
            .await
            .expect("coalesced relative motion");
        let pending = paths
            .relative_motion
            .pending
            .lock()
            .await
            .expect("pending relative motion");
        assert_eq!((pending.dx, pending.dy), (7, 3));
        assert_eq!(pending.buttons, MouseButtons::RIGHT);
        assert_eq!(
            handle.set_pointer_position(PointerPosition {
                x: 1,
                y: 2,
                buttons: MouseButtons::default(),
                display_id: 0,
            }),
            Err(InputSendError::WrongMouseMode)
        );

        mode_sender.send_replace(MouseMode::Client);
        assert_eq!(
            handle.move_pointer(1, 1, MouseButtons::default()).await,
            Err(InputSendError::WrongMouseMode)
        );
        handle
            .set_pointer_position(PointerPosition {
                x: 1,
                y: 2,
                buttons: MouseButtons::default(),
                display_id: 0,
            })
            .expect("absolute position in client mode");
    }
}
