fn main() {
    // Detect newly added migration files as required by SQLx's stable migrate! macro.
    #[allow(clippy::print_stdout)]
    {
        println!("cargo:rerun-if-changed=../../crates/server/migrations");
        println!("cargo:rerun-if-changed=../../crates/server/provision.sql");
    }
}
