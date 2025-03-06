use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    // Cargo sets the CARGO_MANIFEST_DIR env var to the *crate root* (where your Cargo.toml is).
    // In your case, that's likely "ipc/api/".
    // We want to run "make" in the parent directory, i.e. "ipc/".

    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("Missing CARGO_MANIFEST_DIR");
    let parent_dir = Path::new(&crate_dir)
        .parent() // go up one level
        .expect("No parent directory found for crate directory");

    let status = Command::new("make")
        .current_dir(parent_dir) // run "make" in "ipc/" instead of "ipc/api/"
        .status()
        .expect("Failed to run make in the parent folder");

    if !status.success() {
        panic!("'make' command failed to run successfully in the 'ipc' folder");
    }
}
