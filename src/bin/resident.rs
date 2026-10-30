#![windows_subsystem = "windows"]

fn main() {
    if std::env::args().skip(1).eq(["--exit"]) {
        std::process::exit(rustshot::exit_resident());
    }
    rustshot::finish_launch_feedback();
    rustshot::run_gui();
}
