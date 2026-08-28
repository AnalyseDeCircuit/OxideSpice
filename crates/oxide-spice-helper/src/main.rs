//! Standalone OxideSpice helper executable.

fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [argument] if argument == "--stdio" => run_stdio(),
        [argument] if argument == "--print-build-metadata" => print_build_metadata(),
        [argument] if argument == "--check-native-loads" => check_native_loads(),
        _ => {
            eprintln!(
                "oxide-spice-helper: pass --stdio, --print-build-metadata, or --check-native-loads"
            );
            std::process::exit(2);
        }
    }
}

fn check_native_loads() {
    #[cfg(feature = "smartcard")]
    if let Err(error) = oxide_spice_helper::check_pcsc_client_library() {
        eprintln!("oxide-spice-helper: PC/SC client load check failed: {error}");
        std::process::exit(1);
    }
    #[cfg(not(feature = "smartcard"))]
    {
        eprintln!("oxide-spice-helper: Smartcard support is not compiled");
        std::process::exit(1);
    }
}

fn run_stdio() {
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

fn print_build_metadata() {
    let metadata = oxide_spice_helper::build_metadata();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if let Err(error) = oxide_spice_helper_protocol::write_metadata(&mut stdout, &metadata) {
        eprintln!("oxide-spice-helper: {error}");
        std::process::exit(1);
    }
}
