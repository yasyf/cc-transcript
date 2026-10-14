use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const SETTINGS: [&str; 5] = [
    "TARGET",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "CARGO_ENCODED_RUSTFLAGS",
];

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("core source directory") {
        let path = entry.expect("core source entry").path();
        if path.is_dir() {
            sources(&path, found);
        } else {
            found.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .expect("a Cargo.lock above the core crate");
    let watched = [
        manifest.join("src"),
        manifest.join("Cargo.toml"),
        manifest.join("build.rs"),
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
    ];
    for path in &watched {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let mut files = watched[1..].to_vec();
    sources(&watched[0], &mut files);
    files.sort();
    let mut identity = Sha256::new();
    for file in &files {
        let name = file
            .strip_prefix(workspace)
            .expect("a build input under the workspace");
        let contents = fs::read(file).expect("a readable build input");
        identity.update(name.as_os_str().as_encoded_bytes());
        identity.update([0u8]);
        identity.update((contents.len() as u64).to_le_bytes());
        identity.update(&contents);
    }
    let compiler = Command::new(env::var_os("RUSTC").expect("RUSTC"))
        .arg("-vV")
        .output()
        .expect("rustc -vV");
    assert!(compiler.status.success(), "rustc -vV failed");
    identity.update((compiler.stdout.len() as u64).to_le_bytes());
    identity.update(&compiler.stdout);
    let mut settings: Vec<(String, Vec<u8>)> = env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .filter(|(name, _)| {
            name.starts_with("CARGO_FEATURE_")
                || name.starts_with("CARGO_CFG_")
                || SETTINGS.contains(&name.as_str())
        })
        .map(|(name, value)| (name, value.as_encoded_bytes().to_vec()))
        .collect();
    settings.sort();
    for (name, value) in &settings {
        identity.update(name.as_bytes());
        identity.update([0u8]);
        identity.update((value.len() as u64).to_le_bytes());
        identity.update(value);
    }
    println!(
        "cargo:rustc-env=CC_TRANSCRIPT_CORE_BUILD={:x}",
        identity.finalize()
    );
}
