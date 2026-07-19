pub fn stub_info() -> pyo3_stub_gen::Result<pyo3_stub_gen::StubInfo> {
    let manifest_dir: &std::path::Path = env!("CARGO_MANIFEST_DIR").as_ref();
    pyo3_stub_gen::StubInfo::from_pyproject_toml(manifest_dir.join("../../../pyproject.toml"))
}

mod actor_bridge;
mod codex;
mod command;
mod context;
mod corrections;
mod discovery;
mod feedback;
mod judge;
mod lexicon;
mod mining;
mod nlp;
mod python;
mod render;
mod score;
mod sqlite;
mod toolcall;
mod views;
mod watch;
