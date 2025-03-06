use std::process::Command;

fn main() {
    // Change directory to "ipc" and run "make"
    let status = Command::new("make")
        .status()
        .expect("Failed to run make in the 'ipc/api' folder");

    if !status.success() {
        panic!("make command failed to run successfully in 'ipc/api' folder");
    }
}
