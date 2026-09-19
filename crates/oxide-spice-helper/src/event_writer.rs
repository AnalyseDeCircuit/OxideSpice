//! Single-owner stdout delivery with coalescible frame backpressure.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use oxide_spice_helper_protocol::{HelperEvent, HelperIpcError, write_event};

const EVENT_QUEUE_CAPACITY: usize = 64;

#[derive(Clone)]
pub(crate) struct EventSender {
    shared: Arc<(Mutex<EventQueue>, Condvar)>,
}

pub(crate) struct EventWriter {
    sender: EventSender,
    thread: Option<JoinHandle<Result<(), HelperIpcError>>>,
}

struct EventQueue {
    events: VecDeque<QueuedEvent>,
    closed: bool,
}

struct QueuedEvent {
    event: HelperEvent,
    written: Option<mpsc::SyncSender<()>>,
}

impl EventWriter {
    pub(crate) fn stdio() -> Result<Self, std::io::Error> {
        let shared = Arc::new((
            Mutex::new(EventQueue {
                events: VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("oxide-spice-event-writer".to_owned())
            .spawn(move || write_stdout_events(thread_shared))?;
        Ok(Self {
            sender: EventSender { shared },
            thread: Some(thread),
        })
    }

    pub(crate) fn sender(&self) -> EventSender {
        self.sender.clone()
    }

    pub(crate) fn finish(mut self) -> Result<(), HelperIpcError> {
        self.sender.close()?;
        match self
            .thread
            .take()
            .expect("event writer thread exists")
            .join()
        {
            Ok(result) => result,
            Err(_) => Err(HelperIpcError::Io(std::io::Error::other(
                "helper event writer panicked",
            ))),
        }
    }
}

impl EventSender {
    pub(crate) fn send_control(&self, event: HelperEvent) -> Result<(), HelperIpcError> {
        let (queue, wake) = &*self.shared;
        let mut queue = queue.lock().map_err(|_| poisoned_queue_error())?;
        if queue.closed {
            return Err(closed_queue_error());
        }
        if queue.events.len() >= EVENT_QUEUE_CAPACITY {
            return Err(HelperIpcError::Io(std::io::Error::other(
                "helper control event queue is full",
            )));
        }
        queue.events.push_back(QueuedEvent {
            event,
            written: None,
        });
        wake.notify_one();
        Ok(())
    }

    /// Writes one control event completely before allowing the caller to read more input.
    pub(crate) fn send_barrier(&self, event: HelperEvent) -> Result<(), HelperIpcError> {
        let (written, receipt) = mpsc::sync_channel(0);
        let (queue, wake) = &*self.shared;
        let mut queue = queue.lock().map_err(|_| poisoned_queue_error())?;
        if queue.closed {
            return Err(closed_queue_error());
        }
        if queue.events.len() >= EVENT_QUEUE_CAPACITY {
            return Err(HelperIpcError::Io(std::io::Error::other(
                "helper control event queue is full",
            )));
        }
        queue.events.push_back(QueuedEvent {
            event,
            written: Some(written),
        });
        wake.notify_one();
        drop(queue);
        receipt.recv().map_err(|_| {
            HelperIpcError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "helper event writer closed before the handshake was flushed",
            ))
        })
    }

    pub(crate) fn has_pending_frame(&self) -> Result<bool, HelperIpcError> {
        let (queue, _) = &*self.shared;
        let queue = queue.lock().map_err(|_| poisoned_queue_error())?;
        Ok(queue.events.iter().any(|queued| is_frame(&queued.event)))
    }

    pub(crate) fn send_frame(&self, event: HelperEvent) -> Result<(), HelperIpcError> {
        debug_assert!(is_frame(&event));
        let (queue, wake) = &*self.shared;
        let mut queue = queue.lock().map_err(|_| poisoned_queue_error())?;
        if queue.closed {
            return Err(closed_queue_error());
        }
        // Only a full snapshot of the same surface may supersede an earlier frame.
        // Control events form ordering barriers, including topology and reset events.
        if let Some(queued) = queue
            .events
            .iter_mut()
            .rev()
            .take_while(|queued| queued.written.is_none() && is_frame(&queued.event))
            .find(|queued| replaces_frame(&queued.event, &event))
        {
            queued.event = event;
        } else {
            if queue.events.len() >= EVENT_QUEUE_CAPACITY {
                return Err(HelperIpcError::Io(std::io::Error::other(
                    "helper event queue is full",
                )));
            }
            queue.events.push_back(QueuedEvent {
                event,
                written: None,
            });
        }
        wake.notify_one();
        Ok(())
    }

    fn close(&self) -> Result<(), HelperIpcError> {
        let (queue, wake) = &*self.shared;
        let mut queue = queue.lock().map_err(|_| poisoned_queue_error())?;
        queue.closed = true;
        wake.notify_all();
        Ok(())
    }
}

fn write_stdout_events(shared: Arc<(Mutex<EventQueue>, Condvar)>) -> Result<(), HelperIpcError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    loop {
        let queued = {
            let (queue, wake) = &*shared;
            let mut queue = queue.lock().map_err(|_| poisoned_queue_error())?;
            while queue.events.is_empty() && !queue.closed {
                queue = wake.wait(queue).map_err(|_| poisoned_queue_error())?;
            }
            match queue.events.pop_front() {
                Some(event) => event,
                None if queue.closed => return Ok(()),
                None => continue,
            }
        };
        write_event(&mut stdout, &queued.event)?;
        use std::io::Write;
        stdout.flush()?;
        if let Some(written) = queued.written {
            let _ = written.send(());
        }
    }
}

fn is_frame(event: &HelperEvent) -> bool {
    matches!(event, HelperEvent::Frame { .. })
}

fn replaces_frame(previous: &HelperEvent, incoming: &HelperEvent) -> bool {
    match (previous, incoming) {
        (
            HelperEvent::Frame {
                connection_generation: old_generation,
                graphics_epoch: old_epoch,
                display_channel_id: old_channel,
                surface_id: old_surface,
                ..
            },
            HelperEvent::Frame {
                connection_generation,
                graphics_epoch,
                display_channel_id,
                surface_id,
                full_refresh: true,
                ..
            },
        ) => {
            (old_generation, old_epoch, old_channel, old_surface)
                == (
                    connection_generation,
                    graphics_epoch,
                    display_channel_id,
                    surface_id,
                )
        }
        _ => false,
    }
}

fn poisoned_queue_error() -> HelperIpcError {
    HelperIpcError::Io(std::io::Error::other("helper event queue lock is poisoned"))
}

fn closed_queue_error() -> HelperIpcError {
    HelperIpcError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "helper event queue is closed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxide_spice_helper_protocol::{HelperPixelFormat, HelperRect};

    #[test]
    fn coalesces_only_the_same_surface_before_a_control_barrier() {
        let shared = Arc::new((
            Mutex::new(EventQueue {
                events: VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let sender = EventSender {
            shared: shared.clone(),
        };
        let frame = |channel, pixel| HelperEvent::Frame {
            connection_generation: 1,
            graphics_epoch: 1,
            display_channel_id: channel,
            surface_id: 0,
            surface_width: 1,
            surface_height: 1,
            rect: HelperRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            full_refresh: true,
            format: HelperPixelFormat::Rgba8,
            pixels: vec![pixel, 0, 0, 255],
        };
        sender.send_frame(frame(0, 10)).unwrap();
        sender.send_frame(frame(1, 20)).unwrap();
        sender.send_frame(frame(0, 30)).unwrap();
        sender
            .send_control(HelperEvent::KeyboardModifiers { bits: 0 })
            .unwrap();
        sender.send_frame(frame(0, 40)).unwrap();
        let actual = shared
            .0
            .lock()
            .unwrap()
            .events
            .iter()
            .map(|queued| match &queued.event {
                HelperEvent::Frame {
                    display_channel_id,
                    pixels,
                    ..
                } => Some((*display_channel_id, pixels[0])),
                HelperEvent::KeyboardModifiers { .. } => None,
                _ => panic!("unexpected event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![Some((0, 30)), Some((1, 20)), None, Some((0, 40))]
        );
    }
}
