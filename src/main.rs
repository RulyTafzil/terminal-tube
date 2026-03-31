use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use std::path::PathBuf;

mod oauth;
mod tui;
mod youtube;

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

fn default_token_path() -> Result<PathBuf> {
    let proj = ProjectDirs::from("dev", "terminal-tube", "terminal-tube")
        .ok_or_else(|| anyhow!("Could not determine a per-user config directory"))?;
    Ok(proj.config_dir().join("yt_token.json"))
}

#[derive(Parser)]
#[command(name = "terminal-tube", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate once and store a refreshable token locally
    Login {
        /// Path to Google OAuth Desktop client JSON
        #[arg(long)]
        client_secrets: PathBuf,

        /// Where to store the token (defaults to user config dir)
        #[arg(long)]
        token_file: Option<PathBuf>,
    },

    /// Connect to a stream's live chat and open the TUI
    Chat {
        /// YouTube video ID or URL
        video: String,

        /// Token file location (defaults to user config dir)
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Login {
            client_secrets,
            token_file,
        } => {
            let token_path = token_file.unwrap_or(default_token_path()?);
            oauth::login_with_client_secrets(&client_secrets, &token_path, SCOPE)
                .await
                .with_context(|| format!("login and write token to {}", token_path.display()))?;
            eprintln!("Saved token to {}", token_path.display());
            Ok(())
        }
        Commands::Chat { video, token_file } => {
            let token_path = token_file.unwrap_or(default_token_path()?);
            let video_id = extract_video_id(&video)?;

            let token = oauth::get_valid_access_token(&token_path, SCOPE)
                .await
                .context("load/refresh token")?;

            let yt = youtube::YouTube::new(token);
            let (chat_id, title, channel) = yt.get_live_chat_id(&video_id).await?;
            tui::run_tui(yt, chat_id, title, channel).await?;
            Ok(())
        }
    }
}

