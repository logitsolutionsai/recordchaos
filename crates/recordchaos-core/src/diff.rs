//! Line diff for the report: what the shrunk bundle changed against the base
//! (with the same entries removed). Plain LCS with context lines.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    /// Unchanged lines left out.
    Gap,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// Line number in the new text (old text for removed lines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub text: String,
}

/// Most cells the LCS table may use before falling back to a coarse diff.
const MAX_CELLS: usize = 4_000_000;

/// Diff two texts by line, keeping `context` unchanged lines around each
/// change and at most `max_lines` lines in all.
pub fn line_diff(old: &str, new: &str, context: usize, max_lines: usize) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Trim the common prefix and suffix first; bundles mostly match.
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end_a = a.len();
    let mut end_b = b.len();
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    let mid_a = &a[start..end_a];
    let mid_b = &b[start..end_b];
    // ops: (kind, old index, new index)
    let mut ops: Vec<(DiffKind, usize, usize)> = Vec::new();
    for i in 0..start {
        ops.push((DiffKind::Context, i, i));
    }
    let n = mid_a.len();
    let m = mid_b.len();
    if n.saturating_mul(m) > MAX_CELLS {
        for i in 0..n {
            ops.push((DiffKind::Removed, start + i, start));
        }
        for j in 0..m {
            ops.push((DiffKind::Added, start + n, start + j));
        }
    } else {
        // lcs[i][j] = LCS length of mid_a[i..] and mid_b[j..]
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        let w = m + 1;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * w + j] = if mid_a[i] == mid_b[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && mid_a[i] == mid_b[j] {
                ops.push((DiffKind::Context, start + i, start + j));
                i += 1;
                j += 1;
            } else if j < m && (i == n || lcs[i * w + j + 1] > lcs[(i + 1) * w + j]) {
                ops.push((DiffKind::Added, start + i, start + j));
                j += 1;
            } else {
                ops.push((DiffKind::Removed, start + i, start + j));
                i += 1;
            }
        }
    }
    for k in 0..(a.len() - end_a) {
        ops.push((DiffKind::Context, end_a + k, end_b + k));
    }
    // Keep context lines near a change.
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (k, _, _))| *k != DiffKind::Context)
        .map(|(i, _)| i)
        .collect();
    let keep = |idx: usize| changed.iter().any(|c| c.abs_diff(idx) <= context);
    let mut out: Vec<DiffLine> = Vec::new();
    let mut gap = false;
    for (idx, (kind, ia, ib)) in ops.iter().enumerate() {
        if *kind == DiffKind::Context && !keep(idx) {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(DiffLine {
                kind: DiffKind::Gap,
                line: None,
                text: String::from("…"),
            });
        }
        gap = false;
        let (line, text) = match kind {
            DiffKind::Removed => (ia + 1, a.get(*ia).copied().unwrap_or("")),
            _ => (ib + 1, b.get(*ib).copied().unwrap_or("")),
        };
        out.push(DiffLine {
            kind: *kind,
            line: Some(line),
            text: text.to_string(),
        });
        if out.len() >= max_lines {
            let rest = ops.len().saturating_sub(idx + 1);
            if rest > 0 {
                out.push(DiffLine {
                    kind: DiffKind::Gap,
                    line: None,
                    text: format!("… {rest} more lines in bundle.json"),
                });
            }
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_changes_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nX\ne\nf\ng\n";
        let d = line_diff(old, new, 1, 50);
        let kinds: Vec<DiffKind> = d.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context
            ]
        );
        assert_eq!(d[1].text, "d");
        assert_eq!(d[2].text, "X");
    }

    #[test]
    fn identical_texts_give_nothing() {
        assert!(line_diff("a\nb\n", "a\nb\n", 2, 10).is_empty());
    }

    #[test]
    fn caps_the_length() {
        let new: String = (0..100).map(|i| format!("{i}\n")).collect();
        let d = line_diff("", &new, 2, 10);
        assert_eq!(d.len(), 11);
        assert_eq!(d[10].kind, DiffKind::Gap);
    }
}
