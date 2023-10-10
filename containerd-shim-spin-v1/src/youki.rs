//! # Youki
//! Container Runtime written in Rust, inspired by [railcar](https://github.com/oracle/railcar)
//! This crate provides a container runtime which can be used by a high-level container runtime to run containers.

use anyhow::Result;
use youki::run_youki;
use containerd_shim_wasm::{sandbox::Stdio, container::executor::Executor};
use engine::SpinEngine;

mod engine;

fn main() -> Result<()> {
    run_youki(Executor::new(SpinEngine{}, Stdio::init_from_std()))
}