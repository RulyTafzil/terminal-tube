use anyhow::{anyhow, Context, Result};

mod oauth;
mod tui;
mod youtube;

const CLIENT_SECRETS_FILE: &str = "client_secrets.json";
const TOKEN_FILE: &str = "yt_token.json";
const SCOPE: &str = "https://www.googleapis.com/auth/youtube";

fn extract_video_id(input: &str) -> Result<String> {
    // Matches typical watch URLs, shorts, embed, youtu.be, or raw 11-char id.
    let patterns: [&str; 2] = [
        r"(?:v=|youtu\.be/|embed/|shorts/)([A-Za-z0-9_-]{11})",
        r"^([A-Za-z0-9_-]{11})$",
    ];

    for pat in patterns {
        let re = regex::Regex::new(pat).context("compile video id regex")?;
        if let Some(caps) = re.captures(input) {
            return Ok(caps
                .get(1)
                .ok_or_else(|| anyhow!("video id capture missing"))?
                .as_str()
                .to_string());
        }
    }

    Err(anyhow!("Could not extract a video ID from: {input}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let video_arg = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("Usage: terminal-tube <VIDEO_ID_OR_URL>"))?;

    let video_id = extract_video_id(&video_arg)?;

    let cwd = std::env::current_dir().context("get current dir")?;
    let client_secrets_path = cwd.join(CLIENT_SECRETS_FILE);
    let token_path = cwd.join(TOKEN_FILE);

    let token = oauth::get_token_installed_app(
        &client_secrets_path,
        &token_path,
        SCOPE,
    )
    .await
    .context("authenticate with YouTube")?;

    let yt = youtube::YouTube::new(token);
    let (chat_id, title, channel) = yt.get_live_chat_id(&video_id).await?;

    tui::run_tui(yt, chat_id, title, channel).await?;
    Ok(())
}

