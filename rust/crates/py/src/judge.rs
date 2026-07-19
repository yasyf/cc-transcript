use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use cc_transcript_core::judge::{exact_upper_bound, sample_audit, AuditRow};

/// The seeded stratified audit draw over pre-coerced judged rows
/// (judge/verdicts.py sample_audit): returns `(core, oversample)` as row indexes
/// into `rows`.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (rows, accepts, rejects, seed, quotas, remainder_kind, oversample_share))]
pub(crate) fn judge_sample_audit(
    rows: Vec<(String, String, f64, bool)>,
    accepts: usize,
    rejects: usize,
    seed: String,
    quotas: Vec<(String, Option<i64>)>,
    remainder_kind: String,
    oversample_share: f64,
) -> PyResult<(Vec<usize>, Vec<usize>)> {
    let rows: Vec<AuditRow> = rows
        .into_iter()
        .map(|(dedup_key, source_kind, confidence, accepted)| AuditRow {
            dedup_key,
            source_kind,
            confidence,
            accepted,
        })
        .collect();
    sample_audit(
        &rows,
        accepts,
        rejects,
        &seed,
        &quotas,
        &remainder_kind,
        oversample_share,
    )
    .map_err(PyValueError::new_err)
}

/// The exact (Clopper-Pearson) one-sided upper confidence bound
/// (judge/verdicts.py exact_upper_bound).
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn judge_exact_upper_bound(hits: u64, n: u64, alpha: f64) -> f64 {
    exact_upper_bound(hits, n, alpha)
}
