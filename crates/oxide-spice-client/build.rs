#[cfg(feature = "composite-pixman")]
fn main() {
    // pixman-sys names the library but does not propagate non-system search paths.
    pkg_config::Config::new()
        .atleast_version("0.30.0")
        .probe("pixman-1")
        .expect("pixman-1 is required for SPICE Composite rendering");
}

#[cfg(not(feature = "composite-pixman"))]
fn main() {}
