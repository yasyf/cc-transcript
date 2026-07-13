// Emit an rpath to pyo3's shared libpython so cargo-test binaries load; skipped
// under `extension-module` to keep the shipped wheel's exact build flags.
fn main() {
    if std::env::var_os("CARGO_FEATURE_EXTENSION_MODULE").is_some() {
        return;
    }
    println!("cargo:rerun-if-env-changed=PYO3_PYTHON");
    println!("cargo:rerun-if-env-changed=PYO3_CONFIG_FILE");
    let lib_dir = pyo3_build_config::get()
        .lib_dir
        .as_ref()
        .expect("pyo3 linked a shared libpython but reported no lib_dir for the test rpath");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
}
