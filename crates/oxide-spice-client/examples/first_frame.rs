//! Connects to the controlled QEMU fixture and writes the first surface state as a PPM image.

use std::env;
use std::error::Error;
use std::path::PathBuf;

use oxide_spice_client::{ConnectOptions, PixelFormat, Session, TicketSecret};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let host = arguments
        .next()
        .ok_or("usage: first_frame HOST PORT OUTPUT.ppm")?
        .into_string()
        .map_err(|_| "HOST must be valid UTF-8")?;
    let port = arguments
        .next()
        .ok_or("usage: first_frame HOST PORT OUTPUT.ppm")?
        .into_string()
        .map_err(|_| "PORT must be valid UTF-8")?
        .parse::<u16>()?;
    let output_path = PathBuf::from(
        arguments
            .next()
            .ok_or("usage: first_frame HOST PORT OUTPUT.ppm")?,
    );
    if arguments.next().is_some() {
        return Err("usage: first_frame HOST PORT OUTPUT.ppm".into());
    }

    // The environment keeps the Ticket out of process arguments and shell history.
    let ticket = env::var("OXIDE_SPICE_TICKET").unwrap_or_default();
    let mut session =
        Session::connect(ConnectOptions::new(host, port, TicketSecret::new(ticket))).await?;
    let frame = session.next_frame().await?;
    let snapshot = frame.surface.snapshot().await?;
    assert_eq!(snapshot.format, PixelFormat::Rgba8);
    let mut ppm = format!("P6\n{} {}\n255\n", snapshot.width, snapshot.height).into_bytes();
    ppm.reserve(snapshot.pixels.len() / 4 * 3);
    for pixel in snapshot.pixels.chunks_exact(4) {
        ppm.extend_from_slice(&pixel[..3]);
    }

    // File I/O is isolated from network-owner tasks and runs after snapshot locking has completed.
    tokio::task::spawn_blocking(move || std::fs::write(output_path, ppm)).await??;
    session.shutdown().await?;
    Ok(())
}
