//! Native usbredirhost and libusb backend for one raw SPICE USB redirection stream.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use oxide_spice_client::{UsbRedirChannel, UsbRedirSendError};
use tokio::sync::mpsc;
use usbredirhost::rusb::{Context, UsbContext};
use usbredirhost::{Device, DeviceHandler, LogLevel};

const NATIVE_QUEUE_CAPACITY: usize = 16;
const LIBUSB_EVENT_INTERVAL: Duration = Duration::from_millis(5);

/// Stable physical identity for selecting one libusb device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsbDeviceIdentity {
    pub bus_number: u8,
    pub device_address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Failures from device discovery, native usbredir processing, or SPICE transport ownership.
#[derive(Debug, thiserror::Error)]
pub enum UsbRedirectionError {
    #[error("libusb failed: {0}")]
    LibUsb(#[from] usbredirhost::rusb::Error),
    #[error("selected USB device is no longer present")]
    DeviceNotFound,
    #[error("usbredirhost failed: {0}")]
    UsbRedirHost(#[from] usbredirhost::Error),
    #[error("SPICE usbredir transport failed: {0}")]
    Transport(#[from] UsbRedirSendError),
    #[error("native USB worker terminated unexpectedly")]
    WorkerTerminated,
    #[error("native USB worker panicked")]
    WorkerPanicked,
}

/// Lists currently visible USB devices without opening or claiming them.
pub fn list_usb_devices() -> Result<Vec<UsbDeviceIdentity>, UsbRedirectionError> {
    let context = Context::new()?;
    let devices = context.devices()?;
    let mut identities = Vec::with_capacity(devices.len());
    for device in devices.iter() {
        let descriptor = device.device_descriptor()?;
        identities.push(UsbDeviceIdentity {
            bus_number: device.bus_number(),
            device_address: device.address(),
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
        });
    }
    Ok(identities)
}

/// Runs one physical USB device until the SPICE channel closes or native processing fails.
pub async fn run_usb_redirection(
    mut channel: UsbRedirChannel,
    identity: UsbDeviceIdentity,
) -> Result<(), UsbRedirectionError> {
    let (native_output, mut outgoing_bytes) = mpsc::channel(NATIVE_QUEUE_CAPACITY);
    let mut transport_generation = 0_u64;
    let (mut worker_commands, mut worker) =
        spawn_native_worker(identity, transport_generation, native_output.clone());

    let result = loop {
        tokio::select! {
            worker_result = &mut worker => {
                break match worker_result {
                    Ok(result) => result,
                    Err(_) => Err(UsbRedirectionError::WorkerPanicked),
                };
            }
            incoming = channel.next() => {
                match incoming {
                    Ok(inbound) => {
                        if inbound.transport_generation != transport_generation {
                            stop_native_worker(&worker_commands, &mut worker).await?;
                            transport_generation = inbound.transport_generation;
                            (worker_commands, worker) = spawn_native_worker(
                                identity,
                                transport_generation,
                                native_output.clone(),
                            );
                        }
                        if worker_commands
                            .send(WorkerCommand::PeerBytes(inbound.bytes))
                            .is_err()
                        {
                            break Err(UsbRedirectionError::WorkerTerminated);
                        }
                    }
                    Err(UsbRedirSendError::Closed) => break Ok(()),
                    Err(error) => break Err(error.into()),
                }
            }
            outgoing = outgoing_bytes.recv() => {
                let Some((output_generation, bytes)) = outgoing else {
                    break Err(UsbRedirectionError::WorkerTerminated);
                };
                if output_generation != transport_generation {
                    continue;
                }
                if let Err(error) = channel.write(&bytes).await {
                    break Err(error.into());
                }
            }
        }
    };

    let _ = worker_commands.send(WorkerCommand::Stop);
    if !worker.is_finished() {
        match worker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if result.is_ok() => return Err(error),
            Err(_) if result.is_ok() => return Err(UsbRedirectionError::WorkerPanicked),
            _ => {}
        }
    }
    result
}

fn spawn_native_worker(
    identity: UsbDeviceIdentity,
    transport_generation: u64,
    native_output: mpsc::Sender<(u64, Vec<u8>)>,
) -> (
    std_mpsc::Sender<WorkerCommand>,
    tokio::task::JoinHandle<Result<(), UsbRedirectionError>>,
) {
    let (worker_commands, commands) = std_mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        run_native_worker(identity, transport_generation, commands, native_output)
    });
    (worker_commands, worker)
}

async fn stop_native_worker(
    worker_commands: &std_mpsc::Sender<WorkerCommand>,
    worker: &mut tokio::task::JoinHandle<Result<(), UsbRedirectionError>>,
) -> Result<(), UsbRedirectionError> {
    let _ = worker_commands.send(WorkerCommand::Stop);
    match worker.await {
        Ok(result) => result,
        Err(_) => Err(UsbRedirectionError::WorkerPanicked),
    }
}

enum WorkerCommand {
    PeerBytes(Arc<[u8]>),
    Stop,
}

#[derive(Debug)]
struct NativeHandler {
    peer_bytes: Mutex<VecDeque<u8>>,
    transport_generation: u64,
    outgoing: mpsc::Sender<(u64, Vec<u8>)>,
}

impl NativeHandler {
    fn new(transport_generation: u64, outgoing: mpsc::Sender<(u64, Vec<u8>)>) -> Self {
        Self {
            peer_bytes: Mutex::new(VecDeque::new()),
            transport_generation,
            outgoing,
        }
    }

    fn append_peer_bytes(&self, bytes: &[u8]) {
        self.peer_bytes
            .lock()
            .expect("usbredir peer byte lock")
            .extend(bytes);
    }
}

impl DeviceHandler for NativeHandler {
    fn log(&mut self, _level: LogLevel, _message: &str) {}

    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut peer_bytes = self.peer_bytes.lock().expect("usbredir peer byte lock");
        if peer_bytes.is_empty() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let read_bytes = buffer.len().min(peer_bytes.len());
        for destination in &mut buffer[..read_bytes] {
            *destination = peer_bytes
                .pop_front()
                .expect("usbredir peer byte count checked");
        }
        Ok(read_bytes)
    }

    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.outgoing
            .blocking_send((self.transport_generation, buffer.to_vec()))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buffer.len())
    }

    fn flush_writes(&mut self) {}
}

fn run_native_worker(
    identity: UsbDeviceIdentity,
    transport_generation: u64,
    commands: std_mpsc::Receiver<WorkerCommand>,
    native_output: mpsc::Sender<(u64, Vec<u8>)>,
) -> Result<(), UsbRedirectionError> {
    let context = Context::new()?;
    let devices = context.devices()?;
    let device = devices
        .iter()
        .find(|device| {
            device.bus_number() == identity.bus_number
                && device.address() == identity.device_address
                && device.device_descriptor().is_ok_and(|descriptor| {
                    descriptor.vendor_id() == identity.vendor_id
                        && descriptor.product_id() == identity.product_id
                })
        })
        .ok_or(UsbRedirectionError::DeviceNotFound)?;
    let handle = device.open()?;
    let handler = NativeHandler::new(transport_generation, native_output);
    let native_device = Device::new(&context, Some(handle), handler, 0)?;

    loop {
        match commands.recv_timeout(LIBUSB_EVENT_INTERVAL) {
            Ok(WorkerCommand::PeerBytes(bytes)) => {
                native_device.handler().append_peer_bytes(&bytes);
                native_device.read_peer()?;
            }
            Ok(WorkerCommand::Stop) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(());
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
        context.handle_events(Some(Duration::ZERO))?;
        while native_device.has_data_to_write() != 0 {
            native_device.write_peer()?;
        }
    }
}
