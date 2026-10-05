//! `norbelys-server <command>`: the backend's runtime and operator entry point.
//!
//! The same executable runs each backend process; the first argument selects the command
//! (`api`, `sender`, `inbox`, `worker`, `tracking`, `analytics`, `admin`) and the
//! rest of the configuration comes from flags or environment variables. One binary keeps
//! the build and the deploy simple: an image is built once and each container picks its
//! role with its command. In development a `.env` file at the working directory is read
//! first; variables already set in the environment win over it.

use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    // A missing `.env` is normal outside development.
    let _ = dotenvy::dotenv();
    norbelys_server::run(norbelys_server::Cli::parse())
}
