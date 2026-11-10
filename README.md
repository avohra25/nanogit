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
byte for byte (objects, commits, merge commits, `status`, `diff`, ignore decisions, `reset`,
packs, `packed-refs`, `verify-pack` and `count-objects`). They
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

### Packing

```sh
nanogit gc                       # pack-refs --all, then repack -d
nanogit repack -d --window=20 --depth=10
nanogit pack-refs [--all]        # tags only, or every ref
nanogit verify-pack -v .git/objects/pack/pack-*.idx
nanogit count-objects -v
```

`repack` gathers every object reachable from refs, `HEAD`, merge state and
the index, plus whatever is already packed, and writes a single pack with
delta compression. With `-d` it then deletes the old packs and the loose
objects the new pack holds; unreachable loose objects are left alone, so a
repack never loses data. The new pack is re-read and every object re-hashed
before anything is deleted.

`verify-pack` checks a pack end to end (pack and index checksums, entries
tiling the file exactly, every CRC, every object resolving to the id the
index names) and with `-v` lists each entry with its size, packed size,
offset, chain length and base, followed by the chain-length histogram, in
the same format as `git verify-pack -v`. `count-objects -v` reports loose,
packed, prune-packable and garbage counts the way git does.

Commands: `init`, `hash-object`, `cat-file`, `add`, `rm`, `ls-files`,
`check-ignore`, `write-tree`, `commit`, `log`, `status`, `diff`, `branch`,
`checkout`, `merge`, `merge-base`, `reset`, `rev-parse`, `config`, `repack`,
`pack-refs`, `gc`, `verify-pack`, `count-objects`. Run `nanogit help` for flags.

Author and committer come from `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/
`GIT_COMMITTER_*` (and `GIT_*_DATE`), falling back to `user.name` and
`user.email` in the repository config. Timestamps are written with a `+0000`
zone.

## Architecture

| Module | Purpose |
| --- | --- |
| `sha1`, `zlib` | SHA-1; inflate (stored, fixed, dynamic Huffman) and a deflate encoder (hash-chain LZ77, fixed Huffman, stored fallback) |
| `object` | object ids, blobs/trees/commits/tags, the object store (loose objects first, then packs) |
| `pack`, `delta` | pack index v1/v2 parsing, pack entry decoding with delta-chain resolution and a small base cache, pack + index v2 writing with a sliding delta window, git's copy/insert delta format and a block-hash delta encoder |
| `repack` | object collection, `repack`, `pack-refs` (peeled, byte-identical to git's) and `gc` |
| `packinfo` | full pack verification, `verify-pack` and `count-objects` |
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

- Packfiles are read: objects that `git gc` or `git repack` moved into
  `objects/pack/` (index v1 or v2, `OFS_DELTA` and `REF_DELTA` chains,
  several packs at once) are found transparently, and new objects are
  written loose. `repack`, `pack-refs` and `gc` write packs (version 2,
  `OFS_DELTA`, index v2) and `packed-refs` that git reads back.
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
- `tests/git_packs.rs`, `git_repack.rs` and `git_packinfo.rs` read packs
  git wrote (after `gc --aggressive`, with `REF_DELTA`s, index v1, several
  packs, deliberate corruption) and have git read, fsck and clone what
  nanogit packs.
- `tests/randomized.rs` uses a seeded PRNG: `inflate(deflate(x)) == x`,
  `apply(base, encode(base, x)) == x` for deltas, packs written and read
  back with random windows and depths, patch round-trips, clean and symmetric merges of non-overlapping edits,
  SHA-1 against `git hash-object`.

## License

MIT
