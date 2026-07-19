use std::collections::{HashMap, HashSet};

use crate::rng::{round_half_even, sample_indexes, seeded, SplitMix64};

/// One judged row reduced to the fields the audit draw reads (judge/verdicts.py
/// `sample_audit` rows): the dedup key, the detector kind, the judge's confidence,
/// and whether the verdict accepted the row.
pub struct AuditRow {
    pub dedup_key: String,
    pub source_kind: String,
    pub confidence: f64,
    pub accepted: bool,
}

// Parity: judge/verdicts.py draw
fn draw(
    rows: &[AuditRow],
    group: &[usize],
    k: usize,
    rng: &mut SplitMix64,
    oversample_share: f64,
) -> Result<(Vec<usize>, Vec<usize>), String> {
    if k >= group.len() {
        return Ok((group.to_vec(), Vec::new()));
    }
    let n_over = round_half_even(k as f64 * oversample_share) as usize;
    let mut by_confidence = group.to_vec();
    by_confidence.sort_by(|&a, &b| {
        rows[a]
            .confidence
            .partial_cmp(&rows[b].confidence)
            .unwrap()
            .then_with(|| rows[a].dedup_key.cmp(&rows[b].dedup_key))
    });
    let over = by_confidence[..n_over].to_vec();
    let over_keys: HashSet<&str> = over.iter().map(|&i| rows[i].dedup_key.as_str()).collect();
    let pool: Vec<usize> = group
        .iter()
        .copied()
        .filter(|&i| !over_keys.contains(rows[i].dedup_key.as_str()))
        .collect();
    if pool.len() < k - n_over {
        return Err(format!(
            "audit draw wants {} core rows but only {} remain after oversampling duplicate-keyed rows",
            k - n_over,
            pool.len()
        ));
    }
    let core = sample_indexes(rng, pool.len(), k - n_over)
        .into_iter()
        .map(|p| pool[p])
        .collect();
    Ok((core, over))
}

// Parity: judge/verdicts.py stratified
fn stratified(
    rows: &[AuditRow],
    indices: &[usize],
    n: usize,
    rng: &mut SplitMix64,
    quotas: &[(String, Option<i64>)],
    remainder_kind: &str,
    oversample_share: f64,
) -> Result<(Vec<usize>, Vec<usize>), String> {
    let mut sorted = indices.to_vec();
    sorted.sort_by(|&a, &b| rows[a].dedup_key.cmp(&rows[b].dedup_key));
    let mut by_kind: HashMap<String, Vec<usize>> = HashMap::new();
    for index in sorted {
        by_kind
            .entry(rows[index].source_kind.clone())
            .or_default()
            .push(index);
    }
    let mut core: Vec<usize> = Vec::new();
    let mut oversample: Vec<usize> = Vec::new();
    let mut spent = 0usize;
    for (kind, quota) in quotas {
        let group = by_kind.get(kind.as_str()).cloned().unwrap_or_default();
        let take = match quota {
            Some(q) if *q < 0 => return Err(format!("negative audit quota for {kind}: {q}")),
            None => group.len(),
            Some(q) => (*q as usize).min(group.len()),
        };
        let (kind_core, kind_over) = draw(rows, &group, take, rng, oversample_share)?;
        core.extend(kind_core);
        oversample.extend(kind_over);
        spent += take;
    }
    let remainder = by_kind.get(remainder_kind).cloned().unwrap_or_default();
    let rest_take = n.saturating_sub(spent).min(remainder.len());
    let (rest_core, rest_over) = draw(rows, &remainder, rest_take, rng, oversample_share)?;
    core.extend(rest_core);
    oversample.extend(rest_over);
    Ok((core, oversample))
}

/// The deterministic stratified audit draw over judged rows (judge/verdicts.py
/// `sample_audit`): one PRNG seeded from the decimal-string `seed`, threaded through
/// the accepted side then the rejected side. Returns `(core, oversample)` as row
/// indexes into `rows`; a negative quota is an error.
pub fn sample_audit(
    rows: &[AuditRow],
    accepts: usize,
    rejects: usize,
    seed: &str,
    quotas: &[(String, Option<i64>)],
    remainder_kind: &str,
    oversample_share: f64,
) -> Result<(Vec<usize>, Vec<usize>), String> {
    let mut rng = seeded(seed);
    let accepted: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].accepted).collect();
    let rejected: Vec<usize> = (0..rows.len()).filter(|&i| !rows[i].accepted).collect();
    let (mut core, mut oversample) = stratified(
        rows,
        &accepted,
        accepts,
        &mut rng,
        quotas,
        remainder_kind,
        oversample_share,
    )?;
    let (reject_core, reject_over) = stratified(
        rows,
        &rejected,
        rejects,
        &mut rng,
        quotas,
        remainder_kind,
        oversample_share,
    )?;
    core.extend(reject_core);
    oversample.extend(reject_over);
    Ok((core, oversample))
}

// Parity: judge/verdicts.py exact_upper_bound — the binomial CDF via the pmf
// recurrence carried in log space (ln pmf(0) = n·ln q; ln pmf(k+1) = ln pmf(k) +
// ln((n-k)/(k+1)) + ln(p/q)) and summed by log-sum-exp, because (1-p)^n underflows
// f64 to zero near n ≈ 350 and would silently zero every recurrence term.
fn binomial_cdf(hits: u64, n: u64, p: f64) -> f64 {
    let ln_q = (-p).ln_1p();
    let ratio = p.ln() - ln_q;
    let mut log_pmf = n as f64 * ln_q;
    let mut log_terms = Vec::with_capacity(hits as usize + 1);
    log_terms.push(log_pmf);
    for k in 0..hits {
        log_pmf += ((n - k) as f64 / (k + 1) as f64).ln() + ratio;
        log_terms.push(log_pmf);
    }
    let max = log_terms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if max == f64::NEG_INFINITY {
        return 0.0;
    }
    let scaled: f64 = log_terms.iter().map(|&t| (t - max).exp()).sum();
    (max + scaled.ln()).exp()
}

/// The exact (Clopper-Pearson) one-sided upper confidence bound: the 60-iteration
/// bisection from `hits/n` to 1.0 for the smallest rate whose binomial CDF at `hits`
/// drops to `alpha` (judge/verdicts.py `exact_upper_bound`).
pub fn exact_upper_bound(hits: u64, n: u64, alpha: f64) -> f64 {
    if hits >= n {
        return 1.0;
    }
    let mut lo = hits as f64 / n as f64;
    let mut hi = 1.0;
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if binomial_cdf(hits, n, mid) > alpha {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<AuditRow> {
        (0..30)
            .map(|i| AuditRow {
                dedup_key: format!("k{i:02}"),
                source_kind: "transcript_message".to_string(),
                confidence: 0.5 + i as f64 / 100.0,
                accepted: i % 2 == 0,
            })
            .collect()
    }

    #[test]
    fn sample_audit_pins_seeded_draw() {
        let rows = fixture();
        let quotas = vec![("interrupt_rejection".to_string(), None)];
        let (core, over) =
            sample_audit(&rows, 5, 5, "7", &quotas, "transcript_message", 0.3).unwrap();
        assert_eq!(core.len(), 6);
        let mut core_keys: Vec<&str> = core.iter().map(|&i| rows[i].dedup_key.as_str()).collect();
        core_keys.sort_unstable();
        assert_eq!(core_keys, ["k06", "k07", "k09", "k18", "k28", "k29"]);
        let over_keys: Vec<&str> = over.iter().map(|&i| rows[i].dedup_key.as_str()).collect();
        assert_eq!(over_keys, ["k00", "k02", "k01", "k03"]);
        assert_eq!(core.iter().filter(|&&i| rows[i].accepted).count(), 3);
        let again = sample_audit(&rows, 5, 5, "7", &quotas, "transcript_message", 0.3).unwrap();
        assert_eq!(again, (core, over));
    }

    #[test]
    fn sample_audit_errors_when_duplicate_keys_exhaust_the_pool() {
        let rows: Vec<AuditRow> = (0..5)
            .map(|i| AuditRow {
                dedup_key: "dup".to_string(),
                source_kind: "transcript_message".to_string(),
                confidence: 0.5 + i as f64 / 100.0,
                accepted: true,
            })
            .collect();
        let quotas: Vec<(String, Option<i64>)> = Vec::new();
        let err = sample_audit(&rows, 3, 0, "7", &quotas, "transcript_message", 0.3).unwrap_err();
        assert!(err.contains("after oversampling"), "{err}");
    }

    #[test]
    fn sample_audit_rejects_a_negative_quota() {
        let rows = fixture();
        let quotas = vec![("transcript_message".to_string(), Some(-1))];
        let err = sample_audit(&rows, 5, 5, "7", &quotas, "transcript_message", 0.3).unwrap_err();
        assert!(err.contains("negative audit quota"), "{err}");
    }

    #[test]
    fn sample_audit_none_quota_exhausts_before_remainder() {
        let mut rows: Vec<AuditRow> = (0..3)
            .map(|i| AuditRow {
                dedup_key: format!("i{i}"),
                source_kind: "interrupt_rejection".to_string(),
                confidence: 0.9,
                accepted: true,
            })
            .collect();
        rows.extend((0..10).map(|i| AuditRow {
            dedup_key: format!("t{i:02}"),
            source_kind: "transcript_message".to_string(),
            confidence: 0.5 + i as f64 / 100.0,
            accepted: true,
        }));
        let quotas = vec![("interrupt_rejection".to_string(), None)];
        let (core, over) =
            sample_audit(&rows, 5, 0, "1", &quotas, "transcript_message", 0.3).unwrap();
        assert_eq!(core.len(), 4);
        let core_keys: HashSet<&str> = core.iter().map(|&i| rows[i].dedup_key.as_str()).collect();
        assert!(["i0", "i1", "i2"].iter().all(|k| core_keys.contains(k)));
        let over_keys: Vec<&str> = over.iter().map(|&i| rows[i].dedup_key.as_str()).collect();
        assert_eq!(over_keys, ["t00"]);
    }

    #[test]
    fn exact_upper_bound_matches_clopper_pearson() {
        let cases = [
            (0u64, 3u64, 0.6315968501359613),
            (0, 60, 0.04870291331009752),
            (1, 10, 0.3941633024365049),
            (2, 20, 0.2826185248858609),
        ];
        for (hits, n, expected) in cases {
            assert!(
                (exact_upper_bound(hits, n, 0.05) - expected).abs() < 1e-9,
                "hits={hits} n={n}"
            );
        }
    }

    #[test]
    fn exact_upper_bound_survives_the_pmf_underflow_regime() {
        // (1-p)^n underflows f64 past n ≈ 350; references from the math.comb original.
        let cases = [
            (900u64, 1000u64, 0.9152152323315308),
            (450, 500, 0.9212614299547971),
        ];
        for (hits, n, expected) in cases {
            assert!(
                (exact_upper_bound(hits, n, 0.05) - expected).abs() < 1e-9,
                "hits={hits} n={n}"
            );
        }
    }

    #[test]
    fn exact_upper_bound_saturates_at_one() {
        assert_eq!(exact_upper_bound(3, 3, 0.05), 1.0);
        assert_eq!(exact_upper_bound(5, 3, 0.05), 1.0);
    }
}
