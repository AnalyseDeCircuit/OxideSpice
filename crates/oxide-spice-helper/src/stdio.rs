//! Process boundary that owns stdin decoding and stdout event serialization.

use std::io::{self, BufReader};

use tokio::sync::mpsc;

use crate::build_info::helper_build_info;
use crate::event_writer::EventWriter;
use crate::runtime::{HelperRuntimeError, run_helper};
use oxide_spice_helper_protocol::{
    HelperErrorCategory, HelperEvent, HelperIpcError, HelperRequest, ServerHandshake,
    read_initial_hello, read_request,
};

const REQUEST_QUEUE_CAPACITY: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum HelperProcessError {
    #[error("helper runtime failed: {0}")]
    Runtime(#[from] HelperRuntimeError),
    #[error("helper IPC failed: {0}")]
    Ipc(#[from] HelperIpcError),
    #[error("helper process I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn run_stdio() -> Result<(), HelperProcessError> {
    let event_writer = EventWriter::stdio()?;
    let events = event_writer.sender();
    let (request_sender, request_receiver) = mpsc::channel(REQUEST_QUEUE_CAPACITY);
    let reader_events = events.clone();
    let _reader = std::thread::Builder::new()
        .name("oxide-spice-request-reader".to_owned())
        .spawn(move || read_stdin_requests(request_sender, reader_events))?;
    let runtime_result = run_helper(request_receiver, events).await;
    event_writer.finish()?;
    runtime_result?;
    Ok(())
}

fn read_stdin_requests(
    sender: mpsc::Sender<HelperRequest>,
    events: crate::event_writer::EventSender,
) {
    let stdin = io::stdin();
    // A one-byte pre-handshake buffer prevents read-ahead into a pipelined Connect body.
    let mut hello_reader = BufReader::with_capacity(1, stdin.lock());
    let hello = match read_initial_hello(&mut hello_reader) {
        Ok(Some(hello)) => hello,
        Ok(None) => return,
        Err(error) => {
            let _ = events.send_control(HelperEvent::Error {
                category: HelperErrorCategory::Protocol,
                message: error.to_string(),
            });
            return;
        }
    };
    let acknowledgement = ServerHandshake::new(helper_build_info()).negotiate(hello);
    let compatible = acknowledgement.compatible;
    if events
        .send_barrier(HelperEvent::HelloAck { acknowledgement })
        .is_err()
    {
        return;
    }
    if !compatible {
        return;
    }
    let mut reader = BufReader::new(hello_reader.into_inner());
    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(error) => {
                let _ = events.send_control(HelperEvent::Error {
                    category: HelperErrorCategory::Protocol,
                    message: error.to_string(),
                });
                return;
            }
        };
        let close = matches!(request, HelperRequest::Close);
        if sender.blocking_send(request).is_err() {
            return;
        }
        if close {
            return;
        }
    }
}
