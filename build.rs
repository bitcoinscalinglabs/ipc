use std::process::Command;

fn main() {
    // Change directory to "ipc" and run "make"
    let status = Command::new("make")
        .current_dir("ipc") // <--- set the working directory to "ipc"
        .status()
        .expect("Failed to run make in the 'ipc' folder");

    if !status.success() {
        panic!("make command failed to run successfully in 'ipc' folder");
    }
}
