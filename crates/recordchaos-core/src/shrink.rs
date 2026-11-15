//! Shrinking: delta debugging (ddmin) over the list of mutations, then
//! removal of entries the remaining mutations do not need. A step counts only
//! when the failure's fingerprint matches exactly.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::bundle::{entries, resource, resource_key};
use crate::refs::{RefGraph, Resolution};

/// Replays allowed per failure (`--shrink-budget`).
pub const DEFAULT_BUDGET: usize = 64;

/// Counts replays against a budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub limit: usize,
    pub used: usize,
}

impl Budget {
    pub fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// Take one replay. False when the budget is spent.
    pub fn take(&mut self) -> bool {
        if self.used >= self.limit {
            return false;
        }
        self.used += 1;
        true
    }

    pub fn left(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }
}

/// Zeller's ddmin. `fails(subset)` must return true when the subset still
/// fails with the same fingerprint. Order is kept. The empty set is never
/// tested, since a case with no mutation is the base bundle.
pub fn ddmin<T: Clone>(items: &[T], fails: &mut dyn FnMut(&[T]) -> bool) -> Vec<T> {
    let mut current: Vec<T> = items.to_vec();
    let mut n = 2usize;
    while current.len() >= 2 {
        let len = current.len();
        let chunk = len.div_ceil(n);
        let subsets: Vec<Vec<T>> = current.chunks(chunk).map(<[T]>::to_vec).collect();
        let mut reduced = false;
        for s in &subsets {
            if fails(s) {
                current = s.clone();
                n = 2;
                reduced = true;
                break;
            }
        }
        if !reduced && subsets.len() > 2 {
            for i in 0..subsets.len() {
                let complement: Vec<T> = subsets
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .flat_map(|(_, s)| s.iter().cloned())
                    .collect();
                if fails(&complement) {
                    current = complement;
                    n = (n - 1).max(2);
                    reduced = true;
                    break;
                }
            }
        }
        if !reduced {
            if n >= len {
                break;
            }
            n = (n * 2).min(len);
        }
    }
    current
}

/// Entries that may be removed, in reference-graph order: leaves (entries
/// nothing refers to) first, last entry first. Never the Patient, never a
/// resource a remaining mutation touched, and never an entry that another
/// entry still refers to, in the case bundle or in the base.
pub fn removable_entries(
    case_bundle: &Value,
    base: &Value,
    protected: &BTreeSet<String>,
) -> Vec<String> {
    let g = RefGraph::build(case_bundle);
    let gb = RefGraph::build(base);
    let mut out = Vec::new();
    for (i, e) in entries(case_bundle).iter().enumerate().rev() {
        let Some(res) = resource(e) else { continue };
        let Some(key) = resource_key(res) else {
            continue;
        };
        if protected.contains(&key)
            || res.get("resourceType").and_then(Value::as_str) == Some("Patient")
        {
            continue;
        }
        if g.referrers(i).iter().any(|s| s.entry != i) {
            continue;
        }
        if let Some(bi) = gb.entry_of_key(&key) {
            if gb.referrers(bi).iter().any(|s| s.entry != bi) {
                continue;
            }
        }
        out.push(key);
    }
    out
}

/// Number of references that do not resolve, used to make sure a removal
/// does not add a dangling reference.
pub fn dangling_count(bundle: &Value) -> usize {
    let g = RefGraph::build(bundle);
    g.resolved
        .iter()
        .filter(|r| **r == Resolution::Unresolved)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ddmin_finds_a_pair() {
        let items: Vec<u32> = (0..10).collect();
        let mut calls = 0;
        let got = ddmin(&items, &mut |s: &[u32]| {
            calls += 1;
            s.contains(&3) && s.contains(&7)
        });
        assert_eq!(got, vec![3, 7]);
        assert!(calls < 40, "{calls} calls");
    }

    #[test]
    fn ddmin_finds_a_single_cause() {
        let items = vec!["reorder", "urn", "dangling"];
        let got = ddmin(&items, &mut |s: &[&str]| s.contains(&"urn"));
        assert_eq!(got, vec!["urn"]);
    }

    #[test]
    fn ddmin_keeps_everything_when_all_are_needed() {
        let items = vec![1, 2, 3];
        let got = ddmin(&items, &mut |s: &[i32]| s.len() == 3);
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[test]
    fn ddmin_respects_a_budget() {
        let mut budget = Budget::new(2);
        let items: Vec<u32> = (0..16).collect();
        let got = ddmin(&items, &mut |s: &[u32]| budget.take() && s.contains(&5));
        assert!(got.contains(&5));
        assert_eq!(budget.left(), 0);
    }

    #[test]
    fn removable_skips_patients_and_referenced_entries() {
        let b = json!({"resourceType": "Bundle", "entry": [
            {"fullUrl": "https://example.org/fhir/Patient/p1", "resource": {"resourceType": "Patient", "id": "p1"}},
            {"fullUrl": "https://example.org/fhir/Encounter/e1", "resource": {"resourceType": "Encounter", "id": "e1", "subject": {"reference": "Patient/p1"}}},
            {"fullUrl": "https://example.org/fhir/Observation/o1", "resource": {"resourceType": "Observation", "id": "o1", "encounter": {"reference": "Encounter/e1"}}},
            {"fullUrl": "https://example.org/fhir/Observation/o2", "resource": {"resourceType": "Observation", "id": "o2"}}
        ]});
        let protected: BTreeSet<String> = ["Observation/o2".to_string()].into_iter().collect();
        assert_eq!(
            removable_entries(&b, &b, &protected),
            vec!["Observation/o1".to_string()]
        );
    }
}
