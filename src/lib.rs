//! nanogit: a Git-compatible mini version control system written from scratch.
//!
//! The crate is dependency-free (std only).  The pieces, bottom-up:
//!
//! * [`sha1`] and [`zlib`] - hashing and compression primitives.
//! * [`object`] - blobs, trees, commits, tags and the loose object store.
//! * [`repo`] - repository layout (`.git/`), discovery and revision parsing.
//! * [`cli`] - the `nanogit` command line.
//! * [`prng`] - a seeded generator used by the randomized tests.

#[cfg(not(unix))]
compile_error!("nanogit only supports Unix-like systems (it reads POSIX file metadata)");

pub mod cli;
pub mod cmds;
pub mod commit;
pub mod config;
pub mod error;
pub mod history;
pub mod index;
pub mod object;
pub mod prng;
pub mod refs;
pub mod repo;
pub mod revparse;
pub mod sha1;
pub mod timefmt;
pub mod tree;
pub mod workdir;
pub mod zlib;
