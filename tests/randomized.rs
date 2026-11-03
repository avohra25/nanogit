//! Seeded randomized (property-style) tests.  Every case derives from a fixed
//! seed, so a failure message names the seed that reproduces it.

mod common;

use common::git_stdin;
use nanogit::diff::{apply_patch, diff_lines, split_lines, unified_diff, Op};
use nanogit::merge::merge_text;
use nanogit::object::{hash_object, Kind};
use nanogit::prng::Rng;
use nanogit::sha1::{sha1, to_hex, Sha1};
use nanogit::zlib::{deflate, deflate_stored, inflate, zlib_compress, zlib_decompress};

/// Random data of a random flavour: noise, tiny alphabets, long runs, repeats.
fn sample(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(4) {
        0 => rng.below(8),
        1 => rng.below(300),
        2 => rng.below(5_000),
        _ => rng.below(70_000),
    } as usize;
    match rng.below(5) {
        0 => rng.bytes(len),
        1 => rng.text(len, 2),
        2 => rng.text(len, 20),
        3 => {
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                let run = rng.range(1, 400) as usize;
                let byte = rng.next_u64() as u8;
                out.extend(std::iter::repeat(byte).take(run));
            }
            out.truncate(len);
            out
        }
        _ => {
            let width = rng.range(1, 40) as usize;
            let unit = rng.text(width, 8);
            unit.iter().cycle().take(len).copied().collect()
        }
    }
}

#[test]
fn inflate_inverts_deflate() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let data = sample(&mut rng);
        let packed = deflate(&data);
        assert_eq!(inflate(&packed).unwrap(), data, "raw deflate, seed {seed}");
        let wrapped = zlib_compress(&data);
        assert_eq!(zlib_decompress(&wrapped).unwrap(), data, "zlib, seed {seed}");
        assert_eq!(inflate(&deflate_stored(&data)).unwrap(), data, "stored, seed {seed}");
    }
}

#[test]
fn compression_helps_on_repetitive_input() {
    for seed in 0..40u64 {
        let mut rng = Rng::new(seed);
        let data = rng.text(20_000, 3);
        assert!(deflate(&data).len() < data.len() / 2, "seed {seed}");
    }
}

#[test]
fn corrupted_zlib_streams_never_panic() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed ^ 0xBAD);
        let len = rng.range(1, 2_000) as usize;
        let data = rng.text(len, 6);
        let mut packed = zlib_compress(&data);
        let at = rng.below(packed.len() as u64) as usize;
        packed[at] ^= 1 << rng.below(8);
        if rng.chance(30) {
            packed.truncate(rng.below(packed.len() as u64) as usize);
        }
        // Either an error or (rarely) the same data; it must not panic.
        if let Ok(out) = zlib_decompress(&packed) {
            assert!(out == data || at < 2 || at >= packed.len().saturating_sub(4), "seed {seed}");
        }
    }
}

#[test]
fn sha1_streaming_matches_one_shot() {
    for seed in 0..100u64 {
        let mut rng = Rng::new(seed);
        let len = rng.below(1_000) as usize;
        let data = rng.bytes(len);
        let mut h = Sha1::new();
        let mut rest = &data[..];
        while !rest.is_empty() {
            let n = (rng.range(1, 150) as usize).min(rest.len());
            h.update(&rest[..n]);
            rest = &rest[n..];
        }
        assert_eq!(h.finalize(), sha1(&data), "seed {seed}");
    }
}

#[test]
fn sha1_agrees_with_git_hash_object() {
    require_git!();
    for seed in 0..40u64 {
        let mut rng = Rng::new(seed + 7_000);
        let data = sample(&mut rng);
        let dir = common::TempDir::new("sha1-prop");
        common::git(dir.path(), &["init", "-q"]).ok();
        let theirs = git_stdin(dir.path(), &["hash-object", "--stdin"], &data).ok();
        assert_eq!(theirs.trim(), hash_object(Kind::Blob, &data).to_hex(), "seed {seed}");
        assert_eq!(to_hex(&sha1(&data)).len(), 40);
    }
}

fn lines_of(rng: &mut Rng, max_lines: u64, alphabet: u64) -> Vec<u8> {
    let count = rng.below(max_lines);
    let mut out = Vec::new();
    for _ in 0..count {
        let width = rng.below(4) as usize;
        out.extend(rng.text(width, alphabet));
        out.push(b'\n');
    }
    if rng.chance(20) {
        out.pop(); // no newline at end of file
    }
    out
}

/// Edit a few random lines of `text`.
fn mutate(rng: &mut Rng, text: &[u8]) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = split_lines(text).iter().map(|l| l.to_vec()).collect();
    for _ in 0..rng.below(6) {
        let at = rng.below(lines.len() as u64 + 1) as usize;
        match rng.below(3) {
            0 if at < lines.len() => {
                lines.remove(at);
            }
            1 if at < lines.len() => lines[at] = [rng.text(3, 5), b"\n".to_vec()].concat(),
            _ => lines.insert(at, [rng.text(2, 26), b"\n".to_vec()].concat()),
        }
    }
    lines.concat()
}

#[test]
fn edit_scripts_rebuild_the_new_text() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let a = lines_of(&mut rng, 40, 3);
        let b = if rng.chance(70) { mutate(&mut rng, &a) } else { lines_of(&mut rng, 40, 3) };
        let (la, lb) = (split_lines(&a), split_lines(&b));
        let ops = diff_lines(&la, &lb);
        let kept = ops.iter().filter(|o| **o == Op::Equal).count();
        assert_eq!(kept + ops.iter().filter(|o| **o == Op::Delete).count(), la.len(), "seed {seed}");
        assert_eq!(kept + ops.iter().filter(|o| **o == Op::Insert).count(), lb.len(), "seed {seed}");
        assert_eq!(nanogit::diff::apply_ops(&la, &lb, &ops), b, "seed {seed}");
    }
}

#[test]
fn unified_diffs_apply_back() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed + 1_000);
        let a = lines_of(&mut rng, 60, 4);
        let b = mutate(&mut rng, &a);
        let context = rng.below(5) as usize;
        let patch = unified_diff(&a, &b, context);
        assert_eq!(apply_patch(&a, &patch).unwrap(), b, "seed {seed}, context {context}");
        if a == b {
            assert!(patch.is_empty(), "seed {seed}");
        }
    }
}

/// One side's edit to a base line.
#[derive(Clone, Copy)]
enum Edit {
    Keep,
    Replace,
    Delete,
    InsertBefore,
}

/// The lines that base line `i` turns into under `edit`.
fn apply_edit(i: usize, line: &str, edit: Edit, tag: &str) -> Vec<String> {
    match edit {
        Edit::Keep => vec![line.to_string()],
        Edit::Replace => vec![format!("{tag} replaced {i}")],
        Edit::Delete => Vec::new(),
        Edit::InsertBefore => vec![format!("{tag} inserted before {i}"), line.to_string()],
    }
}

fn render(base: &[String], edits: &[Edit], tag: &str) -> Vec<String> {
    base.iter().enumerate().flat_map(|(i, line)| apply_edit(i, line, edits[i], tag)).collect()
}

fn join(lines: &[String]) -> Vec<u8> {
    lines.iter().map(|l| format!("{l}\n")).collect::<String>().into_bytes()
}

#[test]
fn non_overlapping_edits_merge_cleanly_and_symmetrically() {
    for seed in 0..400u64 {
        let mut rng = Rng::new(seed + 2_000);
        let n = rng.range(5, 60) as usize;
        let base: Vec<String> = (0..n).map(|i| format!("base line {i}")).collect();
        // Give each side its own lines, keeping at least two untouched lines
        // between edits of different sides (adjacent edits conflict, as in git).
        let (mut e_ours, mut e_theirs) = (vec![Edit::Keep; n], vec![Edit::Keep; n]);
        let mut i = 0;
        while i < n {
            if rng.chance(25) {
                let edit = *rng.pick(&[Edit::Replace, Edit::Delete, Edit::InsertBefore]);
                if rng.chance(50) { e_ours[i] = edit } else { e_theirs[i] = edit }
                i += 3;
            } else {
                i += 1;
            }
        }
        let (ours, theirs) = (join(&render(&base, &e_ours, "ours")), join(&render(&base, &e_theirs, "theirs")));
        // Expected result: each line has at most one edit, from whichever side made it.
        let mut want = Vec::new();
        for (i, line) in base.iter().enumerate() {
            let (edit, tag) = if matches!(e_ours[i], Edit::Keep) { (e_theirs[i], "theirs") } else { (e_ours[i], "ours") };
            want.extend(apply_edit(i, line, edit, tag));
        }
        let want = join(&want);
        let base = join(&base);
        let a = merge_text(&base, &ours, &theirs, ("ours", "theirs"));
        let b = merge_text(&base, &theirs, &ours, ("theirs", "ours"));
        assert_eq!(a.conflicts, 0, "seed {seed}:\n{}", String::from_utf8_lossy(&a.text));
        assert_eq!(b.conflicts, 0, "seed {seed}");
        assert_eq!(a.text, b.text, "merge must be symmetric, seed {seed}");
        assert_eq!(String::from_utf8_lossy(&a.text), String::from_utf8_lossy(&want), "seed {seed}");
    }
}

#[test]
fn merging_with_an_unchanged_side_yields_the_other_side() {
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed + 3_000);
        let base = lines_of(&mut rng, 30, 5);
        let changed = mutate(&mut rng, &base);
        for (ours, theirs) in [(&base, &changed), (&changed, &base), (&changed, &changed)] {
            let m = merge_text(&base, ours, theirs, ("a", "b"));
            assert_eq!(m.conflicts, 0, "seed {seed}");
            let expect = if ours == &base { theirs } else { ours };
            assert_eq!(&m.text, expect, "seed {seed}");
        }
    }
}

#[test]
fn conflicting_edits_keep_both_versions_in_markers() {
    for seed in 0..200u64 {
        let mut rng = Rng::new(seed + 4_000);
        let n = rng.range(3, 20) as usize;
        let base: Vec<String> = (0..n).map(|i| format!("line {i}")).collect();
        let at = rng.below(n as u64) as usize;
        let (mut ours, mut theirs) = (base.clone(), base.clone());
        ours[at] = "OURS VERSION".into();
        theirs[at] = "THEIRS VERSION".into();
        let m = merge_text(&join(&base), &join(&ours), &join(&theirs), ("HEAD", "topic"));
        assert_eq!(m.conflicts, 1, "seed {seed}");
        let text = String::from_utf8(m.text).unwrap();
        let block = "<<<<<<< HEAD\nOURS VERSION\n=======\nTHEIRS VERSION\n>>>>>>> topic\n";
        assert!(text.contains(block), "seed {seed}:\n{text}");
        // Everything outside the block is the untouched base.
        let outside = text.replace(block, &format!("line {at}\n"));
        assert_eq!(outside.as_bytes(), join(&base).as_slice(), "seed {seed}");
    }
}
