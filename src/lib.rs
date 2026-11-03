//! nanogit: a Git-compatible mini version control system written from scratch.
//!
//! The crate is dependency-free (std only).  The pieces, bottom-up:
//!
//! * [`sha1`] and [`zlib`] - hashing and compression primitives.
//! * [`object`] - blobs, trees, commits, tags and the loose object store.
//! * [`repo`] - repository layout (`.git/`), discovery and revision parsing.
//! * [`cli`] - the `nanogit` command line.
//! * [`prng`] - a seeded generator used by the randomized tests.

pub mod cli;
pub mod error;
pub mod object;
pub mod prng;
pub mod repo;
pub mod sha1;
pub mod zlib;
