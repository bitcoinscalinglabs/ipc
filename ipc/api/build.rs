use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    let crate_dir =
        env::var("CARGO_MANIFEST_DIR").expect("Missing CARGO_MANIFEST_DIR environment variable");

    let contracts_dir = Path::new(&crate_dir)
        .join("../..") // go up two levels from "ipc/api" to "project_root"
        .join("contracts");

    let status = Command::new("make")
        .arg("gen")
        .current_dir(&contracts_dir)
        .status()
        .expect("Failed to run `make gen` in `contracts` directory");

    if !status.success() {
        panic!("`make gen` failed in `contracts` directory");
    }
}
