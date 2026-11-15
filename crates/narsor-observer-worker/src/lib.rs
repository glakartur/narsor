//! `narsor-observer-worker`: the stage-1 observer worker (U6).
//!
//! A program of its own, installed next to `narsor-server`, which starts
//! and supervises it when its configuration enables the worker (D5, D20).
//! [`worker`] claims due runs and executes their scripts; [`cli`] reads the
//! flags and the key and starts it.

pub mod cli;
pub mod worker;
