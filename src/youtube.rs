use crate::oauth::StoredToken;
use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct YouTube {
    http: reqwest::Client,
    token: std::sync::Arc<Mutex<StoredToken>>,
}

impl YouTube {
    pub fn new(token: StoredToken) -> Self {
        Self {
            http: reqwest::Client::new(),
            token: std::sync::Arc::new(Mutex::new(token)),
        }
    }

    async fn auth_headers(&self) -> Result<HeaderMap> {
        let tok = self.token.lock().await;
        let mut headers = HeaderMap::new();
        let hv = HeaderValue::from_str(&format!("Bearer {}", tok.access_token))
            .context("build Authorization header")?;
        headers.insert(AUTHORIZATION, hv);
        Ok(headers)
    }

    async fn json_or_error<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
        context_label: &'static str,
    ) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<T>()
                .await
                .with_context(|| format!("parse {context_label} response json"));
        }

        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());

        Err(anyhow!(
            "{context_label} failed: HTTP {} body: {}",
            status.as_u16(),
            body
        ))
    }

    pub async fn get_live_chat_id(&self, video_id: &str) -> Result<(String, String, String)> {
        let url = "https://www.googleapis.com/youtube/v3/videos";
        let headers = self.auth_headers().await?;

        let resp = self
            .http
            .get(url)
            .headers(headers)
            .query(&[
                ("part", "liveStreamingDetails,snippet"),
                ("id", video_id),
            ])
            .send()
            .await
            .context("videos.list request")?;

        let resp: VideosListResponse = self.json_or_error(resp, "videos.list").await?;

        let item = resp.items.into_iter().next().ok_or_else(|| {
            anyhow!("Video not found: {video_id}")
        })?;

        let chat_id = item
            .live_streaming_details
            .and_then(|d| d.active_live_chat_id)
            .ok_or_else(|| anyhow!("No active live chat found. Is the stream live?"))?;

        Ok((
            chat_id,
            item.snippet.title,
            item.snippet.channel_title,
        ))
    }

    pub async fn list_messages(
        &self,
        live_chat_id: &str,
        page_token: Option<&str>,
    ) -> Result<LiveChatListResponse> {
        let url = "https://www.googleapis.com/youtube/v3/liveChat/messages";
        let headers = self.auth_headers().await?;

        let mut q: Vec<(&str, String)> = vec![
            ("liveChatId", live_chat_id.to_string()),
            ("part", "snippet,authorDetails".to_string()),
            ("maxResults", "200".to_string()),
        ];
        if let Some(pt) = page_token {
            q.push(("pageToken", pt.to_string()));
        }

        let resp = self
            .http
            .get(url)
            .headers(headers)
            .query(&q)
            .send()
            .await
            .context("liveChatMessages.list request")?;

        let resp: LiveChatListResponse = self
            .json_or_error(resp, "liveChatMessages.list")
            .await?;

        Ok(resp)
    }

    pub async fn send_message(&self, live_chat_id: &str, text: &str) -> Result<()> {
        let url = "https://www.googleapis.com/youtube/v3/liveChat/messages?part=snippet";
        let headers = self.auth_headers().await?;

        let body = SendMessageRequest {
            snippet: SendMessageSnippet {
                live_chat_id: live_chat_id.to_string(),
                kind_type: "textMessageEvent".to_string(),
                text_message_details: TextMessageDetails {
                    message_text: text.to_string(),
                },
            },
        };

        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .context("liveChatMessages.insert request")?;

        let _ignored: serde_json::Value = self
            .json_or_error(resp, "liveChatMessages.insert")
            .await?;

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct VideosListResponse {
    #[serde(default)]
    items: Vec<VideoItem>,
}

#[derive(Debug, Deserialize)]
struct VideoItem {
    snippet: VideoSnippet,
    #[serde(rename = "liveStreamingDetails")]
    live_streaming_details: Option<LiveStreamingDetails>,
}

#[derive(Debug, Deserialize)]
struct VideoSnippet {
    title: String,
    #[serde(rename = "channelTitle")]
    channel_title: String,
}

#[derive(Debug, Deserialize)]
struct LiveStreamingDetails {
    #[serde(rename = "activeLiveChatId")]
    active_live_chat_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveChatListResponse {
    #[serde(rename = "nextPageToken")]
    pub next_page_token: Option<String>,
    #[serde(rename = "pollingIntervalMillis")]
    pub polling_interval_millis: Option<u64>,
    #[serde(default)]
    pub items: Vec<LiveChatMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveChatMessage {
    pub snippet: LiveChatSnippet,
    #[serde(rename = "authorDetails")]
    pub author_details: LiveChatAuthorDetails,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveChatSnippet {
    #[serde(rename = "displayMessage")]
    pub display_message: Option<String>,
    #[serde(rename = "type")]
    pub message_type: Option<String>,
    #[serde(rename = "superChatDetails")]
    pub super_chat_details: Option<SuperChatDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SuperChatDetails {
    #[serde(rename = "amountDisplayString")]
    pub amount_display_string: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveChatAuthorDetails {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "isChatOwner")]
    pub is_chat_owner: Option<bool>,
    #[serde(rename = "isVerified")]
    pub is_verified: Option<bool>,
    #[serde(rename = "isChatModerator")]
    pub is_chat_moderator: Option<bool>,
    #[serde(rename = "isChatSponsor")]
    pub is_chat_sponsor: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
struct SendMessageRequest {
    snippet: SendMessageSnippet,
}

#[derive(Debug, serde::Serialize)]
struct SendMessageSnippet {
    #[serde(rename = "liveChatId")]
    live_chat_id: String,
    #[serde(rename = "type")]
    kind_type: String,
    #[serde(rename = "textMessageDetails")]
    text_message_details: TextMessageDetails,
}

#[derive(Debug, serde::Serialize)]
struct TextMessageDetails {
    #[serde(rename = "messageText")]
    message_text: String,
}

