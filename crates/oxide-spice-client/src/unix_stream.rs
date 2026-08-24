//! Unix stream adapter that preserves SCM_RIGHTS ownership during ordinary byte reads.

use std::collections::VecDeque;
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, recvmsg};
use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};
use tokio::net::UnixStream;

const MAX_FILE_DESCRIPTORS_PER_READ: usize = 4;
const MAX_QUEUED_FILE_DESCRIPTORS: usize = 8;

pub(crate) type ReceivedFileDescriptors = Arc<Mutex<VecDeque<OwnedFd>>>;

/// Owns one nonblocking Unix stream and captures ancillary descriptors before bytes are consumed.
pub(crate) struct UnixFdStream {
    stream: UnixStream,
    received_file_descriptors: ReceivedFileDescriptors,
}

impl UnixFdStream {
    pub(crate) fn new(stream: UnixStream) -> (Self, ReceivedFileDescriptors) {
        let received_file_descriptors = Arc::new(Mutex::new(VecDeque::new()));
        (
            Self {
                stream,
                received_file_descriptors: received_file_descriptors.clone(),
            },
            received_file_descriptors,
        )
    }
}

impl AsyncRead for UnixFdStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            ready!(this.stream.poll_read_ready(context))?;
            let unfilled = buffer.initialize_unfilled();
            if unfilled.is_empty() {
                return Poll::Ready(Ok(()));
            }
            let received = this.stream.try_io(Interest::READABLE, || {
                let mut io_slices = [IoSliceMut::new(unfilled)];
                let mut ancillary_space = [MaybeUninit::uninit();
                    rustix::cmsg_space!(ScmRights(MAX_FILE_DESCRIPTORS_PER_READ))];
                let mut ancillary = RecvAncillaryBuffer::new(&mut ancillary_space);
                let message = recvmsg(
                    &this.stream,
                    &mut io_slices,
                    &mut ancillary,
                    RecvFlags::DONTWAIT,
                )
                .map_err(std::io::Error::from)?;
                if message.flags.contains(ReturnFlags::CTRUNC) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "truncated Unix ancillary data",
                    ));
                }
                let mut file_descriptors = Vec::new();
                for control_message in ancillary.drain() {
                    if let RecvAncillaryMessage::ScmRights(received) = control_message {
                        for file_descriptor in received {
                            fcntl_setfd(&file_descriptor, FdFlags::CLOEXEC)
                                .map_err(std::io::Error::from)?;
                            file_descriptors.push(file_descriptor);
                        }
                    }
                }
                Ok((message.bytes, file_descriptors))
            });
            match received {
                Ok((bytes, file_descriptors)) => {
                    let mut queued = this.received_file_descriptors.lock().map_err(|_| {
                        std::io::Error::other("Unix file descriptor queue poisoned")
                    })?;
                    if queued.len().saturating_add(file_descriptors.len())
                        > MAX_QUEUED_FILE_DESCRIPTORS
                    {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "too many queued Unix file descriptors",
                        )));
                    }
                    queued.extend(file_descriptors);
                    drop(queued);
                    buffer.advance(bytes);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

impl AsyncWrite for UnixFdStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(context)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(context, buffers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IoSlice;
    use std::os::fd::AsFd;

    use rustix::fs::{Mode, OFlags, open};
    use rustix::io::fcntl_getfd;
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn byte_read_preserves_owned_close_on_exec_descriptor() {
        let (receiving_stream, sending_stream) = UnixStream::pair().expect("Unix stream pair");
        let (mut receiving_stream, received_file_descriptors) = UnixFdStream::new(receiving_stream);
        let file_descriptor =
            open("/dev/null", OFlags::RDONLY, Mode::empty()).expect("open test descriptor");
        let borrowed = [file_descriptor.as_fd()];
        let mut ancillary_space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = SendAncillaryBuffer::new(&mut ancillary_space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(&borrowed)));
        let payload = [0x5a];
        sendmsg(
            &sending_stream,
            &[IoSlice::new(&payload)],
            &mut ancillary,
            SendFlags::empty(),
        )
        .expect("send descriptor");

        let mut received_byte = [0];
        receiving_stream
            .read_exact(&mut received_byte)
            .await
            .expect("receive payload byte");
        assert_eq!(received_byte, payload);
        let received = received_file_descriptors
            .lock()
            .expect("descriptor queue")
            .pop_front()
            .expect("received descriptor");
        assert!(
            fcntl_getfd(&received)
                .expect("descriptor flags")
                .contains(FdFlags::CLOEXEC)
        );
    }
}
