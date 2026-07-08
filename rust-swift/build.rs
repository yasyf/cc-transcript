fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    swift_bridge_build::parse_bridges(vec!["src/lib.rs"])
        .write_all_concatenated("./generated", "cc_transcript_swift");
}
