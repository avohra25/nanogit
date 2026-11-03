# nanogit

A Git-compatible mini version control system written from scratch in Rust,
using only the standard library (no crates).

nanogit reads and writes the same on-disk formats as real `git`: loose
objects, index v2, refs, `HEAD`, `.gitignore`. A repository created by one
tool can be inspected, committed to and checked out by the other, and
`git fsck --strict` is happy with whatever nanogit writes. Given the same
inputs and dates, it produces byte-identical commit ids.

> Work in progress. Implemented so far: objects, index, commits, history,
> `status`, `diff`, `.gitignore`, branches and checkout. Merging comes next.

## Build and test

```sh
cargo build --release
cargo test            # unit tests plus integration tests against real git
```

The integration tests shell out to `git` to cross-check nanogit's output
byte for byte (objects, commits, `status`, `diff`, ignore decisions). They
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
```

Commands: `init`, `hash-object`, `cat-file`, `add`, `rm`, `ls-files`,
`check-ignore`, `write-tree`, `commit`, `log`, `status`, `diff`, `branch`,
`checkout`, `rev-parse`, `config`. Run `nanogit help` for flags.

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
| `checkout` | two-way tree switch that refuses to clobber local changes; branch commands |
| `prng` | small seeded SplitMix64 generator used by randomized tests |

## Compatibility notes

- Loose objects only (no packfiles, no packed refs writing); reading
  repositories that have packed objects is not supported.
- Unix only (file modes, symlinks and stat data come from `std::os::unix`).
- `diff` output matches `git diff --no-renames` byte for byte on the covered
  cases; `status` pairs exact renames like git does.

## License

MIT
