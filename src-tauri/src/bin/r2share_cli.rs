//! Headless CLI companion to the r2share tray app.
fn main() {
    if let Err(e) = r2share_lib::cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
