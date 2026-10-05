//! The text of docker-mailserver's files, as pure functions over their current text and the
//! database's state; [`super::apply`] publishes the results. Rendering is deterministic
//! (sorted by domain, then login), so an unchanged state renders byte-identical files and
//! nothing is written or reloaded. Lines the service does not manage are always kept: other
//! logins in `postfix-accounts.cf`, and aliases outside the managed block of
//! `postfix-virtual.cf`.

use std::collections::HashSet;

use crate::db::Connection;

/// The first line of the managed block of `postfix-virtual.cf`.
pub const BEGIN: &str = "# BEGIN NORBELYS MANAGED";
/// The last line of the managed block.
pub const END: &str = "# END NORBELYS MANAGED";

/// What the maps are rendered from: the enabled accounts' grants, catch-alls and rate classes.
pub struct State {
    /// `(login, domain)` of each grant, by domain then login.
    pub grants: Vec<(String, String)>,
    /// `(domain, login)` of each catch-all, by domain.
    pub catch_alls: Vec<(String, String)>,
    /// The logins of the `relay` rate class.
    pub relays: Vec<String>,
}

impl State {
    /// Reads the enabled accounts.
    ///
    /// # Errors
    ///
    /// A query fails.
    pub fn load(conn: &Connection) -> crate::db::Result<Self> {
        let pairs = |sql: &str| -> crate::db::Result<Vec<(String, String)>> {
            let stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect()
        };
        let grants = pairs(
            "SELECT username, grant_domain FROM accounts
              WHERE disabled_at IS NULL AND grant_domain IS NOT NULL ORDER BY grant_domain, username",
        )?;
        let catch_alls = pairs(
            "SELECT domain, username FROM accounts
              WHERE disabled_at IS NULL AND catch_all = 1 ORDER BY domain, username",
        )?;
        let stmt = conn.prepare(
            "SELECT username FROM accounts WHERE disabled_at IS NULL AND rate_class = 'relay' ORDER BY username",
        )?;
        let relays = stmt
            .query_map([], |row| row.get(0))?
            .collect::<crate::db::Result<_>>()?;
        Ok(Self {
            grants,
            catch_alls,
            relays,
        })
    }
}

/// The logins of `postfix-accounts.cf`: the text before the first `|` of each line that is not
/// empty or a comment.
#[must_use]
pub fn logins(accounts: &str) -> Vec<&str> {
    accounts.lines().filter_map(login_of).collect()
}

fn login_of(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    trimmed.split('|').next()
}

fn with_line(accounts: &str, line: &str) -> String {
    let mut out = accounts.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
    out
}

/// Adds `login|credential`. A login already present with this credential (a repeated run) is
/// left as is; present with another credential, it is refused: the service never takes over a
/// login it did not create.
///
/// # Errors
///
/// The login exists with another credential.
pub fn create_account(accounts: &str, login: &str, credential: &str) -> Result<String, String> {
    let existing = accounts
        .lines()
        .find(|line| login_of(line) == Some(login))
        .map(|line| line.trim().split('|').nth(1).unwrap_or_default());
    match existing {
        None => Ok(with_line(accounts, &format!("{login}|{credential}"))),
        Some(current) if current == credential => Ok(accounts.to_owned()),
        Some(_) => Err(format!(
            "{login} exists in postfix-accounts.cf with another credential"
        )),
    }
}

/// Sets `login`'s credential, keeping any attributes after it; adds the login when absent.
#[must_use]
pub fn set_account(accounts: &str, login: &str, credential: &str) -> String {
    if !accounts.lines().any(|line| login_of(line) == Some(login)) {
        return with_line(accounts, &format!("{login}|{credential}"));
    }
    let mut out = String::with_capacity(accounts.len());
    for line in accounts.lines() {
        if login_of(line) == Some(login) {
            let mut fields = line.trim().splitn(3, '|');
            let _ = (fields.next(), fields.next());
            match fields.next() {
                Some(attributes) => out.push_str(&format!("{login}|{credential}|{attributes}")),
                None => out.push_str(&format!("{login}|{credential}")),
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Removes `login`'s line.
#[must_use]
pub fn remove_account(accounts: &str, login: &str) -> String {
    if !accounts.lines().any(|line| login_of(line) == Some(login)) {
        return accounts.to_owned();
    }
    accounts
        .lines()
        .filter(|line| login_of(line) != Some(login))
        .fold(String::with_capacity(accounts.len()), |mut out, line| {
            out.push_str(line);
            out.push('\n');
            out
        })
}

fn escape(domain: &str) -> String {
    domain.replace('.', "\\.").replace('-', "\\-")
}

/// `domain-senders.pcre`: a granted login may send as any address of its domain, and each
/// address keeps its own login; given the MTA's host name, the relay logins may also use the
/// VERP return paths `bounce+<token>@<mail host>` as envelope senders ([`crate::bounce`]).
#[must_use]
pub fn domain_senders(
    grants: &[(String, String)],
    mail_host: Option<&str>,
    relays: &[String],
) -> String {
    let mut map: String = grants
        .iter()
        .map(|(login, domain)| format!("/^([^@]+)@{}$/ {login},${{1}}@{domain}\n", escape(domain)))
        .collect();
    if let Some(host) = mail_host
        && !relays.is_empty()
    {
        map.push_str(&format!(
            "/^bounce\\+[0-9a-z]{{{},{}}}@{}$/ {}\n",
            crate::bounce::TOKEN_MIN,
            crate::bounce::TOKEN_MAX,
            escape(host),
            relays.join(",")
        ));
    }
    map
}

/// `sender-domains.map`: the Rspamd sender policy's `login domain` grants.
#[must_use]
pub fn sender_domains(grants: &[(String, String)]) -> String {
    grants
        .iter()
        .map(|(login, domain)| format!("{login} {domain}\n"))
        .collect()
}

/// `relay-logins.map`: one login of the `relay` rate class per line.
#[must_use]
pub fn relay_logins(relays: &[String]) -> String {
    relays.iter().map(|login| format!("{login}\n")).collect()
}

/// `postfix-virtual.cf` with its managed block rebuilt: for each catch-all domain, every real
/// mailbox of that domain mapped to itself (unless an unmanaged line already names it) and then
/// `@domain login`; the block carries `digest` so a change in any rendered map changes this
/// file. Unmanaged lines are kept as they are.
///
/// # Errors
///
/// The block has no end marker, a domain has two catch-alls, or an unmanaged catch-all exists
/// for a managed one's domain.
pub fn virtual_with_block(
    current: &str,
    catch_alls: &[(String, String)],
    logins: &[&str],
    digest: &str,
) -> Result<String, String> {
    let mut unmanaged = Vec::new();
    let mut inside = false;
    for line in current.lines() {
        match (inside, line.trim()) {
            (false, BEGIN) => inside = true,
            (false, _) => unmanaged.push(line),
            (true, END) => inside = false,
            (true, _) => {}
        }
    }
    if inside {
        return Err("postfix-virtual.cf has a managed block without its end marker".to_owned());
    }
    let original: HashSet<&str> = unmanaged
        .iter()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_whitespace().next())
        .collect();

    let mut managed = vec![format!("# maps sha256:{digest}")];
    let mut domains = HashSet::new();
    for (domain, login) in catch_alls {
        let wildcard = format!("@{domain}");
        if !domains.insert(domain.as_str()) {
            return Err(format!("{domain} has more than one catch-all account"));
        }
        if original.contains(wildcard.as_str()) {
            return Err(format!(
                "postfix-virtual.cf has an unmanaged catch-all for {domain}; remove it first"
            ));
        }
        for mailbox in logins {
            if mailbox.ends_with(&wildcard) && !original.contains(mailbox) {
                managed.push(format!("{mailbox} {mailbox}"));
            }
        }
        managed.push(format!("{wildcard} {login}"));
    }

    let mut out = unmanaged.join("\n").trim_end().to_owned();
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(BEGIN);
    out.push('\n');
    for line in managed {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(END);
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNTS: &str =
        "# comment\nowner@example.com|{SHA512-CRYPT}$6$x|userdb_quota_rule=*:storage=1G\n";

    /// A create appends the login, a repeated create with the same credential changes nothing
    /// (a run after a crash), and a login already present with another credential is refused:
    /// the service never takes over a login it did not create.
    #[test]
    fn create_appends_and_never_takes_over() {
        let created = create_account(ACCOUNTS, "a@example.com", "{SCRAM-SHA-256}c").unwrap();
        assert_eq!(
            created,
            format!("{ACCOUNTS}a@example.com|{{SCRAM-SHA-256}}c\n")
        );
        assert_eq!(
            create_account(&created, "a@example.com", "{SCRAM-SHA-256}c").unwrap(),
            created
        );
        assert!(create_account(ACCOUNTS, "owner@example.com", "{SCRAM-SHA-256}c").is_err());
        assert_eq!(
            create_account("x@y.z|h", "a@example.com", "c").unwrap(),
            "x@y.z|h\na@example.com|c\n"
        );
    }

    /// A new credential replaces the old one and keeps the login's attributes; a login missing
    /// from the file (disabled earlier) is added back; removal drops only that login.
    #[test]
    fn set_and_remove_touch_only_their_login() {
        let set = set_account(ACCOUNTS, "owner@example.com", "{SCRAM-SHA-256}n");
        assert_eq!(
            set,
            "# comment\nowner@example.com|{SCRAM-SHA-256}n|userdb_quota_rule=*:storage=1G\n"
        );
        assert_eq!(
            set_account(ACCOUNTS, "b@example.com", "c"),
            format!("{ACCOUNTS}b@example.com|c\n")
        );
        assert_eq!(remove_account(ACCOUNTS, "owner@example.com"), "# comment\n");
        assert_eq!(remove_account(ACCOUNTS, "absent@example.com"), ACCOUNTS);
        assert_eq!(logins(ACCOUNTS), vec!["owner@example.com"]);
    }

    /// The grant maps: Postfix's sender-login map lets a granted login use any address of its
    /// domain (the domain's dots escaped, so `exampleXcom` cannot match) and, given the MTA's
    /// host, lets the relay logins use its VERP return paths; Rspamd's grant map pairs login and
    /// domain, and the relay map lists the logins without a per-login bucket.
    #[test]
    fn renders_the_grant_and_relay_maps() {
        let grants = vec![("relay@ex-ample.com".to_owned(), "ex-ample.com".to_owned())];
        let relays = vec![
            "relay@ex-ample.com".to_owned(),
            "core@ex-ample.com".to_owned(),
        ];
        assert_eq!(
            domain_senders(&grants, Some("mail.example.com"), &relays),
            "/^([^@]+)@ex\\-ample\\.com$/ relay@ex-ample.com,${1}@ex-ample.com\n\
             /^bounce\\+[0-9a-z]{16,56}@mail\\.example\\.com$/ relay@ex-ample.com,core@ex-ample.com\n"
        );
        assert_eq!(sender_domains(&grants), "relay@ex-ample.com ex-ample.com\n");
        assert_eq!(
            relay_logins(&relays),
            "relay@ex-ample.com\ncore@ex-ample.com\n"
        );
        assert_eq!(domain_senders(&[], None, &relays), "");
        assert_eq!(domain_senders(&[], Some("mail.example.com"), &[]), "");
    }

    /// The managed block is rebuilt in place: unmanaged aliases stay, the previous block is
    /// replaced, each real mailbox of a catch-all domain is mapped to itself (unless an
    /// unmanaged line already names it) before the wildcard, and the digest line changes the
    /// file whenever a rendered map changes.
    #[test]
    fn rebuilds_the_managed_block() {
        let current = format!(
            "support@example.com owner@example.com\n{BEGIN}\nstale line\n{END}\n# kept comment\n"
        );
        let catch_alls = vec![("example.com".to_owned(), "relay@example.com".to_owned())];
        let logins = [
            "owner@example.com",
            "relay@example.com",
            "support@example.com",
            "x@other.com",
        ];
        let rendered = virtual_with_block(&current, &catch_alls, &logins, "d1").unwrap();
        assert_eq!(
            rendered,
            format!(
                "support@example.com owner@example.com\n# kept comment\n{BEGIN}\n# maps sha256:d1\n\
                 owner@example.com owner@example.com\nrelay@example.com relay@example.com\n\
                 @example.com relay@example.com\n{END}\n"
            )
        );
        assert_ne!(
            virtual_with_block(&rendered, &catch_alls, &logins, "d2").unwrap(),
            rendered
        );
        assert_eq!(
            virtual_with_block("", &[], &[], "d").unwrap(),
            format!("{BEGIN}\n# maps sha256:d\n{END}\n")
        );
    }

    /// The block is not written when it would be wrong: a block without its end marker, an
    /// unmanaged catch-all for a managed domain, or two catch-alls for one domain.
    #[test]
    fn refuses_a_block_it_cannot_render_safely() {
        let one = vec![("example.com".to_owned(), "a@example.com".to_owned())];
        assert!(virtual_with_block(&format!("{BEGIN}\nx\n"), &[], &[], "d").is_err());
        assert!(virtual_with_block("@example.com old@example.com\n", &one, &[], "d").is_err());
        let two = vec![
            one[0].clone(),
            ("example.com".to_owned(), "b@example.com".to_owned()),
        ];
        assert!(virtual_with_block("", &two, &[], "d").is_err());
    }
}
