//! `bench verify`: compare the correctness records (`replay_check`) of two
//! benchmark runs. Two runs are comparable only when every segment they both
//! replayed committed the same state root and produced the same receipts;
//! any mismatch rejects the comparison.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;
use stacks_bench::db::app::AppDb;
use stacks_bench::db::app::models::ReplayCheck;

/// Outcome of comparing two runs' correctness records.
#[derive(Serialize, schemars::JsonSchema, Debug, Clone)]
pub struct VerifyResult {
    pub baseline_run_id: i32,
    pub candidate_run_id: i32,
    /// Segments present in both runs.
    pub compared: usize,
    /// Segments whose state root and receipts digest agree.
    pub matching: usize,
    pub root_mismatches: usize,
    pub receipt_mismatches: usize,
    /// Segments either run found to disagree with the canonical header root.
    pub canonical_root_mismatches: usize,
    pub only_in_baseline: usize,
    pub only_in_candidate: usize,
    /// Segments of the baseline that were warmup or unmeasured (included in
    /// the comparison like every other segment).
    pub baseline_unmeasured: usize,
    /// True when a run has no records at all (a run made before records
    /// existed): the comparison is then not rejected but is unverified.
    pub unverified: bool,
    /// False on any mismatch or missing segment.
    pub ok: bool,
    /// The first mismatches, for the report.
    pub mismatches: Vec<MismatchJson>,
}
crate::wire::wire_payload!(VerifyResult, "run_verify", 1);

#[derive(Serialize, schemars::JsonSchema, Debug, Clone)]
pub struct MismatchJson {
    pub origin_index_hash: String,
    pub segment_index: i32,
    pub tx_start: i32,
    pub tx_end: i32,
    pub repetition: i32,
    /// `state_root`, `receipts`, `canonical_root`, `only_in_baseline`, `only_in_candidate`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
}

impl VerifyResult {
    /// One line for error messages and logs.
    pub fn describe(&self) -> String {
        if self.unverified {
            return format!(
                "unverified: run {} has {} records and run {} has {}",
                self.baseline_run_id,
                self.compared + self.only_in_baseline,
                self.candidate_run_id,
                self.compared + self.only_in_candidate
            );
        }
        format!(
            "{} segments compared, {} matching, {} state-root mismatches, {} receipt mismatches, \
             {} canonical-root mismatches, {} only in baseline, {} only in candidate",
            self.compared,
            self.matching,
            self.root_mismatches,
            self.receipt_mismatches,
            self.canonical_root_mismatches,
            self.only_in_baseline,
            self.only_in_candidate
        )
    }
}

fn mismatch(c: &ReplayCheck, kind: &str, a: Option<String>, b: Option<String>) -> MismatchJson {
    MismatchJson {
        origin_index_hash: hex::encode(&c.origin_index_hash),
        segment_index: c.segment_index,
        tx_start: c.tx_start,
        tx_end: c.tx_end,
        repetition: c.repetition,
        kind: kind.to_string(),
        baseline: a,
        candidate: b,
    }
}

/// Compare two runs' records; `max_listed` bounds the mismatch list.
pub fn compare_replay_checks(
    baseline_run_id: i32,
    candidate_run_id: i32,
    baseline: &[ReplayCheck],
    candidate: &[ReplayCheck],
    max_listed: usize,
) -> VerifyResult {
    let mut result = VerifyResult {
        baseline_run_id,
        candidate_run_id,
        compared: 0,
        matching: 0,
        root_mismatches: 0,
        receipt_mismatches: 0,
        canonical_root_mismatches: 0,
        only_in_baseline: 0,
        only_in_candidate: 0,
        baseline_unmeasured: baseline.iter().filter(|c| !c.measured).count(),
        unverified: baseline.is_empty() || candidate.is_empty(),
        ok: true,
        mismatches: vec![],
    };
    if result.unverified {
        result.only_in_baseline = baseline.len();
        result.only_in_candidate = candidate.len();
        return result;
    }
    let b: BTreeMap<_, &ReplayCheck> = candidate.iter().map(|c| (c.key(), c)).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |m: MismatchJson, result: &mut VerifyResult| {
        if result.mismatches.len() < max_listed {
            result.mismatches.push(m);
        }
    };
    for a in baseline {
        let key = a.key();
        let Some(c) = b.get(&key) else {
            result.only_in_baseline += 1;
            push(mismatch(a, "only_in_baseline", None, None), &mut result);
            continue;
        };
        seen.insert(key);
        result.compared += 1;
        let mut same = true;
        if a.state_index_root != c.state_index_root {
            same = false;
            result.root_mismatches += 1;
            push(
                mismatch(
                    a,
                    "state_root",
                    Some(hex::encode(&a.state_index_root)),
                    Some(hex::encode(&c.state_index_root)),
                ),
                &mut result,
            );
        }
        if a.receipts_digest != c.receipts_digest {
            same = false;
            result.receipt_mismatches += 1;
            push(
                mismatch(
                    a,
                    "receipts",
                    Some(hex::encode(&a.receipts_digest)),
                    Some(hex::encode(&c.receipts_digest)),
                ),
                &mut result,
            );
        }
        if a.canonical_root_match == Some(false) || c.canonical_root_match == Some(false) {
            same = false;
            result.canonical_root_mismatches += 1;
            push(
                mismatch(
                    a,
                    "canonical_root",
                    a.canonical_root_match.map(|m| m.to_string()),
                    c.canonical_root_match.map(|m| m.to_string()),
                ),
                &mut result,
            );
        }
        if same {
            result.matching += 1;
        }
    }
    for c in candidate {
        if !seen.contains(&c.key()) {
            result.only_in_candidate += 1;
            push(mismatch(c, "only_in_candidate", None, None), &mut result);
        }
    }
    result.ok = result.root_mismatches == 0
        && result.receipt_mismatches == 0
        && result.canonical_root_mismatches == 0
        && result.only_in_baseline == 0
        && result.only_in_candidate == 0;
    result
}

/// Load both runs' records and compare them.
pub async fn verify_runs(
    app_db: &AppDb,
    baseline_run_id: i32,
    candidate_run_id: i32,
    max_listed: usize,
) -> Result<VerifyResult> {
    for id in [baseline_run_id, candidate_run_id] {
        app_db
            .get_benchmark_run(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Benchmark run {id} not found"))?;
    }
    let baseline = app_db.get_replay_checks(baseline_run_id).await?;
    let candidate = app_db.get_replay_checks(candidate_run_id).await?;
    Ok(compare_replay_checks(
        baseline_run_id,
        candidate_run_id,
        &baseline,
        &candidate,
        max_listed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seg: i32, rep: i32, root: u8, digest: u8, canonical: Option<bool>) -> ReplayCheck {
        ReplayCheck {
            benchmark_run_id: 0,
            origin_index_hash: vec![0xab; 32],
            segment_index: seg,
            tx_start: 0,
            tx_end: 10,
            repetition: rep,
            is_warmup: rep == 0,
            measured: rep > 0,
            state_index_root: vec![root; 32],
            receipts_digest: vec![digest; 32],
            canonical_root_match: canonical,
        }
    }

    #[test]
    fn identical_records_are_comparable_and_warmups_count() {
        let a = vec![rec(0, 0, 1, 2, Some(true)), rec(0, 1, 3, 2, None)];
        let r = compare_replay_checks(1, 2, &a, &a, 10);
        assert!(r.ok);
        assert_eq!((r.compared, r.matching), (2, 2));
        assert_eq!(r.baseline_unmeasured, 1);
    }

    #[test]
    fn a_differing_receipt_digest_in_a_warmup_segment_rejects_the_comparison() {
        let a = vec![rec(0, 0, 1, 2, Some(true)), rec(0, 1, 3, 2, None)];
        let mut b = a.clone();
        b[0].receipts_digest = vec![9; 32];
        let r = compare_replay_checks(1, 2, &a, &b, 10);
        assert!(!r.ok);
        assert_eq!(r.receipt_mismatches, 1);
        assert_eq!(r.mismatches[0].kind, "receipts");
        assert_eq!(r.mismatches[0].repetition, 0);
    }

    #[test]
    fn a_differing_root_or_a_missing_segment_rejects_the_comparison() {
        let a = vec![rec(0, 0, 1, 2, Some(true)), rec(1, 0, 4, 5, None)];
        let mut b = a.clone();
        b[0].state_index_root = vec![7; 32];
        b.pop();
        let r = compare_replay_checks(1, 2, &a, &b, 10);
        assert!(!r.ok);
        assert_eq!((r.root_mismatches, r.only_in_baseline, r.compared), (1, 1, 1));
    }

    #[test]
    fn a_canonical_root_failure_on_either_side_rejects() {
        let a = vec![rec(0, 0, 1, 2, Some(true))];
        let b = vec![rec(0, 0, 1, 2, Some(false))];
        let r = compare_replay_checks(1, 2, &a, &b, 10);
        assert!(!r.ok);
        assert_eq!(r.canonical_root_mismatches, 1);
    }

    #[test]
    fn a_run_without_records_is_unverified_not_rejected() {
        let a = vec![rec(0, 0, 1, 2, Some(true))];
        let r = compare_replay_checks(1, 2, &a, &[], 10);
        assert!(r.ok && r.unverified);
    }
}
