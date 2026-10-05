//! `cargo xtask storage smoke`: the operations the product needs from object storage (put, a
//! ranged get, list, a multipart upload, a presigned GET and delete) run against the real bucket
//! that the deployment's variables name (`OBJECT_STORE_URL`, `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT_URL`, `AWS_VIRTUAL_HOSTED_STYLE_REQUEST`),
//! before a provider is relied on.
//!
//! The server crate owns the store and its configuration, so the smoke test lives beside them, an
//! ignored test that builds the store exactly as the roles do; this command runs that one test
//! rather than talking to the bucket a second way, and fails when the test fails or when it no
//! longer exists (moving it would cause that). The test writes under `smoke/<run>/` in the bucket
//! and deletes what it wrote.

use std::path::Path;
use std::process::Command;

use anyhow::Context as _;

/// The server's smoke test of a real bucket, by its full path.
const SMOKE_TEST: &str = "storage::tests::the_configured_bucket_does_what_the_product_needs";

/// Runs the server's smoke test against the configured bucket; one finding when it fails or is
/// missing.
///
/// # Errors
///
/// Cargo cannot be started.
pub fn smoke(root: &Path) -> anyhow::Result<Vec<String>> {
    // `cargo xtask` runs under Cargo, which names itself in `CARGO`.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "test",
            "--locked",
            "-p",
            "norbelys-server",
            "--lib",
            "--",
            "--ignored",
            "--exact",
            SMOKE_TEST,
        ])
        .current_dir(root)
        .output()
        .context("cannot run cargo test")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.contains("1 passed") {
        return Ok(Vec::new());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines
        .iter()
        .skip(lines.len().saturating_sub(20))
        .copied()
        .collect::<Vec<_>>()
        .join("\n    ");
    let what = if output.status.success() {
        "no server test matches it (moved?); point SMOKE_TEST in tools/xtask/src/storage.rs at it"
    } else {
        "it failed"
    };
    Ok(vec![format!(
        "the object store smoke test {SMOKE_TEST}: {what}:\n    {tail}"
    )])
}
