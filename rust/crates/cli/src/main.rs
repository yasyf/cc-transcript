fn main() {
    cc_transcript_cli::install_sigint_handler();
    std::process::exit(cc_transcript_cli::run(std::env::args().collect()));
}
