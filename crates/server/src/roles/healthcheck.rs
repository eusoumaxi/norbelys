//! `norbelys-server healthcheck <url>`: the container health probe, a subcommand of the same
//! binary because the images are distroless (no shell, no `curl`).
//!
//! It sends one `GET` over plain HTTP/1.1 to the URL (a role's own `/health/ready`, on the
//! loopback interface of the container it checks) and exits 0 when the answer's status is
//! `2xx` (the health routes answer `204`), 1 otherwise: refused, timed out, not HTTP, or any
//! other status. The orchestrator runs it every few seconds in every container, so it is built
//! to be cheap: the standard library only, no async runtime, no TLS, and no telemetry (the
//! binary's entry point runs it before the runtime and the exporters exist), so a probe neither
//! costs a role's memory nor appears in its signals. Only `http://` URLs are accepted: a probe
//! never leaves the container, and TLS there would only add a dependency to fail.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context as _, bail};

use crate::config::HealthcheckArgs;

/// The longest status line read; a status line is far shorter.
const STATUS_LINE_LIMIT: u64 = 256;

/// Probes `args.url`.
///
/// # Errors
///
/// The URL is not `http://`, the connection fails or times out, or the answer is not a `2xx`
/// HTTP status; the message says which.
pub fn probe(args: &HealthcheckArgs) -> anyhow::Result<()> {
    let url = &args.url;
    if url.scheme() != "http" {
        bail!("the probe speaks plain HTTP to its own container; {url} is not an http:// URL");
    }
    let timeout = Duration::from_millis(args.timeout_ms);
    let address = url
        .socket_addrs(|| Some(80))
        .with_context(|| format!("cannot resolve {url}"))?
        .into_iter()
        .next()
        .with_context(|| format!("{url} resolves to no address"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("cannot connect to {address}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let authority = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
        &url[url::Position::BeforePath..url::Position::AfterQuery]
    )?;
    // TCP may split the status line; read through its end, with a bound.
    let mut line = String::new();
    BufReader::new(stream)
        .take(STATUS_LINE_LIMIT)
        .read_line(&mut line)
        .with_context(|| format!("no answer from {url}"))?;
    let status = status(&line).with_context(|| format!("{url} did not answer HTTP"))?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        bail!("{url} answered {status}")
    }
}

/// The status code of an HTTP/1.x status line, once the whole line has arrived.
fn status(line: &str) -> Option<u16> {
    if !line.ends_with('\n') {
        return None;
    }
    let mut fields = line.split_whitespace();
    match fields.next()? {
        "HTTP/1.0" | "HTTP/1.1" => fields.next()?.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Duration;

    use clap::Parser as _;

    use super::probe;
    use crate::config::{Cli, Role};

    /// The probe's arguments for `url`, as the command line parses them.
    fn args(url: &str) -> crate::config::HealthcheckArgs {
        match Cli::try_parse_from(["norbelys-server", "healthcheck", url])
            .unwrap()
            .role
        {
            Role::Healthcheck(args) => args,
            other => panic!("parsed {other:?}"),
        }
    }

    /// Serves one connection per answer, in order, writing each answer in two pieces so the
    /// status line arrives split, as TCP may deliver it.
    fn serve(answers: &'static [&'static str]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/health/ready", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                // Read the whole request first: closing a socket with unread data resets it.
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(request.starts_with(b"GET /health/ready HTTP/1.1\r\n"));
                let (head, tail) = answer.split_at(7);
                stream.write_all(head.as_bytes()).unwrap();
                std::thread::sleep(Duration::from_millis(20));
                stream.write_all(tail.as_bytes()).unwrap();
            }
        });
        url
    }

    /// Any `2xx` is healthy (the health routes answer `204`) and anything else is not, even when
    /// the status line arrives in pieces: a container is restarted on this answer alone.
    #[test]
    fn healthy_on_2xx_only() {
        let url = serve(&[
            "HTTP/1.1 204 No Content\r\n\r\n",
            "HTTP/1.1 200 OK\r\n\r\n",
            "HTTP/1.1 503 Service Unavailable\r\n\r\n",
            "SSH-2.0-OpenSSH\r\n\r\n",
        ]);
        let args = args(&url);
        assert!(probe(&args).is_ok());
        assert!(probe(&args).is_ok());
        assert!(
            probe(&args)
                .unwrap_err()
                .to_string()
                .ends_with("answered 503")
        );
        assert!(probe(&args).is_err());
    }

    /// A URL the probe cannot speak to (TLS) and a closed port are unhealthy, never a hang or a
    /// pass.
    #[test]
    fn unreachable_is_unhealthy() {
        assert!(probe(&args("https://127.0.0.1:1/health/ready")).is_err());
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(probe(&args(&format!("http://{closed}/health/ready"))).is_err());
    }
}
