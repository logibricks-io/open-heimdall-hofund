//! Menu-bar process. It delegates to [`hofund_desktop::run`].

fn main() {
    if let Err(error) = hofund_desktop::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
