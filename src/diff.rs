//! Line diffs: Myers' O(ND) algorithm, unified-diff output and a small
//! patch applier (used to verify round-trips).

use crate::error::{Error, Result};

/// One step of an edit script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The line is present in both inputs.
    Equal,
    /// The line only exists in the old input.
    Delete,
    /// The line only exists in the new input.
    Insert,
}

/// Split text into lines, keeping each line's terminating `\n`.
/// A final line without a newline is kept as is.
pub fn split_lines(text: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &b) in text.iter().enumerate() {
        if b == b'\n' {
            lines.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// True if the data looks binary (NUL within the first 8000 bytes, like git).
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// Compute a shortest edit script turning `a` into `b`.
///
/// Common prefixes and suffixes are stripped first, then Myers' greedy
/// algorithm runs on the remainder with a compact per-round trace.
pub fn diff_lines(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let mut ops = vec![Op::Equal; prefix];
    ops.extend(myers(&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]));
    ops.extend(std::iter::repeat(Op::Equal).take(suffix));
    ops
}

fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 {
        return vec![Op::Insert; b.len()];
    }
    if m == 0 {
        return vec![Op::Delete; a.len()];
    }
    let max = n + m;
    let offset = max + 1;
    let mut v = vec![0isize; (2 * max + 3) as usize];
    let at = |k: isize| (k + offset) as usize;
    // trace[d] holds v[-(d+1) ..= d+1] as it was before round d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'search: for d in 0..=max {
        trace.push(v[at(-d - 1)..=at(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                break 'search;
            }
            k += 2;
        }
    }
    let (mut x, mut y) = (n, m);
    let mut ops = Vec::new();
    for d in (0..trace.len() as isize).rev() {
        let t = &trace[d as usize];
        let get = |k: isize| t[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) { k + 1 } else { k - 1 };
        let (prev_x, prev_y) = if d == 0 { (0, 0) } else { (get(prev_k), get(prev_k) - prev_k) };
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Insert } else { Op::Delete });
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

/// Rebuild the new text from the old lines and an edit script.
pub fn apply_ops(a: &[&[u8]], b: &[&[u8]], ops: &[Op]) -> Vec<u8> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::Equal => {
                out.extend_from_slice(a[i]);
                i += 1;
                j += 1;
            }
            Op::Delete => i += 1,
            Op::Insert => {
                out.extend_from_slice(b[j]);
                j += 1;
            }
        }
    }
    out
}

/// A matched region used by merge: `(a_start, b_start, len)` blocks of equal lines.
pub fn matching_blocks(ops: &[Op]) -> Vec<(usize, usize, usize)> {
    let (mut i, mut j) = (0, 0);
    let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
    for op in ops {
        match op {
            Op::Equal => {
                match blocks.last_mut() {
                    Some(last) if last.0 + last.2 == i && last.1 + last.2 == j => last.2 += 1,
                    _ => blocks.push((i, j, 1)),
                }
                i += 1;
                j += 1;
            }
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    blocks
}

/// First line of a hunk header context: the last line before the hunk that
/// starts with a letter, `_` or `$` (git's default funcname heuristic).
fn function_context(a: &[&[u8]], hunk_start: usize, limit: Option<usize>) -> Option<String> {
    let mut l = hunk_start as isize - 1;
    while l >= 0 && Some(l as usize) != limit {
        let line = a[l as usize];
        if line.first().is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c == b'$') {
            let mut text = &line[..line.len().min(80)];
            while let Some((&last, rest)) = text.split_last() {
                if last.is_ascii_whitespace() {
                    text = rest;
                } else {
                    break;
                }
            }
            return Some(String::from_utf8_lossy(text).into_owned());
        }
        l -= 1;
    }
    None
}

/// Render the hunks of a unified diff (no file header) with `context` lines.
/// Returns an empty string when the inputs are identical.
pub fn unified_hunks(a: &[&[u8]], b: &[&[u8]], ops: &[Op], context: usize) -> Vec<u8> {
    let changes: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| **o != Op::Equal).map(|(i, _)| i).collect();
    if changes.is_empty() {
        return Vec::new();
    }
    // Lines consumed from a and b before each op.
    let mut apos = Vec::with_capacity(ops.len() + 1);
    let mut bpos = Vec::with_capacity(ops.len() + 1);
    let (mut i, mut j) = (0usize, 0usize);
    for op in ops {
        apos.push(i);
        bpos.push(j);
        match op {
            Op::Equal => {
                i += 1;
                j += 1;
            }
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    apos.push(i);
    bpos.push(j);

    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &c in &changes {
        match groups.last_mut() {
            Some(g) if c - g.1 - 1 <= 2 * context => g.1 = c,
            _ => groups.push((c, c)),
        }
    }
    let mut out = Vec::new();
    let mut prev_limit: Option<usize> = None;
    for (first, last) in groups {
        let start = first.saturating_sub(context);
        let end = (last + 1 + context).min(ops.len());
        let (a_start, b_start) = (apos[start], bpos[start]);
        let (a_len, b_len) = (apos[end] - a_start, bpos[end] - b_start);
        let fmt_range = |s: usize, len: usize| match len {
            0 => format!("{s},0"),
            1 => format!("{}", s + 1),
            _ => format!("{},{len}", s + 1),
        };
        let ctx = function_context(a, a_start, prev_limit);
        prev_limit = Some(a_start.saturating_sub(1));
        out.extend_from_slice(format!("@@ -{} +{} @@", fmt_range(a_start, a_len), fmt_range(b_start, b_len)).as_bytes());
        if let Some(c) = ctx {
            out.push(b' ');
            out.extend_from_slice(c.as_bytes());
        }
        out.push(b'\n');
        for idx in start..end {
            let (prefix, line) = match ops[idx] {
                Op::Equal => (b' ', a[apos[idx]]),
                Op::Delete => (b'-', a[apos[idx]]),
                Op::Insert => (b'+', b[bpos[idx]]),
            };
            out.push(prefix);
            out.extend_from_slice(line);
            if !line.ends_with(b"\n") {
                out.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// Convenience: hunks for two byte strings.
pub fn unified_diff(old: &[u8], new: &[u8], context: usize) -> Vec<u8> {
    let (a, b) = (split_lines(old), split_lines(new));
    let ops = diff_lines(&a, &b);
    unified_hunks(&a, &b, &ops, context)
}

/// Apply the hunks of a single-file unified diff to `old`.
///
/// Context and removed lines must match exactly; hunks are applied at the
/// line numbers they state.
pub fn apply_patch(old: &[u8], patch: &[u8]) -> Result<Vec<u8>> {
    let a = split_lines(old);
    let plines = split_lines(patch);
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = 0usize;
    let mut p = 0usize;
    let bad = |msg: &str| Error::fatal(format!("patch does not apply: {msg}"));
    while p < plines.len() {
        let header = std::str::from_utf8(plines[p]).map_err(|_| bad("header is not UTF-8"))?;
        if !header.starts_with("@@ ") {
            p += 1;
            continue;
        }
        let spec = header.split(' ').nth(1).ok_or_else(|| bad("malformed hunk header"))?;
        let spec = spec.strip_prefix('-').ok_or_else(|| bad("malformed hunk header"))?;
        let (start, len) = match spec.split_once(',') {
            Some((s, l)) => (s.parse::<usize>(), l.parse::<usize>()),
            None => (spec.parse::<usize>(), Ok(1)),
        };
        let (start, len) = (start.map_err(|_| bad("bad start"))?, len.map_err(|_| bad("bad length"))?);
        let begin = if len == 0 { start } else { start - 1 };
        if begin < cursor || begin > a.len() {
            return Err(bad("hunks overlap or are out of range"));
        }
        for line in &a[cursor..begin] {
            out.extend_from_slice(line);
        }
        cursor = begin;
        p += 1;
        while p < plines.len() && !plines[p].starts_with(b"@@ ") {
            let line = plines[p];
            match line.first() {
                // "\ No newline at end of file" was consumed with the line before it.
                Some(b'\\') => {}
                Some(&k @ (b' ' | b'-' | b'+')) => {
                    let mut body = &line[1..];
                    if plines.get(p + 1).is_some_and(|n| n.starts_with(b"\\")) {
                        body = body.strip_suffix(b"\n").unwrap_or(body);
                    }
                    if k == b'+' {
                        out.extend_from_slice(body);
                    } else {
                        if cursor >= a.len() || a[cursor] != body {
                            return Err(bad("context mismatch"));
                        }
                        if k == b' ' {
                            out.extend_from_slice(body);
                        }
                        cursor += 1;
                    }
                }
                _ => return Err(bad("unexpected line in hunk")),
            }
            p += 1;
        }
    }
    for line in &a[cursor..] {
        out.extend_from_slice(line);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prng::Rng;

    fn ops_of(a: &str, b: &str) -> Vec<Op> {
        diff_lines(&split_lines(a.as_bytes()), &split_lines(b.as_bytes()))
    }

    fn edit_distance(ops: &[Op]) -> usize {
        ops.iter().filter(|o| **o != Op::Equal).count()
    }

    #[test]
    fn splits_lines_keeping_terminators() {
        assert_eq!(split_lines(b""), Vec::<&[u8]>::new());
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
        assert_eq!(split_lines(b"a\n\nb\n"), vec![&b"a\n"[..], &b"\n"[..], &b"b\n"[..]]);
    }

    #[test]
    fn classic_example_is_minimal() {
        // The ABCABBA / CBABAC example from Myers' paper: shortest script has 5 edits.
        let a = "A\nB\nC\nA\nB\nB\nA\n";
        let b = "C\nB\nA\nB\nA\nC\n";
        assert_eq!(edit_distance(&ops_of(a, b)), 5);
    }

    #[test]
    fn trivial_cases() {
        assert!(ops_of("", "").is_empty());
        assert_eq!(ops_of("", "a\nb\n"), vec![Op::Insert; 2]);
        assert_eq!(ops_of("a\nb\n", ""), vec![Op::Delete; 2]);
        assert_eq!(ops_of("a\nb\n", "a\nb\n"), vec![Op::Equal; 2]);
        assert_eq!(edit_distance(&ops_of("a\n", "b\n")), 2);
    }

    #[test]
    fn unified_output_matches_git_conventions() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let new = "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\neleven\n";
        let diff = String::from_utf8(unified_diff(old.as_bytes(), new.as_bytes(), 3)).unwrap();
        let expected = "@@ -2,9 +2,10 @@ one\n two\n three\n four\n-five\n+FIVE\n six\n seven\n eight\n nine\n ten\n+eleven\n";
        assert_eq!(diff, expected);
    }

    #[test]
    fn distant_changes_make_separate_hunks_and_near_ones_merge() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let far = old.replace("line 5\n", "five\n").replace("line 35\n", "thirty-five\n");
        let d = String::from_utf8(unified_diff(old.as_bytes(), far.as_bytes(), 3)).unwrap();
        assert_eq!(d.matches("@@ ").count() / 2, 2, "{d}");
        // Gap of exactly 6 unchanged lines between changes still merges (2 * context).
        let near = old.replace("line 5\n", "five\n").replace("line 12\n", "twelve\n");
        let d = String::from_utf8(unified_diff(old.as_bytes(), near.as_bytes(), 3)).unwrap();
        assert_eq!(d.matches("@@ ").count() / 2, 1, "{d}");
    }

    #[test]
    fn missing_newline_markers() {
        let d = unified_diff(b"a\nb", b"a\nb\n", 3);
        assert_eq!(String::from_utf8(d).unwrap(), "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n");
        let d = unified_diff(b"", b"x\n", 3);
        assert_eq!(String::from_utf8(d).unwrap(), "@@ -0,0 +1 @@\n+x\n");
        let d = unified_diff(b"x\n", b"", 3);
        assert_eq!(String::from_utf8(d).unwrap(), "@@ -1 +0,0 @@\n-x\n");
        assert!(unified_diff(b"same\n", b"same\n", 3).is_empty());
    }

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text\n"));
    }

    fn random_text(rng: &mut Rng, lines: usize, alphabet: u64) -> String {
        (0..lines).map(|_| format!("l{}\n", rng.below(alphabet))).collect()
    }

    #[test]
    fn random_edit_scripts_reproduce_the_target() {
        let mut rng = Rng::new(0xD1FF);
        for round in 0..300 {
            let (la, lb) = (rng.below(40) as usize, rng.below(40) as usize);
            let alphabet = 2 + rng.below(8);
            let a_text = random_text(&mut rng, la, alphabet);
            let b_text = random_text(&mut rng, lb, alphabet);
            let (a, b) = (split_lines(a_text.as_bytes()), split_lines(b_text.as_bytes()));
            let ops = diff_lines(&a, &b);
            assert_eq!(apply_ops(&a, &b, &ops), b_text.as_bytes(), "round {round}");
            let lcs_free_bound = a.len() + b.len();
            assert!(edit_distance(&ops) <= lcs_free_bound);
            let patch = unified_hunks(&a, &b, &ops, 3);
            assert_eq!(apply_patch(a_text.as_bytes(), &patch).unwrap(), b_text.as_bytes(), "patch round {round}");
        }
    }

    #[test]
    fn patches_handle_missing_final_newlines() {
        let cases: [(&[u8], &[u8]); 5] = [
            (b"a\nb", b"a\nb\n"),
            (b"a\nb\n", b"a\nb"),
            (b"a\nb", b"a\nc"),
            (b"", b"x"),
            (b"x", b""),
        ];
        for (old, new) in cases {
            let patch = unified_diff(old, new, 3);
            assert_eq!(apply_patch(old, &patch).unwrap(), new, "{:?} -> {:?}", old, new);
        }
    }

    #[test]
    fn patch_rejects_wrong_context() {
        let patch = unified_diff(b"a\nb\nc\n", b"a\nB\nc\n", 3);
        assert!(apply_patch(b"a\nx\nc\n", &patch).is_err());
        assert_eq!(apply_patch(b"a\nb\nc\n", &patch).unwrap(), b"a\nB\nc\n");
    }

    #[test]
    fn matching_blocks_group_runs() {
        let ops = ops_of("a\nb\nx\nc\n", "a\nb\ny\nc\n");
        assert_eq!(matching_blocks(&ops), vec![(0, 0, 2), (3, 3, 1)]);
    }
}
