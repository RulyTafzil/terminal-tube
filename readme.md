Very WIP terminal app for interacting with YouTube livestream chat.

## What it is

`terminal-tube` is a local CLI/TUI that:
1. Authenticates via OAuth 2.0 (Desktop app flow).
2. Looks up a stream’s `activeLiveChatId`.
3. Polls live chat messages and renders them in a fixed terminal UI.
4. Lets you type a message and sends it to the stream’s live chat.

This project is intended for personal / creator use. For “others”, the recommended model is still to let each user bring their own Google OAuth credentials so they spend their own YouTube API quota.

## Architecture (high level)

- `terminal-tube login`
  - Opens a browser to complete OAuth once.
  - Saves a refreshable token (includes client id/secret) into a local token file.
- `terminal-tube chat`
  - Uses the stored token to call YouTube Data API v3.
  - Poll loop runs until you quit (`Ctrl+C`).

## Setup (required)

### Google Cloud

- **Enable API**: Enable **YouTube Data API v3** in Google Cloud.
- **OAuth client**: Create an **OAuth 2.0 Client ID** of type **Desktop app**.
- **Download secrets**: Save the downloaded JSON as a path you can pass to `login` (e.g. `./client_secrets.json`).

### CLI login (once per machine/project)

```bash
cargo run -- login --client-secrets /path/to/client_secrets.json
```

By default, the token is stored in your user config dir (typically `~/.config/terminal-tube/yt_token.json`). You can override the storage location with `--token-file`.

## Chat usage

```bash
cargo run -- chat https://www.youtube.com/watch?v=VIDEO_ID
```

Quit with `Ctrl+C`.

## CLI commands

### `login`

- **What it does**: Performs OAuth Desktop flow and writes a refreshable token file.
- **What it calls**: Google OAuth endpoints to obtain and refresh tokens.
- **YouTube quota impact**: This step does not use YouTube Data API v3 endpoints. It only uses OAuth token endpoints.

### `chat`

- **What it does**: Opens the TUI, polls live chat messages, and sends typed messages.
- **YouTube quota impact**: This step uses the YouTube Data API v3 endpoints described below.

## YouTube Data API v3 usage (endpoints, timing, parameters)

All YouTube calls use the OAuth access token as `Authorization: Bearer ...`.

### 1. Startup: resolve `activeLiveChatId`

Endpoint:
- `GET https://www.googleapis.com/youtube/v3/videos`

Query/params:
- `part=liveStreamingDetails,snippet`
- `id=<video_id>`

Called:
- Once per `terminal-tube chat` session (i.e. each time you start the TUI for a given run).

Purpose:
- Find the stream’s `liveStreamingDetails.activeLiveChatId`.

Quota usage model:
- This is typically a single request per chat session.

### 2. Poll loop: fetch live chat messages

Endpoint:
- `GET https://www.googleapis.com/youtube/v3/liveChat/messages`

Query/params used by this code:
- `liveChatId=<active_live_chat_id>`
- `part=snippet,authorDetails`
- `maxResults=200`
- `pageToken=<token>` (included after the first poll to request the next page of results)

Scheduling:
- The API response includes `pollingIntervalMillis`.
- The app waits approximately `pollingIntervalMillis` between polls.
- If `pollingIntervalMillis` is missing, it falls back to `5000ms`.

Called:
- Repeatedly until you quit the TUI.

First poll behavior:
- On the first successful poll, the UI only renders the last `30` messages from that response (it still comes from a single API request).

Quota usage model (approximation):
- `N_polls ≈ session_duration_seconds / polling_interval_seconds`
- Total quota for this part is proportional to `N_polls` times the quota cost per `liveChatMessages.list` request (see the next section for how to compute that).

### 3. Send message

Endpoint:
- `POST https://www.googleapis.com/youtube/v3/liveChat/messages?part=snippet`

Body:
- `type=textMessageEvent`
- `textMessageDetails.messageText=<your text>`
- `snippet.liveChatId=<active_live_chat_id>`

Called:
- Exactly once per message you submit (press Enter).

Quota usage model:
- Proportional to number of sent messages.

## How “much API usage” is consumed (quota estimation)

Google’s YouTube Data API v3 uses quota units that vary by endpoint and project configuration. To compute your real quota consumption:

1. In Google Cloud Console, open your project’s **APIs & Services → Library → YouTube Data API v3 → Quotas**.
2. Find the quota unit cost for each of these endpoints:
   - `videos.list`
   - `liveChatMessages.list`
   - `liveChatMessages.insert`
3. Use the approximate formula for a single `chat` session:

Let:
- `C_videos_list` = quota cost per `videos.list` request
- `C_livechat_list` = quota cost per `liveChatMessages.list` request
- `C_livechat_insert` = quota cost per `liveChatMessages.insert` request
- `N_list_start` = number of startup `videos.list` calls (usually `1` per session)
- `N_polls` = number of `liveChatMessages.list` polls during the session
- `N_sent` = number of messages sent by you (press Enter)

Then:
- `quota_used ≈ C_videos_list * N_list_start + C_livechat_list * N_polls + C_livechat_insert * N_sent`

Estimating `N_polls`:
- If the stream reports `pollingIntervalMillis = 5000`, then in 1 hour:
  - `N_polls ≈ 3600 / 5 = 720`
- The exact number depends on the stream’s `pollingIntervalMillis` and any errors/backoff.

## Cost control / quota reduction strategies

For this CLI, the best levers are about reducing the number of *requests*.

These changes are already aligned with the API’s intended usage:
- Polling uses `pollingIntervalMillis` (good).
- Messages use `pageToken` (avoids re-fetching the same pages).

Practical strategies for lowering quota usage:
- Run only one `chat` session per stream at a time (multiple terminals scale quota linearly).
- Use a sensible session duration (quota scales with how long you keep the TUI open).
- If you plan to message often, keep `N_sent` in mind (each message triggers an `insert`).

Potential future optimization (not implemented yet in this repo):
- Cache the `activeLiveChatId` by `video_id` for a short TTL so `videos.list` is not repeated across restarts.

## Common failure mode: `403 quotaExceeded`

If you see errors like:
- `reason: quotaExceeded`

That means your Google project’s YouTube Data API v3 quota was exhausted. The fix is:
- wait for quota reset, or
- increase quota in Google Cloud, or
- authenticate users with their own projects (recommended for releasing this to others).

## Security notes

- Tokens are stored locally on disk in a user config directory by default.
- Keep `client_secrets.json` and token files private; treat them as secrets.
