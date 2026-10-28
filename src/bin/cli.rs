fn main() {
    if let Err(error) = rustshot::cli::run() {
        eprintln!("{}：{error}", rustshot::brand::NAME);
        std::process::exit(1);
    }
}
