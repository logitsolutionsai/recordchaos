//! References layer: every reference resolves inside the bundle, no two
//! entries share a fullUrl, and no reference links resources of two patients.

use serde_json::Value;

use super::{Issue, LayerResult};
use crate::refs::RefGraph;

fn key_or_entry(g: &RefGraph, idx: usize) -> String {
    g.keys
        .get(idx)
        .cloned()
        .flatten()
        .unwrap_or_else(|| format!("entry {idx}"))
}

/// Check a bundle's references.
pub fn check(bundle: &Value) -> LayerResult {
    let g = RefGraph::build(bundle);
    let mut issues = Vec::new();
    for site in g.unresolved() {
        issues.push(Issue {
            path: site.path.clone(),
            message: format!("{} does not resolve inside the bundle", site.reference),
        });
    }
    for (url, idxs) in g.duplicate_full_urls() {
        let list: Vec<String> = idxs.iter().map(|i| i.to_string()).collect();
        for j in idxs.iter().skip(1) {
            issues.push(Issue {
                path: format!("/entry/{j}/fullUrl"),
                message: format!("fullUrl {url} is used by entries {}", list.join(", ")),
            });
        }
    }
    for (site, p, q) in g.cross_patient() {
        issues.push(Issue {
            path: site.path.clone(),
            message: format!(
                "{} belongs to {} but its {} reference points at a resource of {}",
                key_or_entry(&g, site.entry),
                key_or_entry(&g, p),
                site.field,
                key_or_entry(&g, q)
            ),
        });
    }
    LayerResult::from_issues(issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::LayerStatus;
    use serde_json::json;

    #[test]
    fn clean_bundle_is_valid() {
        let b = json!({"resourceType": "Bundle", "entry": [
            {"fullUrl": "https://example.org/fhir/Patient/p1", "resource": {"resourceType": "Patient", "id": "p1"}},
            {"fullUrl": "https://example.org/fhir/Encounter/e1", "resource": {"resourceType": "Encounter", "id": "e1", "subject": {"reference": "Patient/p1"}}}
        ]});
        assert_eq!(check(&b).status, LayerStatus::Valid);
    }

    #[test]
    fn dangling_and_duplicates_are_reported() {
        let b = json!({"resourceType": "Bundle", "entry": [
            {"fullUrl": "https://example.org/fhir/Patient/p1", "resource": {"resourceType": "Patient", "id": "p1"}},
            {"fullUrl": "https://example.org/fhir/Patient/p1", "resource": {"resourceType": "Encounter", "id": "e1", "subject": {"reference": "Patient/p9"}}}
        ]});
        let r = check(&b);
        assert_eq!(r.status, LayerStatus::Invalid);
        assert_eq!(r.issues.len(), 2);
    }
}
