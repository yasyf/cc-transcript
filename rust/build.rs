// Local `cargo test` on the default (python) feature links a shared libpython
// whose install name is `@rpath/libpython3.X.dylib`. The cargo test harness
// carries no rpath to the interpreter's lib dir, so the binary aborts at load.
// Emit that rpath, derived from the same interpreter pyo3 linked. Gated off when
// `extension-module` is active, so the shipped wheel (which links no libpython
// and resolves symbols from the host interpreter) keeps its exact build flags.
// Without the `python` feature — the Swift consumer's `--no-default-features`
// build — `pyo3-build-config` is not resolved, so this whole path compiles out
// and no Python toolchain is required.
#[cfg(feature = "python")]
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

#[cfg(not(feature = "python"))]
fn main() {}
