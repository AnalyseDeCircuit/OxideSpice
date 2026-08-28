fn main() {
    let target = std::env::var("TARGET").expect("Cargo provides TARGET to build scripts");
    let minimum_system_version = std::env::var("OXIDE_SPICE_MINIMUM_SYSTEM_VERSION")
        .unwrap_or_else(|_| "unspecified".to_owned());
    let dynamic_libraries = std::env::var("OXIDE_SPICE_DYNAMIC_LIBRARIES").unwrap_or_default();
    println!("cargo:rerun-if-env-changed=OXIDE_SPICE_MINIMUM_SYSTEM_VERSION");
    println!("cargo:rerun-if-env-changed=OXIDE_SPICE_DYNAMIC_LIBRARIES");
    println!("cargo:rustc-env=OXIDE_SPICE_BUILD_TARGET={target}");
    println!("cargo:rustc-env=OXIDE_SPICE_MINIMUM_SYSTEM_VERSION={minimum_system_version}");
    println!("cargo:rustc-env=OXIDE_SPICE_DYNAMIC_LIBRARIES={dynamic_libraries}");
}
