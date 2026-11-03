# nanogit

A Git-compatible mini version control system written from scratch in Rust,
using only the standard library (no crates).

nanogit reads and writes the same on-disk formats as real `git`: loose
objects, index v2, refs, `HEAD`, `.gitignore`, merge state. A repository created by one
tool can be inspected, committed to and checked out by the other, and
`git fsck --strict` is happy with whatever nanogit writes. Given the same
inputs and dates, it produces byte-identical commit ids.

## Build and test

```sh
cargo build --release
cargo test            # unit tests plus integration tests against real git
```

The integration tests shell out to `git` to cross-check nanogit's output
byte for byte (objects, commits, merge commits, `status`, `diff`, ignore decisions, `reset`). They
skip themselves with a note when `git` is not installed.

## Usage

```sh
nanogit init
echo 'hello' > greeting.txt
nanogit add greeting.txt
nanogit commit -m "first commit"
nanogit status -s
nanogit diff
nanogit branch feature
nanogit checkout feature
nanogit log --oneline
nanogit checkout main
nanogit merge feature        # fast-forward, or a three-way merge commit
```

### Merging

```sh
nanogit merge feature            # fast-forwards when it can, else merges
nanogit merge --no-ff feature    # always create a merge commit
nanogit merge --ff-only feature  # refuse unless it fast-forwards
nanogit merge --no-commit feature
nanogit merge --abort            # give up on a conflicted merge
nanogit merge-base [--all] [--is-ancestor] A B
nanogit reset [--soft|--mixed|--hard] [<commit>] [-- <path>...]
```

A three-way merge finds the best common ancestor, merges every path with the
usual rules (one-sided changes win, identical changes count once), and runs a
line-based diff3 on files both sides touched. Overlapping or adjacent edits
become conflict blocks in git's format, trimmed to the lines that really
disagree:

```text
<<<<<<< HEAD
our version
=======
their version
>>>>>>> feature
```

On conflicts nanogit writes index stages 1/2/3 (`ls-files -s` shows them),
`MERGE_HEAD`, `MERGE_MSG` and `ORIG_HEAD`, then exits with status 1. Fix the
files, `nanogit add` them and run `nanogit commit` to create the two-parent
merge commit with the prepared message. Modify/delete, add/add and binary
conflicts are reported with git's `CONFLICT (...)` wording. A merge
refuses to start over staged changes, or over local changes and untracked
files it would overwrite.

Commands: `init`, `hash-object`, `cat-file`, `add`, `rm`, `ls-files`,
`check-ignore`, `write-tree`, `commit`, `log`, `status`, `diff`, `branch`,
`checkout`, `merge`, `merge-base`, `reset`, `rev-parse`, `config`. Run `nanogit help` for flags.

Author and committer come from `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/
`GIT_COMMITTER_*` (and `GIT_*_DATE`), falling back to `user.name` and
`user.email` in the repository config. Timestamps are written with a `+0000`
zone.

## Architecture

| Module | Purpose |
| --- | --- |
| `sha1`, `zlib` | SHA-1; inflate (stored, fixed, dynamic Huffman) and a deflate encoder (hash-chain LZ77, fixed Huffman, stored fallback) |
| `object` | object ids, blobs/trees/commits/tags, the loose object store |
| `index` | index v2 reader/writer with stat data and conflict stages |
| `tree`, `commit`, `refs`, `history`, `revparse` | tree building, commit creation, branches and `HEAD`, log walking, revision parsing |
| `diff`, `difftool` | Myers O(ND) diff, unified hunks with git's context rules, git-style headers |
| `ignore` | `.gitignore` matching: globs, `**`, directory-only, anchoring, negation, nested files, `info/exclude` |
| `status` | staged vs unstaged vs untracked, rename pairing, long and short formats |
| `merge` | `merge-base` (all best ancestors), diff3 text merge with zealous conflict trimming, per-path tree merge, merge state files, `merge` command |
| `reset` | `--soft`, `--mixed`, `--hard` and path resets |
| `checkout` | two-way tree switch that refuses to clobber local changes; branch commands |
| `prng` | small seeded SplitMix64 generator used by randomized tests |

## Compatibility notes

- Loose objects only (no packfiles, no packed refs writing); reading
  repositories that have packed objects is not supported.
- Unix only (file modes, symlinks and stat data come from `std::os::unix`).
- Merging uses one merge base (the most recent); there is no recursive
  merge of several bases and **no rename detection**, so a rename plus an
  edit shows up as delete + add. File/directory clashes abort the merge
  with an error instead of being resolved.
- Merge commit messages and `CONFLICT` lines follow git's wording; given
  identical inputs and dates the merge commit id equals git's.
- `diff` output matches `git diff --no-renames` byte for byte on the covered
  cases; `status` pairs exact renames like git does.

## Tests

- Unit tests per module (SHA-1/zlib known vectors, diff, merge, ignore, status).
- `tests/git_*.rs` run nanogit and real git on copies of the same repository
  and compare commit ids, index contents, work trees and command output.
- `tests/randomized.rs` uses a seeded PRNG: `inflate(deflate(x)) == x`,
  patch round-trips, clean and symmetric merges of non-overlapping edits,
  SHA-1 against `git hash-object`.

## License

MIT
