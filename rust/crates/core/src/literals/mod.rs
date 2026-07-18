//! Hand-owned shared literal tables — the single source of truth for constants
//! mirrored to Python via `_native.embedded_literals()`. Edit here; the Python spec
//! builders read these values, and `tests/test_literals_parity.py` guards against a
//! Python-side redeclaration drifting from them.

pub mod command;
pub mod corrections;
pub mod feedback;
pub mod mining;
pub mod protocol;
