-- Dovecot 2.4 passdb: a workspace API key authenticates SMTP, never IMAP.
-- The API checks key revocation, scope, domain ownership and service health.
local client
local origin

function script_init(settings)
  origin = settings.api_url
  if not origin or not origin:match("^https://[a-zA-Z0-9.:%-]+/$") then
    error("SMTP authentication requires a configured HTTPS API origin")
  end
  client = dovecot.http.client {
    auto_redirect = false,
    auto_retry = false,
    request_max_attempts = 1,
    connect_timeout = 3000,
    request_timeout = 8000,
    request_absolute_timeout = 8000,
    ssl_client_require_valid_cert = true,
    ssl_client_ca_file = "/etc/ssl/certs/ca-certificates.crt",
  }
  return 0
end

local function verify(request, password)
  local domain = string.lower(request.user or "")
  if request.service ~= "smtp" or (request.master_user and request.master_user ~= "")
      or not password or #password > 512 or not password:match("^nb_live_[a-zA-Z0-9]+$")
      or #domain > 253 or not domain:match("^[a-zA-Z0-9][a-zA-Z0-9.%-]+$") then
    return dovecot.auth.PASSDB_RESULT_PASSWORD_MISMATCH, {}
  end
  local lookup = client:request {
    method = "GET",
    url = origin .. "v1/smtp/auth?domain=" .. domain,
  }
  lookup:add_header("Authorization", "Bearer " .. password)
  local response = lookup:submit()
  local status = response:status()
  if status ~= 200 then
    if status >= 500 or status == 429 then
      return dovecot.auth.PASSDB_RESULT_INTERNAL_FAILURE, "SMTP authorization unavailable"
    end
    return dovecot.auth.PASSDB_RESULT_PASSWORD_MISMATCH, {}
  end
  local body = response:payload()
  if not body or #body > 2048 then
    return dovecot.auth.PASSDB_RESULT_INTERNAL_FAILURE, "Invalid SMTP authorization response"
  end
  local username = body:match('^%s*{%s*"username"%s*:%s*"([a-z0-9%-@.]+)"%s*}%s*$')
  local tenant, returned_domain
  if username then
    tenant, returned_domain = username:match("^norbelys%-([a-f0-9]+)@(.+)$")
  end
  if not tenant or #tenant ~= 32 or returned_domain ~= domain then
    return dovecot.auth.PASSDB_RESULT_INTERNAL_FAILURE, "Invalid SMTP authorization response"
  end
  return dovecot.auth.PASSDB_RESULT_OK, { user = username }
end

function auth_password_verify(request, password)
  local ok, result, fields = pcall(verify, request, password)
  if not ok then
    -- Never log exceptions: HTTP client errors can contain request headers.
    return dovecot.auth.PASSDB_RESULT_INTERNAL_FAILURE, "SMTP authorization unavailable"
  end
  return result, fields
end
