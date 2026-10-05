//! The stalling SMTP server of the sender's gate: it accepts any login, sender and recipient, and
//! never answers the end of a message's content, so a submission that reaches it stays in progress
//! until its client goes away, which is the moment the gate kills the sender.
//!
//! It offers `AUTH PLAIN` and no `PIPELINING`, so a client waits for every reply, and no
//! `STARTTLS`: the mailbox is connected with `security: plain`, which a development deployment
//! allows on the loopback interface. Each session runs on a thread of its own; the server counts
//! the submissions (the `DATA` commands) it ever received, so the gate can tell when one is under
//! way and that no other followed.

use std::io::{self, BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the server does with one command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Sends these reply lines.
    Say(&'static [&'static str]),
    /// `334`, then reads the client's credentials and accepts them.
    Credentials,
    /// `354`, then reads the content and whatever follows, never answering.
    Stall,
    /// `221`, then closes the session.
    Close,
}

/// The answer to `line` (a command without its line ending), by its verb, whatever its case.
#[must_use]
pub fn answer(line: &str) -> Answer {
    let command = line.trim().to_ascii_uppercase();
    match command.split_ascii_whitespace().next().unwrap_or_default() {
        "EHLO" => Answer::Say(&["250-stall.gates", "250-8BITMIME", "250 AUTH PLAIN"]),
        "HELO" => Answer::Say(&["250 stall.gates"]),
        "AUTH" if command == "AUTH PLAIN" => Answer::Credentials,
        "AUTH" => Answer::Say(&["235 2.7.0 Authentication successful"]),
        "MAIL" | "RCPT" | "RSET" | "NOOP" => Answer::Say(&["250 2.0.0 OK"]),
        "DATA" => Answer::Stall,
        "QUIT" => Answer::Close,
        _ => Answer::Say(&["502 5.5.2 Command not recognized"]),
    }
}

/// Writes `lines`, each ended by CRLF.
fn say(stream: &mut TcpStream, lines: &[&str]) -> io::Result<()> {
    for line in lines {
        write!(stream, "{line}\r\n")?;
    }
    stream.flush()
}

/// One session: the greeting, then each command answered by [`answer`]; a `DATA` is counted in
/// `submissions` and never answered.
fn session(mut stream: TcpStream, submissions: &AtomicUsize) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    say(&mut stream, &["220 stall.gates ESMTP"])?;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        match answer(&line) {
            Answer::Say(lines) => say(&mut stream, lines)?,
            Answer::Credentials => {
                say(&mut stream, &["334 "])?;
                line.clear();
                reader.read_line(&mut line)?;
                say(&mut stream, &["235 2.7.0 Authentication successful"])?;
            }
            Answer::Stall => {
                say(&mut stream, &["354 End data with <CR><LF>.<CR><LF>"])?;
                submissions.fetch_add(1, Ordering::SeqCst);
                io::copy(&mut reader, &mut io::sink())?;
                return Ok(());
            }
            Answer::Close => return say(&mut stream, &["221 2.0.0 Bye"]),
        }
    }
}

/// Serves `listener` until the process ends, a thread per session, counting submissions in
/// `submissions`.
pub fn serve(listener: &TcpListener, submissions: &Arc<AtomicUsize>) {
    for stream in listener.incoming().flatten() {
        let submissions = Arc::clone(submissions);
        std::thread::spawn(move || {
            // A session that fails (its client went away) ends alone; the server goes on.
            let _ = session(stream, &submissions);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Answer, answer};

    /// Every command a submitting client sends gets the answer that keeps it going until the end
    /// of the content, whatever its case: the capabilities without `PIPELINING` or `STARTTLS`, a
    /// login by `PLAIN` with or without its initial response, sender and recipients accepted,
    /// then `DATA` stalls; anything else is refused rather than guessed.
    #[test]
    fn a_submission_is_accepted_up_to_its_content_then_stalls() {
        assert_eq!(
            answer("EHLO client.example\r\n"),
            Answer::Say(&["250-stall.gates", "250-8BITMIME", "250 AUTH PLAIN"])
        );
        assert_eq!(answer("helo client"), Answer::Say(&["250 stall.gates"]));
        assert_eq!(answer("AUTH PLAIN"), Answer::Credentials);
        assert_eq!(answer("auth plain\r\n"), Answer::Credentials);
        assert_eq!(
            answer("AUTH PLAIN AGdhdGVzAGdhdGVz"),
            Answer::Say(&["235 2.7.0 Authentication successful"])
        );
        for command in [
            "MAIL FROM:<gates@mailbox.example> BODY=8BITMIME",
            "RCPT TO:<crash-gate@gmail.com>",
            "rset",
            "NOOP",
        ] {
            assert_eq!(answer(command), Answer::Say(&["250 2.0.0 OK"]), "{command}");
        }
        assert_eq!(answer("DATA\r\n"), Answer::Stall);
        assert_eq!(answer("QUIT"), Answer::Close);
        for refused in ["STARTTLS", "VRFY gates", ""] {
            assert_eq!(
                answer(refused),
                Answer::Say(&["502 5.5.2 Command not recognized"]),
                "{refused}"
            );
        }
    }
}
