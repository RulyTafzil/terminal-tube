#!/usr/bin/env python3
"""
YouTube Live Chat Terminal Client  —  Textual TUI
--------------------------------------------------
Read and send YouTube live chat messages from a fixed-height terminal UI.

Requirements:
    pip install google-auth-oauthlib google-api-python-client rich textual

Setup (one-time):
    1. Go to https://console.cloud.google.com/
    2. Create a project (or select an existing one)
    3. Enable the "YouTube Data API v3"
    4. Go to APIs & Services → Credentials → Create Credentials → OAuth 2.0 Client ID
    5. Choose "Desktop app" as the application type
    6. Download the JSON file and save it as "client_secrets.json" next to this script

Usage:
    python yt_livechat.py <VIDEO_ID_OR_URL>

Controls:
    Type a message and press Enter to send.
    Press Ctrl+C to quit.
"""

import sys
import re
import time
import argparse
from datetime import datetime
from pathlib import Path

# ── Dependency check ──────────────────────────────────────────────────────────
try:
    from rich.text import Text
    from rich.console import Console
except ImportError:
    print("Missing dependency: pip install rich")
    sys.exit(1)

try:
    from textual.app import App, ComposeResult
    from textual.widgets import Input, RichLog, Static
    from textual.binding import Binding
    from textual import work
    from textual.worker import get_current_worker
except ImportError:
    print("Missing dependency: pip install textual")
    sys.exit(1)

try:
    from google_auth_oauthlib.flow import InstalledAppFlow
    from google.auth.transport.requests import Request
    from google.oauth2.credentials import Credentials
    from googleapiclient.discovery import build
    from googleapiclient.errors import HttpError
except ImportError:
    print("Missing dependency: pip install google-auth-oauthlib google-api-python-client")
    sys.exit(1)

# ── Config ────────────────────────────────────────────────────────────────────
SCOPES              = ["https://www.googleapis.com/auth/youtube"]
CLIENT_SECRETS_FILE = "client_secrets.json"
TOKEN_FILE          = "yt_token.json"
POLL_INTERVAL_MS    = 5000   # floor; API's nextPollIntervalMs takes precedence
HISTORY_LINES       = 30     # messages shown on first load

# Rich Console used only before the TUI starts (auth + lookup output)
_console = Console()

# ── Auth ──────────────────────────────────────────────────────────────────────
def get_authenticated_service():
    if not Path(CLIENT_SECRETS_FILE).exists():
        _console.print(f"\n[bold red]Error:[/bold red] '{CLIENT_SECRETS_FILE}' not found.")
        _console.print("Follow the setup instructions at the top of this script.")
        sys.exit(1)

    creds = None
    if Path(TOKEN_FILE).exists():
        creds = Credentials.from_authorized_user_file(TOKEN_FILE, SCOPES)

    if not creds or not creds.valid:
        if creds and creds.expired and creds.refresh_token:
            creds.refresh(Request())
        else:
            flow = InstalledAppFlow.from_client_secrets_file(CLIENT_SECRETS_FILE, SCOPES)
            creds = flow.run_local_server(port=0)
        with open(TOKEN_FILE, "w") as f:
            f.write(creds.to_json())

    return build("youtube", "v3", credentials=creds)

# ── Helpers ───────────────────────────────────────────────────────────────────
def extract_video_id(input_str: str) -> str:
    patterns = [
        r"(?:v=|youtu\.be/|embed/|shorts/)([A-Za-z0-9_-]{11})",
        r"^([A-Za-z0-9_-]{11})$",
    ]
    for p in patterns:
        m = re.search(p, input_str)
        if m:
            return m.group(1)
    _console.print(f"[bold red]Could not extract a video ID from:[/bold red] {input_str}")
    sys.exit(1)


def get_live_chat_id(youtube, video_id: str) -> tuple[str, str, str]:
    try:
        resp = youtube.videos().list(
            part="liveStreamingDetails,snippet",
            id=video_id,
        ).execute()
    except HttpError as e:
        _console.print(f"[bold red]API Error:[/bold red] {e}")
        sys.exit(1)

    if not resp.get("items"):
        _console.print(f"[bold red]Video not found:[/bold red] {video_id}")
        sys.exit(1)

    item    = resp["items"][0]
    details = item.get("liveStreamingDetails", {})
    chat_id = details.get("activeLiveChatId")

    if not chat_id:
        _console.print("[bold red]No active live chat found.[/bold red] Is the stream live?")
        sys.exit(1)

    return chat_id, item["snippet"]["title"], item["snippet"]["channelTitle"]


def send_message(youtube, chat_id: str, text: str) -> tuple[bool, str]:
    """Returns (success, error_message)."""
    try:
        youtube.liveChatMessages().insert(
            part="snippet",
            body={
                "snippet": {
                    "liveChatId": chat_id,
                    "type": "textMessageEvent",
                    "textMessageDetails": {"messageText": text},
                }
            },
        ).execute()
        return True, ""
    except HttpError as e:
        return False, str(e)


def format_timestamp(iso_str: str) -> str:
    try:
        dt = datetime.fromisoformat(iso_str.replace("Z", "+00:00"))
        return dt.strftime("%H:%M:%S")
    except Exception:
        return "??:??:??"


def build_message_line(item: dict) -> Text:
    """
    Build a Rich Text object for one chat message.
    All appends use style= (plain text), never markup, so brackets in
    timestamps or usernames are never misinterpreted as Rich markup tags.
    """
    snippet  = item.get("snippet", {})
    author   = item.get("authorDetails", {})

    ts       = format_timestamp(snippet.get("publishedAt", ""))
    name     = author.get("displayName", "Unknown")
    body     = snippet.get("displayMessage", "")
    msg_type = snippet.get("type", "textMessageEvent")

    is_owner  = bool(author.get("isChatOwner") or author.get("isVerified"))
    is_mod    = bool(author.get("isChatModerator"))
    is_member = bool(author.get("isChatSponsor"))

    BADGES: dict[str, tuple[str, str]] = {
        "superChatEvent":              ("💰 SUPERCHAT", "bold yellow"),
        "superStickerEvent":           ("🎉 STICKER",   "bold magenta"),
        "memberMilestoneChatEvent":    ("⭐ MEMBER",    "bold green"),
        "newSponsorEvent":             ("🌟 NEW MEMBER","bold green"),
        "membershipGiftingEvent":      ("🎁 GIFTING",   "bold blue"),
        "giftMembershipReceivedEvent": ("🎁 GIFT",      "bold blue"),
    }

    line = Text(no_wrap=False)

    # # Timestamp — Text.append() with style= is always plain text, never markup
    # line.append(f"[{ts}] ", style="dim")

    # Optional event badge
    badge_label, badge_style = BADGES.get(msg_type, ("", ""))
    if badge_label:
        line.append(f"{badge_label} ", style=badge_style)

    # Role icon
    if is_owner:
        line.append("👑 ", style="bold yellow")
    elif is_mod:
        line.append("🔧 ", style="bold cyan")
    elif is_member:
        line.append("⭐ ", style="bold blue")

    # Username — colour by role
    if is_owner:
        name_style = "bold yellow"
    elif is_mod:
        name_style = "bold cyan"
    elif is_member:
        name_style = "bold blue"     # subscribers / members → green
    else:
        name_style = "bold white"

    line.append(name, style=name_style)
    line.append(": ", style="dim")
    line.append(body)

    # SuperChat amount suffix
    if msg_type == "superChatEvent":
        amount = snippet.get("superChatDetails", {}).get("amountDisplayString", "")
        if amount:
            line.append(f"  [{amount}]", style="bold yellow")

    return line


# ── Textual TUI ───────────────────────────────────────────────────────────────
class YTLiveChatApp(App[None]):
    """Fixed-height YouTube live chat TUI built with Textual."""

    CSS = """
    Screen {
        overflow: hidden hidden;
        layers: base;
    }

    /* ── Title bar ─────────────────────────────────────── */
    #title-bar {
        height: 1;
        background: #cc0000;
        color: white;
        text-style: bold;
        padding: 0 2;
    }

    /* ── Scrollable chat area ──────────────────────────── */
    #chat-log {
        height: 1fr;
        background: $background;
        padding: 0 1;
        border: none;
        scrollbar-gutter: stable;
    }

    /* ── Status bar ────────────────────────────────────── */
    #status-bar {
        height: 1;
        background: $surface;
        color: gray;
        padding: 0 2;
    }

    /* ── Input ─────────────────────────────────────────── */
    #msg-input {
        height: 3;
        background: $surface;
        border: tall $panel;
        margin: 0;
        padding: 0 1;
    }

    #msg-input:focus {
        border: $secondary;
    }
    """

    BINDINGS = [
        Binding("ctrl+c", "quit", "Quit", priority=True, show=True),
    ]

    def __init__(
        self,
        youtube,
        chat_id: str,
        video_title: str,
        channel: str,
    ) -> None:
        super().__init__()
        self.youtube     = youtube
        self.chat_id     = chat_id
        self.video_title = video_title
        self.channel     = channel

    # ── Layout ────────────────────────────────────────────────────────────────
    def compose(self) -> ComposeResult:
        short = self.video_title[:65] + ("…" if len(self.video_title) > 65 else "")
        yield Static(f" ▶  {short}   ·  {self.channel}", id="title-bar")
        yield RichLog(id="chat-log", wrap=True, highlight=False, markup=False, max_lines=1000)
        yield Static(" Connecting…", id="status-bar")
        yield Input(
            placeholder="💬  Type a message and press Enter to send…",
            id="msg-input",
        )

    def on_mount(self) -> None:
        self.query_one("#msg-input", Input).focus()
        self._poll_loop()   # kicks off the background worker

    # ── Thread-safe UI helpers (always run on the main Textual thread) ────────
    def _write(self, line: Text) -> None:
        self.query_one("#chat-log", RichLog).write(line)

    def _set_status(self, text: str) -> None:
        self.query_one("#status-bar", Static).update(f" {text}")

    # ── Background polling worker ─────────────────────────────────────────────
    @work(thread=True)
    def _poll_loop(self) -> None:
        """
        Polls the YouTube live chat API and pushes new Rich Text lines into
        the RichLog via call_from_thread.  This runs in a Textual-managed
        worker thread and is cancelled automatically on app exit.
        """
        worker     = get_current_worker()
        page_token: str | None = None
        next_poll  = 0.0
        is_first   = True
        error_backoff = 10.0

        while not worker.is_cancelled:
            if time.monotonic() < next_poll:
                time.sleep(0.2)
                continue

            try:
                kwargs: dict = dict(
                    liveChatId=self.chat_id,
                    part="snippet,authorDetails",
                    maxResults=200,
                )
                if page_token:
                    kwargs["pageToken"] = page_token

                resp       = self.youtube.liveChatMessages().list(**kwargs).execute()
                items      = resp.get("items", [])
                poll_ms    = resp.get("pollingIntervalMillis", POLL_INTERVAL_MS)
                page_token = resp.get("nextPageToken")
                next_poll  = time.monotonic() + poll_ms / 1000

                # First fetch → recent history only; subsequent → all new items
                to_show = items[-HISTORY_LINES:] if is_first else items
                for item in to_show:
                    self.call_from_thread(self._write, build_message_line(item))

                if is_first:
                    sep = Text(
                        f"{'─' * 18}  history loaded  {'─' * 18}",
                        style="dim",
                    )
                    self.call_from_thread(self._write, sep)
                    is_first = False

                self.call_from_thread(
                    self._set_status,
                    f"Connected  ·  next poll in {poll_ms // 1000}s  ·  Ctrl+C to quit",
                )
                error_backoff = 10.0

            except HttpError as e:
                self.call_from_thread(
                    self._write, Text(f"API error: {e}", style="bold red")
                )
                self.call_from_thread(self._set_status, "API error — retrying in 10s…")
                next_poll = time.monotonic() + 10

            except Exception as e:
                self.call_from_thread(self._set_status, f"error — retrying in {int(error_backoff)}s…")
                next_poll = time.monotonic() + error_backoff
                # double the backoff, capped at 60 seconds
                error_backoff = min(error_backoff * 2, 60.0)

    # ── Send on Enter ─────────────────────────────────────────────────────────
    # Change your on_input_submitted to trigger a worker:
    def on_input_submitted(self, event: Input.Submitted) -> None:
        msg = event.value.strip()
        event.input.clear()
        if not msg:
            return
        
        # Render optimistically right away
        #self._render_optimistic_message(msg)
        # Fire and forget the background sender
        self._send_message_worker(msg)

    @work(thread=True)
    def _send_message_worker(self, msg: str) -> None:
        ok, err = send_message(self.youtube, self.chat_id, msg)
        if not ok:
            self.call_from_thread(
                self._write, Text(f"Send failed: {err}", style="bold red")
            )
# ── Entry point ───────────────────────────────────────────────────────────────
def main() -> None:
    parser = argparse.ArgumentParser(description="YouTube Live Chat TUI")
    parser.add_argument("video", help="YouTube video ID or URL")
    args = parser.parse_args()

    video_id = extract_video_id(args.video)

    _console.print("\n[bold]🔐 Authenticating with YouTube…[/bold]")
    youtube = get_authenticated_service()

    _console.print(f"[bold]🔍 Looking up live chat for [cyan]{video_id}[/cyan]…[/bold]")
    chat_id, video_title, channel = get_live_chat_id(youtube, video_id)
    _console.print(
        f"[bold green]✓ Connected![/bold green]  "
        f"[cyan]{video_title}[/cyan]  by [yellow]{channel}[/yellow]"
    )
    time.sleep(0.5)   # let the user read the confirmation before TUI takes over

    YTLiveChatApp(youtube, chat_id, video_title, channel).run()


if __name__ == "__main__":
    main()
