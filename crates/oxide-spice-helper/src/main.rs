//! Standalone OxideSpice helper executable.

fn main() {
    if !std::env::args()
        .skip(1)
        .any(|argument| argument == "--stdio")
    {
        eprintln!("oxide-spice-helper: pass --stdio to enable the helper protocol");
        std::process::exit(2);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("oxide-spice-helper: failed to create runtime: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(oxide_spice_helper::run_stdio()) {
        eprintln!("oxide-spice-helper: {error}");
        std::process::exit(1);
    }
}
