//! Configuration of `norbelys-smtp`, read once at start from flags or the environment; no
//! other module reads the environment. Values are checked here or, when they need more than a
//! parse (labels, host names, threshold pairs), by the subcommand before anything starts.
//!
//! Every variable, by subcommand (a flag of the same name in kebab case overrides it):
//!
//! | Variable | Used by | Default | Meaning |
//! |---|---|---|---|
//! | `NORBELYS_SMTP_STATE_DIR` | `serve`, `provision-apply` | `/var/lib/norbelys-smtp` | the bounded pending queue, locks and provisioning trigger file |
//! | `NORBELYS_TRACE_SAMPLE_PERCENT` | `serve`, `provision-apply` | `100` | trace sampling percentage, 0 through 100 |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | `serve`, `provision-apply` | unset | OTLP/HTTP endpoint; unset keeps telemetry on stdout as JSON lines |
//! | `NORBELYS_ENVIRONMENT` | `serve`, `provision-apply` | `development` | `deployment.environment.name` in telemetry |
//! | `RUST_LOG` | every subcommand | `info` | the log filter |
//! | `NORBELYS_SMTP_NODE` | `serve`, `provision-apply` | required | this host's name (`[a-z0-9-]`, at most 63): the log cursor's key, part of every event id of its remote table namespace |
//! | `NORBELYS_SMTP_DATABASE_URL` | `serve`, `provision-apply` | required | Turso Cloud libsql URL or a self-hosted libSQL HTTP(S) endpoint; no local replica |
//! | `NORBELYS_SMTP_DATABASE_TOKEN` | `serve`, `provision-apply` | empty | bearer token; empty for a private self-hosted endpoint without authentication |
//! | `NORBELYS_SMTP_QUEUE_MAX_BYTES` | `serve` | `1073741824` | maximum local queue and journal budget (4 MiB to 64 GiB); confirmation by Turso frees records |
//! | `NORBELYS_SMTP_SECRET` | `serve` | required | the per-installation Standard Webhooks secret (`whsec_` + base64 of 24–64 bytes) shared with the core; it verifies every control request and seals the route secrets at rest |
//! | `NORBELYS_SMTP_LISTEN` | `serve` | `127.0.0.1:8443` | the control API listener: the private interface, never a public one (plain HTTP: a WireGuard tunnel or a TLS proxy in front when it leaves the host) |
//! | `NORBELYS_SMTP_POLICY_LISTEN` | `serve` | `127.0.0.1:10040` | the Postfix policy-delegation listener of admission control, reachable from the mail container |
//! | `NORBELYS_SMTP_LMTP_LISTEN` | `serve` | `127.0.0.1:10025` | the LMTP listener Postfix delivers notifications for VERP return paths to (`bounce+…@<mail host>`), reachable from the mail container |
//! | `NORBELYS_SMTP_MAIL_HOST` | `serve` (required), `provision-apply` | | the MTA's public host name: MX target, the SMTP and IMAP host handed to the core, and the domain of VERP return paths; without it `provision-apply` cannot let relay logins use return paths |
//! | `NORBELYS_SMTP_PUBLIC_IPV4` | `serve` | required | the MTA's sending address, rendered into SPF |
//! | `NORBELYS_SMTP_DKIM_SELECTOR` | `serve` | required | the DKIM selector of new domains; it must match the Rspamd `dkim_signing` path template |
//! | `NORBELYS_SMTP_EVIDENCE_HOSTS` | `serve` | empty | comma-separated host names a route may post evidence to (the core's public ingress); empty refuses every route |
//! | `NORBELYS_SMTP_FBL_REPORTERS` | `serve` | empty | comma-separated domains of the enrolled feedback loops (Yahoo's, Microsoft's): an abuse report DKIM-signed by one of them, or a subdomain, authenticates its complaint; empty authenticates none |
//! | `NORBELYS_SMTP_MAIL_LOG` | `serve` | unset | Postfix's `mail.log`; unset turns the tail off |
//! | `NORBELYS_SMTP_SPOOL_DIR` | `serve` | the state directory | a path on the volume holding Postfix's queue, measured by admission control |
//! | `NORBELYS_SMTP_ADMISSION_CLOSE_FREE_BYTES` | `serve` | `2147483648` | admission closes when the spool volume has fewer free bytes than this (before Postfix's own `queue_minfree` floor) |
//! | `NORBELYS_SMTP_ADMISSION_OPEN_FREE_BYTES` | `serve` | `3221225472` | and reopens only above this |
//! | `NORBELYS_SMTP_ADMISSION_CLOSE_FREE_INODES` | `serve` | `100000` | admission closes below this many free inodes |
//! | `NORBELYS_SMTP_ADMISSION_OPEN_FREE_INODES` | `serve` | `150000` | and reopens only above this |
//! | `NORBELYS_SMTP_ADMISSION_CLOSE_BACKLOG` | `serve` | `500000` | admission closes above this many queued submissions |
//! | `NORBELYS_SMTP_ADMISSION_OPEN_BACKLOG` | `serve` | `400000` | and reopens only below this |
//! | `NORBELYS_SMTP_DMS_CONFIG_DIR` | `provision-apply` | required | docker-mailserver's configuration directory on the host (mounted at `/tmp/docker-mailserver` in the container) |
//! | `NORBELYS_SMTP_ACCOUNTS_SH` | `dms-patch` | `/usr/local/bin/helpers/accounts.sh` | the upstream helper patched inside the container |

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use secrecy::SecretString;

/// `norbelys-smtp <command>`.
#[derive(Debug, Parser)]
#[command(
    name = "norbelys-smtp",
    version,
    about = "The managed MTA's control service, provisioning helper and collector"
)]
pub struct Cli {
    /// What this process does.
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands: the long-running service, the privileged helper, and the container patch.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// The unprivileged service: control API, admission policy server, tail, events and remote archival.
    Serve(Box<ServeArgs>),
    /// The short-lived privileged helper: applies queued provisioning changes, then exits.
    ProvisionApply(ProvisionArgs),
    /// Patches docker-mailserver's `accounts.sh` inside the container, at its start.
    DmsPatch(DmsPatchArgs),
}

impl Command {
    /// The subcommand's name, the `norbelys.role` telemetry attribute.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Serve(_) => "serve",
            Self::ProvisionApply(_) => "provision-apply",
            Self::DmsPatch(_) => "dms-patch",
        }
    }

    /// The telemetry settings; `dms-patch` runs inside the container and only logs to stdout.
    #[must_use]
    pub fn telemetry(&self) -> Option<&Telemetry> {
        match self {
            Self::Serve(args) => Some(&args.common.telemetry),
            Self::ProvisionApply(args) => Some(&args.common.telemetry),
            Self::DmsPatch(_) => None,
        }
    }
}

/// Telemetry export.
#[derive(Debug, Clone, Args)]
pub struct Telemetry {
    /// The OTLP/HTTP endpoint; without it, telemetry stays on stdout.
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,
    /// Percentage of complete traces exported; unset starts with every trace at launch.
    #[arg(long, env = "NORBELYS_TRACE_SAMPLE_PERCENT", default_value = "100", value_parser = clap::value_parser!(u8).range(0..=100))]
    pub trace_sample_percent: u8,
    /// The deployment's name in telemetry.
    #[arg(long, env = "NORBELYS_ENVIRONMENT", default_value = "development")]
    pub environment: String,
}

/// Settings shared by `serve` and `provision-apply`.
#[derive(Debug, Clone, Args)]
pub struct Common {
    /// Stable node identity; isolates this host's tables in the remote database.
    #[arg(long, env = "NORBELYS_SMTP_NODE")]
    pub node: String,
    /// Remote canonical database connection.
    #[command(flatten)]
    pub database: DatabaseArgs,
    /// Local pending evidence, locks and the provisioning trigger.
    #[arg(
        long,
        env = "NORBELYS_SMTP_STATE_DIR",
        default_value = "/var/lib/norbelys-smtp"
    )]
    pub state_dir: PathBuf,
    /// Telemetry export.
    #[command(flatten)]
    pub telemetry: Telemetry,
}

impl Common {
    /// The bounded local queue; it contains no canonical database or historical replica.
    #[must_use]
    pub fn queue(&self) -> PathBuf {
        self.state_dir.join("pending.sqlite")
    }

    /// The SQLite fixture used only by the test harness.
    #[cfg(test)]
    pub fn database(&self) -> PathBuf {
        self.state_dir.join("smtp.sqlite")
    }

    /// The file whose change starts `provision-apply` (a systemd path unit watches it).
    #[must_use]
    pub fn trigger(&self) -> PathBuf {
        self.state_dir.join("provision.trigger")
    }
}

/// The `serve` subcommand.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Shared settings.
    #[command(flatten)]
    pub common: Common,

    /// The per-installation Standard Webhooks secret shared with the core.
    #[arg(long, env = "NORBELYS_SMTP_SECRET", hide_env_values = true)]
    pub secret: SecretString,
    /// The control API listener (the private interface).
    #[arg(long, env = "NORBELYS_SMTP_LISTEN", default_value = "127.0.0.1:8443")]
    pub listen: SocketAddr,
    /// Permit control HTTP on a private IP inside an encrypted tunnel. Public and wildcard
    /// binds remain refused even with this option.
    #[arg(
        long,
        env = "NORBELYS_SMTP_CONTROL_PRIVATE_TRANSPORT",
        default_value_t = false
    )]
    pub control_private_transport: bool,
    /// The Postfix policy-delegation listener of admission control.
    #[arg(
        long,
        env = "NORBELYS_SMTP_POLICY_LISTEN",
        default_value = "127.0.0.1:10040"
    )]
    pub policy_listen: SocketAddr,
    /// The LMTP listener for notifications returned to VERP paths.
    #[arg(
        long,
        env = "NORBELYS_SMTP_LMTP_LISTEN",
        default_value = "127.0.0.1:10025"
    )]
    pub lmtp_listen: SocketAddr,
    /// The MTA's public host name.
    #[arg(long, env = "NORBELYS_SMTP_MAIL_HOST")]
    pub mail_host: String,
    /// The MTA's sending address, for SPF.
    #[arg(long, env = "NORBELYS_SMTP_PUBLIC_IPV4")]
    pub public_ipv4: Ipv4Addr,
    /// The DKIM selector of new domains.
    #[arg(long, env = "NORBELYS_SMTP_DKIM_SELECTOR")]
    pub dkim_selector: String,
    /// Host names a route may post evidence to.
    #[arg(
        long,
        env = "NORBELYS_SMTP_EVIDENCE_HOSTS",
        value_delimiter = ',',
        default_value = ""
    )]
    pub evidence_hosts: Vec<String>,
    /// Domains of the enrolled feedback loops whose DKIM-signed reports authenticate a complaint.
    #[arg(
        long,
        env = "NORBELYS_SMTP_FBL_REPORTERS",
        value_delimiter = ',',
        default_value = ""
    )]
    pub fbl_reporters: Vec<String>,
    /// Postfix's `mail.log`; unset turns the tail off.
    #[arg(long, env = "NORBELYS_SMTP_MAIL_LOG")]
    pub mail_log: Option<PathBuf>,
    /// A path on the volume holding Postfix's queue; the state directory when unset.
    #[arg(long, env = "NORBELYS_SMTP_SPOOL_DIR")]
    pub spool_dir: Option<PathBuf>,
    /// Admission thresholds.
    #[command(flatten)]
    pub admission: AdmissionArgs,
    /// Maximum physical bytes reserved for the local evidence queue and its journal.
    #[arg(
        long,
        env = "NORBELYS_SMTP_QUEUE_MAX_BYTES",
        default_value_t = 1_073_741_824
    )]
    pub queue_max_bytes: u64,
}

/// Admission control's thresholds, each a closing and a reopening value: the gap between them
/// is the hysteresis that keeps admission from flapping at a boundary ([`crate::admission`]).
#[derive(Debug, Clone, Copy, Args)]
pub struct AdmissionArgs {
    /// Close below this many free bytes on the spool volume.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ADMISSION_CLOSE_FREE_BYTES",
        default_value_t = 2_147_483_648
    )]
    pub admission_close_free_bytes: u64,
    /// Reopen above this many free bytes.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ADMISSION_OPEN_FREE_BYTES",
        default_value_t = 3_221_225_472
    )]
    pub admission_open_free_bytes: u64,
    /// Close below this many free inodes.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ADMISSION_CLOSE_FREE_INODES",
        default_value_t = 100_000
    )]
    pub admission_close_free_inodes: u64,
    /// Reopen above this many free inodes.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ADMISSION_OPEN_FREE_INODES",
        default_value_t = 150_000
    )]
    pub admission_open_free_inodes: u64,
    /// Close above this many queued submissions.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ADMISSION_CLOSE_BACKLOG",
        default_value_t = 500_000
    )]
    pub admission_close_backlog: u64,
    /// Reopen below this many queued submissions.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ADMISSION_OPEN_BACKLOG",
        default_value_t = 400_000
    )]
    pub admission_open_backlog: u64,
}

/// Canonical SQL storage, accessed remotely without downloading a database file.
#[derive(Debug, Clone, Args)]
pub struct DatabaseArgs {
    /// Turso Cloud's libsql URL or the HTTP(S) URL of a self-hosted libSQL server.
    #[arg(long, env = "NORBELYS_SMTP_DATABASE_URL", hide_env_values = true)]
    pub database_url: String,
    /// Bearer token; empty only for a private self-hosted server without authentication.
    #[arg(
        long,
        env = "NORBELYS_SMTP_DATABASE_TOKEN",
        default_value = "",
        hide_env_values = true
    )]
    pub database_token: SecretString,
}

/// The `provision-apply` subcommand.
#[derive(Debug, Args)]
pub struct ProvisionArgs {
    /// Shared settings.
    #[command(flatten)]
    pub common: Common,
    /// docker-mailserver's configuration directory on the host.
    #[arg(long, env = "NORBELYS_SMTP_DMS_CONFIG_DIR")]
    pub dms_config_dir: PathBuf,
    /// The MTA's public host name, the domain of VERP return paths.
    #[arg(long, env = "NORBELYS_SMTP_MAIL_HOST")]
    pub mail_host: Option<String>,
}

/// The `dms-patch` subcommand.
#[derive(Debug, Args)]
pub struct DmsPatchArgs {
    /// The upstream helper to patch.
    #[arg(
        long,
        env = "NORBELYS_SMTP_ACCOUNTS_SH",
        default_value = "/usr/local/bin/helpers/accounts.sh"
    )]
    pub file: PathBuf,
}

/// True for a lowercase DNS label of 1 to 63 characters that starts with a letter or digit:
/// node names and DKIM selectors.
#[must_use]
pub fn is_label(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    /// The command line is internally consistent (no duplicate flags or variables, valid
    /// defaults): clap checks this only when asked, so a broken definition would otherwise
    /// surface at start on the mail host.
    #[test]
    fn command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Node names and selectors are lowercase DNS labels: they become file names, object keys
    /// and DNS owner names, so anything else is refused at start.
    #[test]
    fn labels_are_lowercase_dns_labels() {
        for valid in ["a", "mail-01", "norbe2026", &"x".repeat(63)] {
            assert!(is_label(valid), "{valid}");
        }
        for invalid in ["", "-a", "a-", "Mail", "a.b", "a_b", &"x".repeat(64)] {
            assert!(!is_label(invalid), "{invalid}");
        }
    }
}

/// The policy that `.env.example`, the repository's one list of every variable, stays complete
/// for this binary too.
#[cfg(test)]
mod env_example {
    use clap::CommandFactory as _;

    /// Every variable of `serve`, `provision-apply` and `dms-patch` (one level of subcommands,
    /// their flattened settings included) is in `.env.example`, so whoever installs the mail
    /// host finds each setting, its default and its meaning in one place.
    #[test]
    fn every_variable_is_in_env_example() {
        let example = include_str!("../../../.env.example");
        let cli = super::Cli::command();
        let variables: Vec<String> = std::iter::once(&cli)
            .chain(cli.get_subcommands())
            .flat_map(clap::Command::get_arguments)
            .filter_map(clap::Arg::get_env)
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        // The walk must reach the subcommands' arguments, or this test would pass on nothing.
        assert!(variables.iter().any(|name| name == "NORBELYS_SMTP_SECRET"));
        let mut missing: Vec<String> = variables
            .into_iter()
            .filter(|name| {
                !example.lines().any(|line| {
                    let line = line.trim_start_matches(['#', ' ']);
                    line.strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.starts_with('='))
                })
            })
            .collect();
        missing.dedup();
        assert!(missing.is_empty(), "add to .env.example: {missing:?}");
    }
}
