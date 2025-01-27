use std::env;

use containerd_shim as shim;
use containerd_shim_wasm::container::Instance;
use containerd_shim_wasm::sandbox::ShimCli;
use engine::SpinEngine;

mod engine;

pub type SpinInstance = Instance<SpinEngine>;

fn parse_version() {
    let os_args: Vec<_> = env::args_os().collect();
    let flags = shim::parse(&os_args[1..]).unwrap();
    if flags.version {
        println!("{}:", os_args[0].to_string_lossy());
        println!("  Version: {}", env!("CARGO_PKG_VERSION"));
        println!("  Revision: {}", env!("CARGO_GIT_HASH"));
        println!();
        std::process::exit(0);
    }
}

fn main() {
    parse_version();
    shim::run::<ShimCli<SpinInstance>>("io.containerd.spin.v1", None);
}
