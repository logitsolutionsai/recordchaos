//! Reference graph for a bundle: indexes `fullUrl`, `Type/id`, `urn:uuid` and
//! absolute URLs, finds every `reference` string and resolves it inside the
//! bundle.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::bundle::{entries, resource, resource_key};
use crate::json::ptr_join;

/// One place in the bundle that holds a reference string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefSite {
    /// Index of the entry that holds the reference.
    pub entry: usize,
    /// JSON Pointer from the bundle root to the `reference` string.
    pub path: String,
    /// Pointer relative to the resource, e.g. `/subject/reference`.
    pub rel_path: String,
    /// The member that holds the Reference object, e.g. `subject` or `result`.
    pub field: String,
    /// True when the Reference sits directly on the resource (not nested).
    pub top_level: bool,
    /// True when the reference sits inside a contained resource.
    pub in_contained: bool,
    pub reference: String,
}

/// Where a reference points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Entry(usize),
    Contained,
    Unresolved,
}

/// Reference graph of one bundle.
#[derive(Clone, Debug, Default)]
pub struct RefGraph {
    pub full_urls: BTreeMap<String, Vec<usize>>,
    pub by_key: BTreeMap<String, Vec<usize>>,
    pub keys: Vec<Option<String>>,
    pub types: Vec<Option<String>>,
    pub entry_full_urls: Vec<Option<String>>,
    pub sites: Vec<RefSite>,
    pub resolved: Vec<Resolution>,
}

fn strip_history(r: &str) -> &str {
    match r.find("/_history/") {
        Some(idx) => &r[..idx],
        None => r,
    }
}

/// True for `urn:uuid:`, `urn:oid:` and absolute `http(s)` style references.
pub fn is_absolute(r: &str) -> bool {
    r.starts_with("urn:") || r.contains("://")
}

/// For a RESTful fullUrl `base/Type/id`, return `base`.
pub fn rest_base(full_url: &str) -> Option<&str> {
    if !full_url.contains("://") {
        return None;
    }
    let mut it = full_url.rsplitn(3, '/');
    let _id = it.next()?;
    let _ty = it.next()?;
    let base = it.next()?;
    if base.ends_with(':') || base.ends_with('/') {
        return None;
    }
    Some(base)
}

/// Split a relative `Type/id` reference.
pub fn split_relative(r: &str) -> Option<(&str, &str)> {
    if is_absolute(r) || r.starts_with('#') {
        return None;
    }
    let r = strip_history(r);
    let (t, id) = r.split_once('/')?;
    if t.is_empty() || id.is_empty() || id.contains('/') {
        return None;
    }
    if !t.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return None;
    }
    Some((t, id))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    value: &Value,
    path: &str,
    rel: &str,
    field: &str,
    depth: usize,
    in_contained: bool,
    entry: usize,
    out: &mut Vec<RefSite>,
) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("reference") {
                out.push(RefSite {
                    entry,
                    path: ptr_join(path, "reference"),
                    rel_path: ptr_join(rel, "reference"),
                    field: field.to_string(),
                    top_level: depth == 1,
                    in_contained,
                    reference: r.clone(),
                });
            }
            for (k, v) in map {
                if k == "reference" {
                    continue;
                }
                let contained = in_contained || (depth == 0 && k == "contained");
                walk(
                    v,
                    &ptr_join(path, k),
                    &ptr_join(rel, k),
                    k,
                    depth + 1,
                    contained,
                    entry,
                    out,
                );
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                // Array items keep the field name of the array and its depth.
                walk(
                    v,
                    &format!("{path}/{i}"),
                    &format!("{rel}/{i}"),
                    field,
                    depth,
                    in_contained,
                    entry,
                    out,
                );
            }
        }
        _ => {}
    }
}

impl RefGraph {
    /// Build the graph for a bundle.
    pub fn build(bundle: &Value) -> RefGraph {
        let mut g = RefGraph::default();
        for (i, entry) in entries(bundle).iter().enumerate() {
            let full_url = entry
                .get("fullUrl")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(f) = &full_url {
                g.full_urls.entry(f.clone()).or_default().push(i);
            }
            g.entry_full_urls.push(full_url);
            let res = resource(entry);
            let key = res.and_then(resource_key);
            if let Some(k) = &key {
                g.by_key.entry(k.clone()).or_default().push(i);
            }
            g.keys.push(key);
            g.types.push(
                res.and_then(|r| r.get("resourceType"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            );
            if let Some(r) = res {
                let path = format!("/entry/{i}/resource");
                walk(r, &path, "", "", 0, false, i, &mut g.sites);
            }
        }
        g.resolved = g
            .sites
            .iter()
            .map(|s| g.resolve(s.entry, &s.reference))
            .collect();
        g
    }

    /// Resolve a reference made from the given entry.
    pub fn resolve(&self, from_entry: usize, reference: &str) -> Resolution {
        if reference.starts_with('#') {
            return Resolution::Contained;
        }
        let plain = strip_history(reference);
        for candidate in [reference, plain] {
            if let Some(found) = self.full_urls.get(candidate) {
                if let Some(first) = found.first() {
                    return Resolution::Entry(*first);
                }
            }
        }
        if is_absolute(reference) {
            return Resolution::Unresolved;
        }
        if split_relative(plain).is_none() {
            return Resolution::Unresolved;
        }
        // Relative reference: resolve against the RESTful base of the
        // referencing entry first, then by Type/id anywhere in the bundle.
        if let Some(Some(full_url)) = self.entry_full_urls.get(from_entry) {
            if let Some(base) = rest_base(full_url) {
                let joined = format!("{base}/{plain}");
                if let Some(found) = self.full_urls.get(&joined) {
                    if let Some(first) = found.first() {
                        return Resolution::Entry(*first);
                    }
                }
            }
        }
        match self.by_key.get(plain).and_then(|v| v.first()) {
            Some(idx) => Resolution::Entry(*idx),
            None => Resolution::Unresolved,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The Patient entry a resource belongs to: itself for a Patient, else the
    /// target of its top-level `subject` or `patient` reference.
    pub fn patient_of(&self, entry: usize) -> Option<usize> {
        if self.types.get(entry)?.as_deref() == Some("Patient") {
            return Some(entry);
        }
        for (site, res) in self.sites.iter().zip(self.resolved.iter()) {
            if site.entry != entry || !site.top_level || site.in_contained {
                continue;
            }
            if site.field != "subject" && site.field != "patient" {
                continue;
            }
            if let Resolution::Entry(target) = res {
                if self.types.get(*target).and_then(|t| t.as_deref()) == Some("Patient") {
                    return Some(*target);
                }
            }
        }
        None
    }

    /// Patient entry for every entry, computed in one pass.
    pub fn patient_map(&self) -> Vec<Option<usize>> {
        let mut out: Vec<Option<usize>> = self
            .types
            .iter()
            .enumerate()
            .map(|(i, t)| (t.as_deref() == Some("Patient")).then_some(i))
            .collect();
        for (site, res) in self.sites.iter().zip(self.resolved.iter()) {
            if !site.top_level
                || site.in_contained
                || (site.field != "subject" && site.field != "patient")
            {
                continue;
            }
            let Resolution::Entry(target) = res else {
                continue;
            };
            if self.types.get(*target).and_then(|t| t.as_deref()) != Some("Patient") {
                continue;
            }
            if let Some(slot) = out.get_mut(site.entry) {
                if slot.is_none() {
                    *slot = Some(*target);
                }
            }
        }
        out
    }

    /// Sites (with their resolution) that point at the given entry.
    pub fn referrers(&self, entry: usize) -> Vec<&RefSite> {
        self.sites
            .iter()
            .zip(self.resolved.iter())
            .filter(|(_, r)| **r == Resolution::Entry(entry))
            .map(|(s, _)| s)
            .collect()
    }

    /// Sites held by the given entry.
    pub fn sites_of(&self, entry: usize) -> Vec<(&RefSite, Resolution)> {
        self.sites
            .iter()
            .zip(self.resolved.iter())
            .filter(|(s, _)| s.entry == entry)
            .map(|(s, r)| (s, *r))
            .collect()
    }

    /// Sites that do not resolve inside the bundle.
    pub fn unresolved(&self) -> Vec<&RefSite> {
        self.sites
            .iter()
            .zip(self.resolved.iter())
            .filter(|(_, r)| **r == Resolution::Unresolved)
            .map(|(s, _)| s)
            .collect()
    }

    /// fullUrls used by more than one entry.
    pub fn duplicate_full_urls(&self) -> Vec<(&String, &Vec<usize>)> {
        self.full_urls.iter().filter(|(_, v)| v.len() > 1).collect()
    }

    /// References that link a resource of one patient to a resource of another.
    /// Returns (site, patient of the holder, patient of the target).
    pub fn cross_patient(&self) -> Vec<(&RefSite, usize, usize)> {
        let pats = self.patient_map();
        let mut out = Vec::new();
        for (site, res) in self.sites.iter().zip(self.resolved.iter()) {
            if site.in_contained || site.field == "subject" || site.field == "patient" {
                continue;
            }
            if self.types.get(site.entry).and_then(|t| t.as_deref()) == Some("Patient") {
                continue;
            }
            let Resolution::Entry(target) = res else {
                continue;
            };
            if self.types.get(*target).and_then(|t| t.as_deref()) == Some("Patient") {
                // A direct link to a Patient outside subject/patient (for example
                // a performer) is not a crossing by itself.
                continue;
            }
            let p = pats.get(site.entry).copied().flatten();
            let q = pats.get(*target).copied().flatten();
            if let (Some(p), Some(q)) = (p, q) {
                if p != q {
                    out.push((site, p, q));
                }
            }
        }
        out
    }

    /// Patient entry indexes in the bundle.
    pub fn patients(&self) -> Vec<usize> {
        self.types
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_deref() == Some("Patient"))
            .map(|(i, _)| i)
            .collect()
    }

    /// Entry index for a resource key.
    pub fn entry_of_key(&self, key: &str) -> Option<usize> {
        self.by_key.get(key).and_then(|v| v.first().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "collection",
            "entry": [
                {"fullUrl": "https://example.org/fhir/Patient/p1",
                 "resource": {"resourceType": "Patient", "id": "p1"}},
                {"fullUrl": "urn:uuid:5b0e7c1a-9d2f-4c11-8f6e-2a7d3c9b1e04",
                 "resource": {"resourceType": "Patient", "id": "p2"}},
                {"fullUrl": "https://example.org/fhir/Encounter/e1",
                 "resource": {"resourceType": "Encounter", "id": "e1",
                              "subject": {"reference": "Patient/p1"}}},
                {"fullUrl": "https://example.org/fhir/Observation/o1",
                 "resource": {"resourceType": "Observation", "id": "o1",
                              "subject": {"reference": "urn:uuid:5b0e7c1a-9d2f-4c11-8f6e-2a7d3c9b1e04"},
                              "encounter": {"reference": "https://example.org/fhir/Encounter/e1"},
                              "hasMember": [{"reference": "Observation/missing"}],
                              "derivedFrom": [{"reference": "Observation/o1/_history/2"}]}}
            ]
        })
    }

    #[test]
    fn resolves_every_reference_form() {
        let g = RefGraph::build(&sample());
        assert_eq!(g.resolve(2, "Patient/p1"), Resolution::Entry(0));
        assert_eq!(
            g.resolve(3, "urn:uuid:5b0e7c1a-9d2f-4c11-8f6e-2a7d3c9b1e04"),
            Resolution::Entry(1)
        );
        assert_eq!(
            g.resolve(3, "https://example.org/fhir/Encounter/e1"),
            Resolution::Entry(2)
        );
        assert_eq!(
            g.resolve(3, "Observation/o1/_history/2"),
            Resolution::Entry(3)
        );
        assert_eq!(g.resolve(3, "#contained-1"), Resolution::Contained);
        assert_eq!(g.resolve(3, "Observation/missing"), Resolution::Unresolved);
        assert_eq!(
            g.resolve(3, "https://elsewhere.example.org/Patient/p1"),
            Resolution::Unresolved
        );
    }

    #[test]
    fn finds_sites_and_dangling_references() {
        let g = RefGraph::build(&sample());
        assert_eq!(g.sites.len(), 5);
        let dangling = g.unresolved();
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].path, "/entry/3/resource/hasMember/0/reference");
        assert_eq!(dangling[0].field, "hasMember");
    }

    #[test]
    fn finds_cross_patient_links() {
        let g = RefGraph::build(&sample());
        assert_eq!(g.patient_of(2), Some(0));
        assert_eq!(g.patient_of(3), Some(1));
        let cross = g.cross_patient();
        assert_eq!(cross.len(), 1);
        assert_eq!(cross[0].0.field, "encounter");
    }

    #[test]
    fn finds_duplicate_full_urls() {
        let mut b = sample();
        b["entry"][2]["fullUrl"] = json!("https://example.org/fhir/Patient/p1");
        let g = RefGraph::build(&b);
        assert_eq!(g.duplicate_full_urls().len(), 1);
    }

    #[test]
    fn rest_base_of_full_urls() {
        assert_eq!(
            rest_base("https://example.org/fhir/Patient/p1"),
            Some("https://example.org/fhir")
        );
        assert_eq!(rest_base("urn:uuid:abc"), None);
    }
}
