fn main() {
    if let Err(message) = sdr_fox::transfer_probe::run(std::env::args().skip(1)) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
