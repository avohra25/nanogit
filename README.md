# nanogit

A Git-compatible mini version control system written from scratch in Rust,
using only the standard library (no crates).

nanogit reads and writes the same on-disk formats as real `git`, so a
repository created by one tool can be inspected by the other.

> Work in progress. This first snapshot contains the foundations:
> SHA-1, zlib (inflate and deflate), the loose object store, `init`,
> `hash-object` and `cat-file`.

## Build and test

```sh
cargo build --release
cargo test
```

## Usage so far

```sh
nanogit init
echo 'hello' | nanogit hash-object -w --stdin
nanogit cat-file -p ce01362
```
