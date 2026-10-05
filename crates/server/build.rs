fn main() {
    // Cargo build-script output is the compiler protocol, not application logging.
    #[allow(clippy::print_stdout)]
    {
        println!("cargo:rerun-if-changed=migrations");
    }
}
