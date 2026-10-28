#![windows_subsystem = "windows"]

fn main() {
    rustshot::finish_launch_feedback();
    rustshot::run_gui();
}
