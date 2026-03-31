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

fn oauth_client_from_secrets(secrets_path: &Path) -> Result<BasicClient> {
    let data = std::fs::read_to_string(secrets_path)
        .with_context(|| format!("read {}", secrets_path.display()))?;
    let secrets: ClientSecretsFile =
        serde_json::from_str(&data).context("parse client_secrets.json")?;

    Ok(BasicClient::new(
        ClientId::new(secrets.installed.client_id),
        Some(ClientSecret::new(secrets.installed.client_secret)),
        AuthUrl::new(secrets.installed.auth_uri).context("invalid auth_uri")?,
        Some(TokenUrl::new(secrets.installed.token_uri).context("invalid token_uri")?),
    ))
}

pub async fn get_token_installed_app(
    client_secrets_path: &Path,
    token_path: &Path,
    scope: &str,
) -> Result<StoredToken> {
    if !client_secrets_path.exists() {
        return Err(anyhow!(
            "`{}` not found (download an OAuth Desktop client JSON from Google Cloud)",
            client_secrets_path.display()
        ));
    }

    let client = oauth_client_from_secrets(client_secrets_path)?;

    // Load token if present.
    if token_path.exists() {
        let txt = std::fs::read_to_string(token_path)
            .with_context(|| format!("read {}", token_path.display()))?;
        if let Ok(tok) = serde_json::from_str::<TokenFile>(&txt) {
            let mut tok = tok.into_stored_token();
            if let Some(rt) = tok.refresh_token.clone() {
                if tok.is_expired_soon() {
                    tok = refresh_token(&client, &rt).await?;
                    let _ = std::fs::write(
                        token_path,
                        serde_json::to_string_pretty(&tok).unwrap_or_default(),
                    );
                }
            }
            return Ok(tok);
        }
    }

    let tok = first_time_authorize(&client, scope).await?;
    std::fs::write(
        token_path,
        serde_json::to_string_pretty(&tok).context("serialize token")?,
    )
    .with_context(|| format!("write {}", token_path.display()))?;
    Ok(tok)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TokenFile {
    Stored(StoredToken),
    GoogleAuthorizedUser(GoogleAuthorizedUserToken),
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
        }
    }
}

#[derive(Debug, Deserialize)]
struct GoogleAuthorizedUserToken {
    token: String,
    refresh_token: Option<String>,
    expiry: Option<String>,
}

