use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;
use url::Url;

#[derive(Debug, Deserialize)]
struct ClientSecretsFile {
    installed: InstalledClient,
}

#[derive(Debug, Deserialize)]
struct InstalledClient {
    client_id: String,
    client_secret: String,
    auth_uri: String,
    token_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_utc: Option<DateTime<Utc>>,
}

impl StoredToken {
    fn is_expired_soon(&self) -> bool {
        let Some(expires_at) = self.expires_at_utc else {
            return false;
        };
        // Refresh a bit early to avoid mid-request expiry.
        expires_at <= Utc::now() + chrono::Duration::seconds(60)
    }
}

async fn refresh_token(
    client: &BasicClient,
    refresh_token: &str,
) -> Result<StoredToken> {
    let token = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(async_http_client)
        .await
        .context("refresh access token")?;

    let expires_at_utc = token
        .expires_in()
        .map(|d| Utc::now() + chrono::Duration::from_std(d).unwrap_or_default());

    Ok(StoredToken {
        access_token: token.access_token().secret().to_string(),
        refresh_token: token.refresh_token().map(|t| t.secret().to_string()).or_else(|| {
            Some(refresh_token.to_string())
        }),
        expires_at_utc,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizedUserTokenFile {
    pub token: String,
    pub refresh_token: Option<String>,
    pub token_uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    pub expiry: Option<String>,
}

impl AuthorizedUserTokenFile {
    fn to_stored_token(&self) -> StoredToken {
        StoredToken {
            access_token: self.token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at_utc: self
                .expiry
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
        }
    }

    fn oauth_client(&self) -> Result<BasicClient> {
        Ok(BasicClient::new(
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
                .context("invalid auth url")?,
            Some(TokenUrl::new(self.token_uri.clone()).context("invalid token url")?),
        ))
    }
}

async fn first_time_authorize(
    client: &BasicClient,
    scope: &str,
) -> Result<StoredToken> {
    // Bind to an ephemeral port on localhost for the OAuth redirect.
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind localhost port")?;
    listener
        .set_nonblocking(true)
        .context("set nonblocking listener")?;
    let port = listener.local_addr().context("get listener addr")?.port();

    let redirect = format!("http://127.0.0.1:{port}/oauth2/callback");
    let client = client
        .clone()
        .set_redirect_uri(
            RedirectUrl::new(redirect.clone()).context("invalid redirect url")?,
        );

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, _csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(scope.to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    // Open browser, fall back to printing if it fails.
    if open::that(auth_url.as_str()).is_err() {
        eprintln!("Open this URL in your browser:\n{auth_url}");
    }

    // Minimal HTTP accept loop to capture `code` from the redirect.
    let code = wait_for_auth_code(&listener, Duration::from_secs(180))
        .context("waiting for OAuth redirect")?;

    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(async_http_client)
        .await
        .context("exchange code for token")?;

    let expires_at_utc = token
        .expires_in()
        .map(|d| Utc::now() + chrono::Duration::from_std(d).unwrap_or_default());

    Ok(StoredToken {
        access_token: token.access_token().secret().to_string(),
        refresh_token: token.refresh_token().map(|t| t.secret().to_string()),
        expires_at_utc,
    })
}

fn wait_for_auth_code(listener: &TcpListener, timeout: Duration) -> Result<String> {
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(anyhow!("Timed out waiting for OAuth redirect"));
        }

        match listener.accept() {
            Ok((mut stream, _addr)) => {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                let first_line = req.lines().next().unwrap_or("");
                // Example: GET /oauth2/callback?code=...&scope=... HTTP/1.1
                let path = first_line
                    .split_whitespace()
                    .nth(1)
                    .ok_or_else(|| anyhow!("malformed HTTP request"))?;

                let url = Url::parse(&format!("http://localhost{path}"))
                    .context("parse redirect URL")?;
                let code = url
                    .query_pairs()
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.to_string());

                let body = if code.is_some() {
                    "Authentication complete. You can close this tab and return to the terminal."
                } else {
                    "Missing `code` parameter. You can close this tab."
                };

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());

                if let Some(code) = code {
                    return Ok(code);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e).context("accept redirect connection"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TokenFile {
    Stored(StoredToken),
    GoogleAuthorizedUser(GoogleAuthorizedUserToken),
    AuthorizedUser(AuthorizedUserTokenFile),
}

impl TokenFile {
    fn into_stored_token(self) -> StoredToken {
        match self {
            TokenFile::Stored(t) => t,
            TokenFile::GoogleAuthorizedUser(g) => StoredToken {
                access_token: g.token,
                refresh_token: g.refresh_token,
                expires_at_utc: g
                    .expiry
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc)),
            },
            TokenFile::AuthorizedUser(a) => a.to_stored_token(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GoogleAuthorizedUserToken {
    token: String,
    refresh_token: Option<String>,
    expiry: Option<String>,
}

fn read_token_file(token_path: &Path) -> Result<TokenFile> {
    let txt = std::fs::read_to_string(token_path)
        .with_context(|| format!("read {}", token_path.display()))?;
    serde_json::from_str::<TokenFile>(&txt).context("parse token file json")
}

fn write_authorized_user_file(token_path: &Path, file: &AuthorizedUserTokenFile) -> Result<()> {
    if let Some(parent) = token_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(
        token_path,
        serde_json::to_string_pretty(file).context("serialize token file")?,
    )
    .with_context(|| format!("write {}", token_path.display()))?;
    Ok(())
}

pub async fn login_with_client_secrets(
    client_secrets_path: &Path,
    token_path: &Path,
    scope: &str,
) -> Result<()> {
    if !client_secrets_path.exists() {
        return Err(anyhow!(
            "`{}` not found (download an OAuth Desktop client JSON from Google Cloud)",
            client_secrets_path.display()
        ));
    }

    let data = std::fs::read_to_string(client_secrets_path)
        .with_context(|| format!("read {}", client_secrets_path.display()))?;
    let secrets: ClientSecretsFile =
        serde_json::from_str(&data).context("parse client_secrets.json")?;

    let base_client = BasicClient::new(
        ClientId::new(secrets.installed.client_id.clone()),
        Some(ClientSecret::new(secrets.installed.client_secret.clone())),
        AuthUrl::new(secrets.installed.auth_uri).context("invalid auth_uri")?,
        Some(TokenUrl::new(secrets.installed.token_uri.clone()).context("invalid token_uri")?),
    );

    let tok = first_time_authorize(&base_client, scope).await?;

    let expiry = tok
        .expires_at_utc
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let file = AuthorizedUserTokenFile {
        token: tok.access_token,
        refresh_token: tok.refresh_token,
        token_uri: secrets.installed.token_uri,
        client_id: secrets.installed.client_id,
        client_secret: secrets.installed.client_secret,
        scopes: vec![scope.to_string()],
        expiry,
    };

    write_authorized_user_file(token_path, &file)?;
    Ok(())
}

pub async fn get_valid_access_token(
    token_path: &Path,
    scope: &str,
) -> Result<StoredToken> {
    if !token_path.exists() {
        return Err(anyhow!(
            "No token found at `{}`. Run `terminal-tube login ...` first.",
            token_path.display()
        ));
    }

    let tf = read_token_file(token_path)?;
    match tf {
        TokenFile::AuthorizedUser(mut f) => {
            // Basic sanity: scope match (we only store one scope currently).
            if !f.scopes.iter().any(|s| s == scope) {
                return Err(anyhow!(
                    "Token file scopes do not include required scope `{}`. Re-run login.",
                    scope
                ));
            }

            let mut tok = f.to_stored_token();
            if tok.is_expired_soon() {
                let rt = tok
                    .refresh_token
                    .clone()
                    .ok_or_else(|| anyhow!("Token has no refresh_token; re-run login"))?;
                let client = f.oauth_client()?;
                let new_tok = refresh_token(&client, &rt).await?;
                f.token = new_tok.access_token.clone();
                f.refresh_token = new_tok.refresh_token.clone();
                f.expiry = new_tok
                    .expires_at_utc
                    .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                write_authorized_user_file(token_path, &f)?;
                tok = new_tok;
            }
            Ok(tok)
        }
        // Back-compat: old formats can be used, but cannot be refreshed without secrets.
        other => Ok(other.into_stored_token()),
    }
}

