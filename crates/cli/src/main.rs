//! `norbelys`: the command line of the Norbelys API.
//!
//! The CLI is an HTTP client of the public API and nothing else: it never reads the database
//! and does not depend on the server's code. Its shape follows the Stripe CLI's:
//!
//! - one command per public operation, derived from the OpenAPI document embedded at build
//!   time (`norbelys people create --email …`, `norbelys webhook_endpoints rotate_secret
//!   whe_…`; see `spec` and `command`);
//! - `norbelys login`, the OAuth device authorization grant in the browser, or an API key
//!   stored with `--api-key` (see `login`); profiles (`--profile`) in a private configuration
//!   file (see `config`); `--api-key` or `NORBELYS_API_KEY` for CI;
//! - `--json` for the API's JSON as it is, a readable rendering otherwise, problems with their
//!   code, detail, fields and request id, and an exit code per class of failure (see
//!   `output`);
//! - `norbelys listen --forward-to localhost:3000/hooks` and `norbelys trigger message.sent`
//!   for webhooks on a developer's machine (see `listen`).
//!
//! Everything runs on one thread: a command is one request (or a few), and the configuration
//! lock is a blocking file lock that a second task of the same process could never wait for.

mod api;
mod command;
mod config;
mod listen;
mod login;
mod output;
mod spec;
#[cfg(test)]
mod testing;

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::process::ExitCode;

use clap::ArgMatches;
use serde_json::{Value, json};

use crate::api::{Api, ApiError, Context};
use crate::command::UsageError;
use crate::config::ConfigError;
use crate::login::LoginError;
use crate::output::{Class, Terminal};
use crate::spec::{Operation, SpecError};

/// Why a run failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Arguments that make no request.
    #[error(transparent)]
    Usage(#[from] UsageError),
    /// The embedded document cannot be turned into commands.
    #[error(transparent)]
    Spec(#[from] SpecError),
    /// The configuration file cannot be read or written.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The API refused or could not be reached.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// Logging in failed.
    #[error(transparent)]
    Login(#[from] LoginError),
    /// The terminal cannot be written.
    #[error("cannot write the output: {0}")]
    Output(#[from] io::Error),
    /// The system's random source failed.
    #[error("cannot create a signing secret: the system's random source failed")]
    Random,
}

impl Error {
    /// The exit class of the failure.
    #[must_use]
    pub fn class(&self) -> Class {
        match self {
            Self::Usage(_) => Class::Usage,
            Self::Spec(_) | Self::Config(_) | Self::Output(_) | Self::Random => Class::Failure,
            Self::Api(error) => error.class(),
            Self::Login(error) => error.class(),
        }
    }
}

fn main() -> ExitCode {
    let (stdout, stderr) = (io::stdout(), io::stderr());
    let (mut out, mut err) = (stdout.lock(), stderr.lock());
    let code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(
            std::env::args_os(),
            &mut Terminal {
                out: &mut out,
                err: &mut err,
                open: login::open_browser,
            },
        )),
        Err(error) => {
            let _ = writeln!(err, "error: cannot start: {error}");
            Class::Failure.code()
        }
    };
    ExitCode::from(code)
}

/// Runs one command line and returns its exit code.
async fn run(args: impl IntoIterator<Item = OsString>, terminal: &mut Terminal<'_>) -> u8 {
    execute(args, terminal, None).await
}

/// Runs one command line; `listen` stops after `listen_limit` forwarded events when one is
/// given, which only tests do.
async fn execute(
    args: impl IntoIterator<Item = OsString>,
    terminal: &mut Terminal<'_>,
    listen_limit: Option<usize>,
) -> u8 {
    let operations = match spec::embedded() {
        Ok(operations) => operations,
        Err(error) => {
            let error = Error::from(error);
            report(&error, false, terminal);
            return error.class().code();
        }
    };
    let matches = match command::root(&operations).try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => {
            // Help and the version are "errors" to clap, printed on standard output with code 0.
            let text = error.render().to_string();
            let stream = if error.use_stderr() {
                &mut *terminal.err
            } else {
                &mut *terminal.out
            };
            let _ = stream.write_all(text.as_bytes());
            return u8::try_from(error.exit_code()).unwrap_or(Class::Usage.code());
        }
    };
    let leaf = innermost(&matches);
    let context = match Context::from_matches(leaf) {
        Ok(context) => context,
        Err(error) => {
            let error = Error::from(error);
            report(&error, false, terminal);
            return error.class().code();
        }
    };
    match dispatch(&operations, &context, &matches, terminal, listen_limit).await {
        Ok(()) => 0,
        Err(error) => {
            report(&error, context.json, terminal);
            error.class().code()
        }
    }
}

/// The matches of the innermost command, where clap gathers the global flags.
fn innermost(matches: &ArgMatches) -> &ArgMatches {
    match matches.subcommand() {
        Some((_, sub)) => innermost(sub),
        None => matches,
    }
}

/// Runs the command `matches` names.
async fn dispatch(
    operations: &[Operation],
    context: &Context,
    matches: &ArgMatches,
    terminal: &mut Terminal<'_>,
    listen_limit: Option<usize>,
) -> Result<(), Error> {
    let Some((name, sub)) = matches.subcommand() else {
        return Err(UsageError("a command is required".to_owned()).into());
    };
    match name {
        "login" => login::run(context, sub, terminal).await,
        "listen" => {
            let target =
                listen::forward_url(command::value(sub, "forward-to").map_or("", |target| target))?;
            let api = Api::connect(context).await?;
            listen::listen(context, &api, &target, terminal, listen_limit).await
        }
        "trigger" => {
            let kind = command::value(sub, "type").map_or("", |kind| kind);
            let api = Api::connect(context).await?;
            listen::trigger(
                context,
                &api,
                kind,
                command::value(sub, "webhook-endpoint-id"),
                terminal,
            )
            .await
        }
        resource => {
            let Some((action, leaf)) = sub.subcommand() else {
                return Err(UsageError(format!("`{resource}` needs an action")).into());
            };
            let operation = operations
                .iter()
                .find(|operation| operation.resource == resource && operation.action == action)
                .ok_or_else(|| UsageError(format!("no command `{resource} {action}`")))?;
            call(context, operation, leaf, terminal).await
        }
    }
}

/// Runs an operation's command: one request, or with `--all` one per page.
async fn call(
    context: &Context,
    operation: &Operation,
    matches: &ArgMatches,
    terminal: &mut Terminal<'_>,
) -> Result<(), Error> {
    let mut request = command::request(operation, matches)?;
    let api = Api::connect(context).await?;
    let all = matches.try_get_one::<bool>("all").ok().flatten() == Some(&true);
    if !all {
        let key = request.is_effectful().then(|| context.idempotency_key());
        let answer = api.send(&request, key.as_deref()).await?;
        output::answer(
            terminal,
            context.json,
            answer.body.as_ref(),
            answer.etag.as_deref(),
        )?;
        return Ok(());
    }
    let mut items = Vec::new();
    loop {
        let page = api.send(&request, None).await?.body.unwrap_or_default();
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if context.json {
            items.extend(data);
        } else {
            output::items_readable(terminal, &data)?;
        }
        let next = page
            .pointer("/meta/next_cursor")
            .and_then(Value::as_str)
            .filter(|_| page.pointer("/meta/has_more") == Some(&Value::Bool(true)));
        match next {
            Some(cursor) => request.set_query("cursor", cursor),
            None => break,
        }
    }
    if context.json {
        let whole = json!({ "data": items, "meta": { "has_more": false, "next_cursor": null } });
        output::pretty(terminal.out, &whole)?;
    }
    Ok(())
}

/// Prints a failure: with `--json`, an API problem's document on standard output for scripts;
/// otherwise `error: …` on standard error. A closed output pipe is not reported: whoever read
/// the output stopped reading on purpose.
fn report(error: &Error, json: bool, terminal: &mut Terminal<'_>) {
    let _ = match error {
        Error::Api(ApiError::Problem(problem)) if json && !problem.body.is_null() => {
            output::pretty(terminal.out, &problem.body)
        }
        Error::Output(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => writeln!(terminal.err, "error: {other}"),
    };
}
