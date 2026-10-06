//! Install the Dovecot 2.4 Lua passdb for domain/API-key SMTP submission.
//! Existing mailbox passdb/userdb remain in place. No credential is stored here.

use std::fs;
use std::path::Path;

/// Enables the adapter only when the installation explicitly configures its HTTPS API origin.
pub fn install(origin: &url::Url) -> anyhow::Result<()> {
    anyhow::ensure!(
        origin.scheme() == "https"
            && origin.username().is_empty()
            && origin.password().is_none()
            && origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none(),
        "SMTP authentication requires an HTTPS API origin"
    );
    let auth_path = Path::new("/etc/dovecot/conf.d/10-auth.conf");
    let current = fs::read_to_string(auth_path)?;
    const ANCHOR: &str = "!include auth-passwdfile.inc";
    const INCLUDE: &str = "!include auth-norbelys.inc";
    anyhow::ensure!(
        current.matches(ANCHOR).count() == 1,
        "unsupported Dovecot authentication configuration"
    );
    let config = format!(
        "# API-key authentication is never cached: revocation applies on the next login.\nauth_cache_size = 0\npassdb norbelys {{\n  driver = lua\n  lua_file = /etc/dovecot/norbelys-auth.lua\n  lua_settings {{\n    api_url = {origin}\n  }}\n  mechanisms_filter = PLAIN LOGIN\n  username_filter = *.* !*@*\n  result_success = return-ok\n  result_failure = return-fail\n  result_internalfail = return-fail\n}}\n"
    );
    fs::write(
        "/etc/dovecot/norbelys-auth.lua",
        include_str!("../dovecot/auth.lua"),
    )?;
    fs::write("/etc/dovecot/conf.d/auth-norbelys.inc", config)?;
    if !current.lines().any(|line| line == INCLUDE) {
        let updated = current.replace(ANCHOR, &format!("{INCLUDE}\n{ANCHOR}"));
        let temporary = auth_path.with_extension("norbelys-new");
        fs::write(&temporary, updated)?;
        fs::set_permissions(&temporary, fs::metadata(auth_path)?.permissions())?;
        fs::rename(temporary, auth_path)?;
    }
    Ok(())
}
