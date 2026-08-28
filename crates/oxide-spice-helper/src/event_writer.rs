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
        if queue
            .events
            .back()
            .is_some_and(|queued| queued.written.is_none() && is_frame(&queued.event))
        {
            queue.events.back_mut().expect("frame exists").event = event;
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

fn poisoned_queue_error() -> HelperIpcError {
    HelperIpcError::Io(std::io::Error::other("helper event queue lock is poisoned"))
}

fn closed_queue_error() -> HelperIpcError {
    HelperIpcError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "helper event queue is closed",
    ))
}
