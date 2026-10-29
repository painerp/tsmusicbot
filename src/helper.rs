use crate::{Action, Config, InfoJson, PlaybackState};
use anyhow::{Context, Result};
use axum::extract::State;
use axum::{routing::get, Json, Router};
use chrono::Utc;
use log::{error, info};
use serde_json::json;
use socketioxide::{extract::SocketRef, SocketIo};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::sync::Mutex;
use tsclientlib::{ClientId, Connection, Identity, MessageHandle, MessageTarget, OutCommandExt};
use which::which;

pub fn check_dependencies() -> () {
    if which("ffmpeg").is_err() {
        panic!("Unable to find ffmpeg");
    };

    if which("yt-dlp").is_err() {
        panic!("Unable to find yt-dlp");
    };
}

pub fn read_config(config_file_path: &str) -> Config {
    let config_file = match File::open(config_file_path) {
        Ok(id) => id,
        Err(why) => {
            panic!("Unable to open configuration file: {}", why);
        }
    };

    match serde_json::from_reader(config_file) {
        Ok(cfg) => cfg,
        Err(why) => {
            panic!("Failed to parse config: {}", why);
        }
    }
}

pub fn connect_to_ts(config: Config) -> Connection {
    let con_config = Connection::build(config.host)
        .name(config.name)
        .password(config.password)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false);

    let id = match Identity::new_from_str(&config.id) {
        Ok(id) => id,
        Err(why) => {
            panic!("Invalid teamspeak3 identity string: {}", why);
        }
    };

    let con_config = con_config.identity(id);

    match con_config.connect() {
        Ok(con) => con,
        Err(why) => {
            panic!("Unable to connect: {}", why);
        }
    }
}

pub fn read_info_json() -> Result<InfoJson> {
    let file = File::open("-.info.json").with_context(|| "Failed to open the file: -.info.json")?;

    let reader = BufReader::new(file);

    let info_json: InfoJson = serde_json::from_reader(reader)
        .with_context(|| "Failed to parse the JSON file: -.info.json")?;

    Ok(info_json)
}

pub fn is_link(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn ytdlp_args(link: &str, cookies: bool) -> Vec<&str> {
    let mut args = vec![
        "--quiet",
        "--extract-audio",
        "--audio-format",
        "opus",
        "--audio-quality",
        "48K",
        "--buffer-size",
        "16M",
        "--socket-timeout",
        "5",
        "--write-info-json",
        "--no-playlist",
        // what isn't a link is a search
        "--default-search",
        "ytsearch",
        "--output",
        "-",
    ];

    if cookies {
        args.push("--cookies");
        args.push("cookies.txt");
    }
    args.push("--");
    args.push(link);
    args
}

// First match wins, so the specific YouTube reasons come before the generic ones.
const YTDLP_ERRORS: &[(&[&str], &str)] = &[
    (&["HTTP Error 429", "Too Many Requests"], "Rate limited, try again later"),
    (&["not a bot"], "Blocked by YouTube's bot check"),
    (&["your country", "geo restriction", "from your location"], "Geoblocked"),
    (&["confirm your age", "age-restricted", "inappropriate for some users"], "Age-restricted"),
    (&["Private video"], "Private video"),
    (&["members-only"], "Members-only"),
    (&["live event will begin", "Premieres in", "Premiere will begin"], "Livestream hasn't started yet"),
    (&["copyright"], "Blocked for copyright"),
    (&["DRM protected"], "DRM protected"),
    (&["delivered no audio"], "No playable audio"),
    (&["couldn't spawn"], "The player failed to start"),
    (&["HTTP Error 403"], "Download refused (HTTP 403)"),
    (&["is not a valid URL", "Unsupported URL"], "Not a playable link"),
    (&["unavailable", "removed", "terminated", "no longer available", "HTTP Error 404"], "Unavailable"),
    (
        &["timed out", "Connection refused", "resolve", "Name or service not known", "unreachable"],
        "Network error",
    ),
];

// A short reason for yt-dlp's ERROR line that's fine to show in chat.
pub fn describe_ytdlp_error(error: &str) -> &'static str {
    YTDLP_ERRORS
        .iter()
        .find(|(needles, _)| needles.iter().any(|needle| error.contains(needle)))
        .map(|(_, reason)| *reason)
        .unwrap_or("Unknown error")
}

pub fn failure_message(name: &str, played: bool, error: &str, detailed: bool) -> String {
    let reason = describe_ytdlp_error(error);
    let mut msg = if played {
        format!("Stopped playing {} ({})", name, reason)
    } else {
        format!("Can't play {} currently ({})", name, reason)
    };
    if detailed {
        msg += &format!("\n{}", error);
    }
    msg
}

pub fn now_playing_message(link: &str, info: Option<&InfoJson>) -> String {
    let info = match info {
        Some(info) => info,
        None => return format!("Now playing: {}", link),
    };
    let mut msg = format!("Now playing: {}", info.title);
    if !info.channel.is_empty() {
        msg += &format!(" by {}", info.channel);
    }
    if let Some(duration) = info.duration {
        msg += &format!(" ({})", format_duration(duration));
    }
    msg
}

pub fn format_duration(seconds: f64) -> String {
    let s = seconds as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub async fn cleanup_process(process: &mut tokio::process::Child, name: &str) -> () {
    if let Err(e) = process.start_kill() {
        error!("Failed to kill {}: {}", name, e);
    }
    match process.wait().await {
        Ok(status) => {
            if !status.success() && !status.code().is_none() {
                error!("{} exited with non-zero status: {:?}", name, status.code());
            }
        }
        Err(e) => error!("Failed to wait on {}: {}", name, e),
    }
}

// The server answers later with a StreamItem::MessageResult for the returned handle.
pub fn send_ts_message(con: &mut Connection, target: MessageTarget, msg: &str) -> Option<MessageHandle> {
    let state = con.get_state().unwrap_or_else(|e| {
        panic!("Unable to get state: {}", e);
    });

    match state.send_message(target, &msg).send_with_result(con) {
        Ok(handle) => Some(handle),
        Err(e) => {
            error!("Message sending error: {}", e);
            None
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            c.is_alphanumeric()
                || [
                    ' ', '.', ' ', '=', '\t', ',', '?', '!', ':', '&', '/', '-', '_',
                ]
                .contains(c)
        })
        .collect()
}

pub fn parse_command(msg: &str, user_id: ClientId) -> Action {
    let stripped = msg.replace("[URL]", "").replace("[/URL]", "");
    let sanitized = sanitize(&stripped).trim().to_string();

    if !sanitized.starts_with('!') {
        return Action::None;
    }

    let split_vec: Vec<&str> = sanitized.split_whitespace().collect();
    // a link plays as is; anything else is a YouTube search for the whole rest of the message
    let target = match split_vec.get(1) {
        Some(first) if is_link(first) => first.to_string(),
        _ => split_vec.get(1..).unwrap_or_default().join(" "),
    };

    if split_vec[0] == "!stop" {
        info!("Stopping all tracks (requested by {})", user_id);
        return Action::Stop(user_id);
    }

    if split_vec[0] == "!pause" || split_vec[0] == "!p" {
        return Action::Pause(user_id);
    }

    if split_vec[0] == "!continue"
        || split_vec[0] == "!c"
        || split_vec[0] == "!resume"
        || split_vec[0] == "!r"
    {
        return Action::Resume(user_id);
    }

    if split_vec[0] == "!next" || split_vec[0] == "!n" {
        if split_vec.len() > 1 {
            info!("Queueing: {} (requested by {})", target, user_id);
            return Action::QueueNextAudio(target, user_id);
        }
        info!("Skipping (requested by {})", user_id);
        return Action::Skip(user_id);
    }

    if split_vec[0] == "!skip" || split_vec[0] == "!s" {
        info!("Skipping (requested by {})", user_id);
        return Action::Skip(user_id);
    }

    if split_vec[0] == "!help" || split_vec[0] == "!h" {
        return Action::Help(user_id);
    }

    if split_vec[0] == "!info" || split_vec[0] == "!i" {
        return Action::Info(user_id);
    }

    if split_vec[0] == "!quit" || split_vec[0] == "!q" {
        info!("Quitting (requested by {})", user_id);
        return Action::Quit;
    }

    if split_vec[0] == "!volume" || split_vec[0] == "!v" {
        return if split_vec.len() < 2 {
            Action::ChangeVolume {
                modifier: -1.0,
                user_id,
            }
        } else {
            info!(
                "Changing volume to {} (requested by {})",
                split_vec[1], user_id
            );
            let amount = split_vec[1].parse::<u32>();
            match amount {
                Err(_) => Action::None,
                Ok(num) => {
                    let modifier: f32 = num.max(0).min(100) as f32 / 100_f32;
                    Action::ChangeVolume { modifier, user_id }
                }
            }
        };
    }

    // return if no second argument
    if split_vec.len() < 2 {
        return Action::None;
    }

    if split_vec[0] == "!yt" || split_vec[0] == "!play" {
        info!("Playing: {} (requested by {})", target, user_id);
        return Action::PlayAudio(target, user_id);
    }

    Action::None
}

pub async fn get_status(State(state): State<Arc<Mutex<PlaybackState>>>) -> Json<serde_json::Value> {
    Json(status_json(&*state.lock().await))
}

pub fn status_json(state: &PlaybackState) -> serde_json::Value {
    json!({
        "time": state.position(),
        "timestamp": Utc::now().to_rfc3339(),
        "paused": state.paused,
        "loading": state.loading,
        "duration": state.duration,
        "link": state.link.clone().unwrap_or_default(),
        "entry_id": state.entry_id,
        "queued_by": state.queued_by,
    })
}

pub fn queue_json(state: &PlaybackState) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = state
        .queue
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "link": entry.link,
                "queued_by": entry.queued_by,
                "queued_at": entry.queued_at.to_rfc3339(),
            })
        })
        .collect();
    json!({ "entries": entries, "total": state.queue_total })
}

pub fn app(playback_state: Arc<Mutex<PlaybackState>>) -> Router {
    let (layer, io) = SocketIo::new_layer();
    let socket_state = Arc::clone(&playback_state);
    io.ns("/", |s: SocketRef| async move {
        info!("Client connected to Socket");
        s.on_disconnect(|| async {
            info!("Client disconnected");
        });

        let changed = Arc::clone(&socket_state.lock().await.changed);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            // the first tick is immediate; the loop sends the first status itself
            interval.tick().await;
            let mut sent_queue_version = None;
            loop {
                // armed before reading the state, so a change during the emit still wakes it
                let notified = changed.notified();
                if !s.connected() {
                    break;
                }
                // one snapshot for both, and the queue only after this status went out
                let state = socket_state.lock().await.clone();
                if s.emit("status", &status_json(&state).to_string()).is_ok()
                    && sent_queue_version != Some(state.queue_version)
                    && s.emit("queue", &queue_json(&state).to_string()).is_ok()
                {
                    sent_queue_version = Some(state.queue_version);
                }
                // every second, and right away when the song, queue, loading or paused changes
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = notified => {}
                }
            }
        });
    });

    Router::new()
        .route("/", get(|| async { "TSMusicbot is running!" }))
        .route("/status", get(move || get_status(State(playback_state))))
        .layer(layer)
}

pub async fn resolve_host(host: &str) -> Result<String> {
    match lookup_host((host, 0)).await {
        Ok(addresses) => {
            if let Some(addr) = addresses.filter(|addr| addr.ip().is_ipv4()).next() {
                info!("Resolved host: {} to IP: {}", host, addr.ip());
                return Ok(addr.ip().to_string());
            }
        }
        Err(e) => {
            error!(
                "DNS resolution error for host: {}. Reason: {}. Using hostname as fallback.",
                host, e
            );
        }
    }

    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Song;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::future::IntoFuture;
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const U: ClientId = ClientId(1);
    const VIDEO: &str = include_str!("../tests/fixtures/youtube_video.info.json");
    const LIVESTREAM: &str = include_str!("../tests/fixtures/youtube_livestream.info.json");
    const DIRECT_LINK: &str = include_str!("../tests/fixtures/direct_link.info.json");

    #[test]
    fn parse_command_ignores_non_commands_without_panicking() {
        // "" is what a poke without text sends; "^^" sanitizes to ""; "über" starts with a multi-byte char
        for msg in ["", "^^", ";)", "über", "😀", "…", "hello", "!", "!unknown"] {
            assert_eq!(parse_command(msg, U), Action::None, "{:?}", msg);
        }
    }

    #[test]
    fn parse_command_maps_every_alias() {
        let link = || "https://youtu.be/dQw4w9WgXcQ".to_string();
        let cases = vec![
            ("!play https://youtu.be/dQw4w9WgXcQ", Action::PlayAudio(link(), U)),
            ("!yt https://youtu.be/dQw4w9WgXcQ", Action::PlayAudio(link(), U)),
            ("!next https://youtu.be/dQw4w9WgXcQ", Action::QueueNextAudio(link(), U)),
            ("!n https://youtu.be/dQw4w9WgXcQ", Action::QueueNextAudio(link(), U)),
            ("!next", Action::Skip(U)),
            ("!n", Action::Skip(U)),
            ("!skip", Action::Skip(U)),
            ("!s", Action::Skip(U)),
            ("!pause", Action::Pause(U)),
            ("!p", Action::Pause(U)),
            ("!resume", Action::Resume(U)),
            ("!r", Action::Resume(U)),
            ("!continue", Action::Resume(U)),
            ("!c", Action::Resume(U)),
            ("!stop", Action::Stop(U)),
            ("!info", Action::Info(U)),
            ("!i", Action::Info(U)),
            ("!help", Action::Help(U)),
            ("!h", Action::Help(U)),
            ("!quit", Action::Quit),
            ("!q", Action::Quit),
            ("!play", Action::None),
        ];
        for (msg, expected) in cases {
            assert_eq!(parse_command(msg, U), expected, "{:?}", msg);
        }
    }

    #[test]
    fn parse_command_keeps_links_intact() {
        for link in [
            "https://www.youtube.com/watch?v=a-b_c1&list=PL-x_y&index=2&t=30s",
            "https://youtu.be/dQw4w9WgXcQ?si=Ab-c_d",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
            "https://soundcloud.com/artist-name/track_name",
        ] {
            let expected = Action::PlayAudio(link.to_string(), U);
            // TeamSpeak wraps pasted links in [URL] tags; anything but one space used to give an empty link
            for msg in [
                format!("!play {}", link),
                format!("!play [URL]{}[/URL]", link),
                format!("!play  {}", link),
                format!("!play\t{}", link),
                format!("  !play {}  ", link),
            ] {
                assert_eq!(parse_command(&msg, U), expected, "{:?}", msg);
            }
        }
    }

    #[test]
    fn parse_command_searches_for_anything_but_a_link() {
        let play = |link: &str| Action::PlayAudio(link.to_string(), U);
        assert_eq!(parse_command("!play tetris theme", U), play("tetris theme"));
        assert_eq!(parse_command("!yt  tetris\t theme ", U), play("tetris theme"));
        assert_eq!(parse_command("!play ytsearch:tetris theme", U), play("ytsearch:tetris theme"));
        assert_eq!(parse_command("!play dQw4w9WgXcQ", U), play("dQw4w9WgXcQ"));
        // a link is the first word only, as before
        assert_eq!(parse_command("!play https://youtu.be/x please", U), play("https://youtu.be/x"));
        assert_eq!(
            parse_command("!n tetris theme", U),
            Action::QueueNextAudio("tetris theme".to_string(), U)
        );
    }

    #[test]
    fn parse_command_volume() {
        let volume = |modifier| Action::ChangeVolume { modifier, user_id: U };
        assert_eq!(parse_command("!v", U), volume(-1.0));
        assert_eq!(parse_command("!volume 50", U), volume(0.5));
        assert_eq!(parse_command("!v 100", U), volume(1.0));
        assert_eq!(parse_command("!v 250", U), volume(1.0));
        assert_eq!(parse_command("!v abc", U), Action::None);
        assert_eq!(parse_command("!v -5", U), Action::None);
    }

    #[test]
    fn info_json_parses_real_yt_dlp_output() {
        let video: InfoJson = serde_json::from_str(VIDEO).unwrap();
        assert_eq!((video.channel.as_str(), video.duration), ("media.ccc.de", Some(2326.0)));
        let live: InfoJson = serde_json::from_str(LIVESTREAM).unwrap();
        assert_eq!((live.channel.as_str(), live.duration), ("Lofi Girl", None));
        let direct: InfoJson = serde_json::from_str(DIRECT_LINK).unwrap();
        assert_eq!((direct.title.as_str(), direct.channel.as_str(), direct.duration), ("song", "", None));
        assert_eq!(video.webpage_url.as_deref(), Some("https://www.youtube.com/watch?v=iHsz6jzjbRc"));
        assert_eq!(direct.webpage_url.as_deref(), Some("http://127.0.0.1:8000/song.mp3"));
    }

    #[test]
    fn ytdlp_errors_get_a_short_reason() {
        for (error, reason) in [
            // captured from yt-dlp
            ("[youtube] aaaaaaaaaaa: This video is unavailable", "Unavailable"),
            ("[generic] 'notaurl' is not a valid URL", "Not a playable link"),
            ("Unsupported URL: https://example.com/", "Not a playable link"),
            ("[generic] x: Unable to download webpage: HTTPConnection(host='127.0.0.1', port=9): Failed to establish a new connection: [Errno 111] Connection refused", "Network error"),
            ("[generic] missing: Unable to download webpage: HTTP Error 404: File not found (caused by <HTTPError 404: File not found>)", "Unavailable"),
            ("[generic] x: Unable to download webpage: (<HTTPConnection(host='10.255.255.1', port=80) at 0x74f4bc8030e0>, 'Connection to 10.255.255.1 timed out. (connect timeout=5.0)')", "Network error"),
            // YouTube's playability reasons as yt-dlp prints them
            ("[youtube] dQw4w9WgXcQ: Unable to download API page: HTTP Error 429: Too Many Requests", "Rate limited, try again later"),
            ("[youtube] dQw4w9WgXcQ: Sign in to confirm you\u{2019}re not a bot. Use --cookies-from-browser or --cookies for the authentication.", "Blocked by YouTube's bot check"),
            ("[youtube] dQw4w9WgXcQ: Video unavailable. The uploader has not made this video available in your country", "Geoblocked"),
            ("[youtube] dQw4w9WgXcQ: The requested video is not available from your location due to geo restriction", "Geoblocked"),
            ("[youtube] dQw4w9WgXcQ: Sign in to confirm your age. This video may be inappropriate for some users.", "Age-restricted"),
            ("[youtube] dQw4w9WgXcQ: Private video. Sign in if you've been granted access to this video", "Private video"),
            ("[youtube] dQw4w9WgXcQ: Join this channel to get access to members-only content like this video, and other exclusive perks.", "Members-only"),
            ("[youtube] dQw4w9WgXcQ: This live event will begin in 2 hours.", "Livestream hasn't started yet"),
            ("[youtube] dQw4w9WgXcQ: Video unavailable. This video contains content from SME, who has blocked it on copyright grounds", "Blocked for copyright"),
            ("[youtube] dQw4w9WgXcQ: Video unavailable. This video is no longer available because the YouTube account associated with this video has been terminated.", "Unavailable"),
            ("unable to download video data: HTTP Error 403: Forbidden", "Download refused (HTTP 403)"),
            ("couldn't spawn yt-dlp: No such file or directory (os error 2)", "The player failed to start"),
            ("something new", "Unknown error"),
        ] {
            assert_eq!(describe_ytdlp_error(error), reason, "{}", error);
        }
    }

    #[test]
    fn chat_messages() {
        let error = "[youtube] x: HTTP Error 429: Too Many Requests";
        assert_eq!(
            failure_message("https://youtu.be/x", false, error, false),
            "Can't play https://youtu.be/x currently (Rate limited, try again later)"
        );
        assert_eq!(
            failure_message("Song", true, error, true),
            format!("Stopped playing Song (Rate limited, try again later)\n{}", error)
        );

        let info = |json| serde_json::from_str::<InfoJson>(json).unwrap();
        assert_eq!(
            now_playing_message("x", Some(&info(VIDEO))),
            "Now playing: 38C3 - Wir wissen wo dein Auto steht - Volksdaten von Volkswagen by media.ccc.de (38:46)"
        );
        assert!(now_playing_message("x", Some(&info(LIVESTREAM))).ends_with("by Lofi Girl"));
        assert_eq!(now_playing_message("x", Some(&info(DIRECT_LINK))), "Now playing: song");
        assert_eq!(now_playing_message("https://x", None), "Now playing: https://x");

        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(59.9), "0:59");
        assert_eq!(format_duration(3725.0), "1:02:05");
    }

    #[test]
    fn ytdlp_args_contract() {
        let args = ytdlp_args("-x", false);
        // play_file reads audio from stdout and get_status the duration from -.info.json
        assert!(args.windows(2).any(|w| w == ["--output", "-"]));
        assert!(args.contains(&"--write-info-json"));
        // without it a Mix link (watch?v=..&list=..) streams the whole playlist as one song
        assert!(args.contains(&"--no-playlist"));
        // !play <words> searches YouTube
        assert!(args.windows(2).any(|w| w == ["--default-search", "ytsearch"]));
        // "--" keeps a link like "-x" from being parsed as an option
        assert_eq!(args[args.len() - 2..], ["--", "-x"]);
        assert!(!args.contains(&"--cookies"));
        assert!(ytdlp_args("x", true).windows(2).any(|w| w == ["--cookies", "cookies.txt"]));
    }

    // playing `link` from now on, or idle with None
    fn new_state(link: Option<&str>) -> Arc<Mutex<PlaybackState>> {
        let mut state = PlaybackState::new();
        let song = link.map(|link| Song::new(link.to_string(), U, "tester".to_string()));
        state.set_song(song.as_ref(), &VecDeque::new());
        state.loading = false;
        Arc::new(Mutex::new(state))
    }

    // queued songs https://youtu.be/<id>, queued by "tester"
    fn queue_of(ids: &[u64]) -> VecDeque<Song> {
        ids.iter()
            .map(|&id| {
                let mut song = Song::new(format!("https://youtu.be/{}", id), U, "tester".to_string());
                song.id = Some(id);
                song
            })
            .collect()
    }

    fn entry_ids(queue: &Value) -> Vec<u64> {
        queue["entries"].as_array().unwrap().iter().map(|e| e["id"].as_u64().unwrap()).collect()
    }

    async fn serve(state: Arc<Mutex<PlaybackState>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app(state)).into_future());
        addr
    }

    // minimal HTTP/1.1 client, enough for these tests without another dependency
    async fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            method,
            path,
            body.len(),
            body
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(!head.to_lowercase().contains("chunked"), "{}", head);
        (head[9..12].parse().unwrap(), body.to_string())
    }

    // engine.io v4 long-polling: handshake, then join the "/" namespace
    async fn socket_connect(addr: SocketAddr) -> String {
        let (_, handshake) = http(addr, "GET", "/socket.io/?EIO=4&transport=polling", "").await;
        let handshake: Value = serde_json::from_str(handshake.strip_prefix('0').unwrap()).unwrap();
        let path = format!("/socket.io/?EIO=4&transport=polling&sid={}", handshake["sid"].as_str().unwrap());
        assert_eq!(http(addr, "POST", &path, "40").await, (200, "ok".to_string()));
        path
    }

    #[tokio::test]
    async fn http_routes() {
        let addr = serve(new_state(None)).await;
        assert_eq!(http(addr, "GET", "/", "").await, (200, "TSMusicbot is running!".to_string()));

        let (code, body) = http(addr, "GET", "/status", "").await;
        assert_eq!(code, 200);
        // the now-playing page reads these fields
        let status: Value = serde_json::from_str(&body).unwrap();
        assert!(status["time"].is_f64(), "{}", status);
        assert!(status["paused"].is_boolean(), "{}", status);
        assert!(status["loading"].is_boolean(), "{}", status);
        assert!(status["duration"].is_u64(), "{}", status);
        assert_eq!(status["link"], "");
        assert!(chrono::DateTime::parse_from_rfc3339(status["timestamp"].as_str().unwrap()).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn status_time_runs_only_while_audio_plays() {
        let state = new_state(Some("https://youtu.be/x"));
        {
            let mut s = state.lock().await;
            s.time_passed = 1.0;
            s.updated = tokio::time::Instant::now();
            s.duration = 19;
        }
        let status = || async { get_status(State(Arc::clone(&state))).await.0 };

        tokio::time::advance(Duration::from_millis(500)).await;
        let st = status().await;
        assert_eq!((st["time"].as_f64(), st["duration"].as_u64()), (Some(1.5), Some(19)));
        // paused and loading hold the clock where the audio stopped
        state.lock().await.paused = true;
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(status().await["time"], 1.0);
        {
            let mut s = state.lock().await;
            s.paused = false;
            s.loading = true;
        }
        tokio::time::advance(Duration::from_millis(500)).await;
        let st = status().await;
        assert_eq!((st["time"].as_f64(), st["loading"].as_bool()), (Some(1.0), Some(true)));
    }

    #[tokio::test]
    async fn socket_emits_status_every_second() {
        let addr = serve(new_state(Some("https://youtu.be/x"))).await;
        let path = socket_connect(addr).await;
        let start = Instant::now();
        let mut statuses = 0;
        while statuses < 2 {
            assert!(start.elapsed() < Duration::from_secs(5), "only {} status events", statuses);
            let (_, body) = http(addr, "GET", &path, "").await;
            for packet in body.split('\x1e') {
                if let Some(event) = packet.strip_prefix("42") {
                    // the payload is a JSON string, not an object
                    let (name, payload): (String, String) = serde_json::from_str(event).unwrap();
                    if name != "status" {
                        continue;
                    }
                    let status: Value = serde_json::from_str(&payload).unwrap();
                    assert_eq!(status["link"], "https://youtu.be/x");
                    statuses += 1;
                }
            }
        }
    }

    #[tokio::test]
    async fn socket_emits_status_right_away_on_change() {
        let state = new_state(None);
        let addr = serve(Arc::clone(&state)).await;
        let path = socket_connect(addr).await;
        let mut body = String::new();
        while !body.contains("42[") {
            body = http(addr, "GET", &path, "").await.1;
        }
        // well before the next 1 s tick
        let start = Instant::now();
        state.lock().await.set_song(Some(&Song::new("https://youtu.be/y".to_string(), U, "tester".to_string())), &VecDeque::new());
        let (_, body) = http(addr, "GET", &path, "").await;
        assert!(body.contains("youtu.be/y") && body.contains(r#"\"loading\":true"#), "{}", body);
        assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
    }

    // socket.io events (name, payload) in arrival order, until done says so; at most 5 s
    async fn socket_events(
        addr: SocketAddr,
        path: &str,
        done: impl Fn(&[(String, Value)]) -> bool,
    ) -> Vec<(String, Value)> {
        let start = Instant::now();
        let mut events = Vec::new();
        while !done(&events) {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out with {:?}", events);
            let (_, body) = http(addr, "GET", path, "").await;
            for packet in body.split('\x1e') {
                if let Some(event) = packet.strip_prefix("42") {
                    let (name, payload): (String, String) = serde_json::from_str(event).unwrap();
                    events.push((name, serde_json::from_str(&payload).unwrap()));
                }
            }
        }
        events
    }

    fn has_queue(events: &[(String, Value)]) -> bool {
        events.iter().any(|(name, _)| name == "queue")
    }

    #[test]
    fn queue_snapshot_is_capped_and_only_versioned_on_change() {
        let mut state = PlaybackState::new();
        let queue = queue_of(&(1..=150).collect::<Vec<_>>());
        state.set_queue(&queue);
        assert_eq!((state.queue.len(), state.queue_total, state.queue_version), (100, 150, 1));
        let json = queue_json(&state);
        assert_eq!((entry_ids(&json)[99], json["total"].as_u64()), (100, Some(150)));
        // the same queue again is not a change
        state.set_queue(&queue);
        assert_eq!(state.queue_version, 1);
        // !stop
        state.set_queue(&VecDeque::new());
        assert_eq!((queue_json(&state), state.queue_version), (json!({"entries": [], "total": 0}), 2));
    }

    #[test]
    fn a_song_start_is_one_update() {
        let mut state = PlaybackState::new();
        let mut queue = queue_of(&[1, 2]);
        state.set_queue(&queue);
        let next = queue.pop_front().unwrap();
        state.set_song(Some(&next), &queue);
        let status = status_json(&state);
        assert_eq!(
            (&status["link"], &status["entry_id"], &status["queued_by"], &status["loading"]),
            (&json!("https://youtu.be/1"), &json!(1), &json!("tester"), &json!(true))
        );
        assert_eq!((entry_ids(&queue_json(&state)), state.queue_version), (vec![2], 2));
        // a direct play has no entry id
        let direct = Song::new("https://youtu.be/d".to_string(), U, "someone".to_string());
        state.set_song(Some(&direct), &queue);
        let status = status_json(&state);
        assert_eq!((&status["entry_id"], &status["queued_by"]), (&Value::Null, &json!("someone")));
        // nothing playing
        state.set_song(None, &queue);
        let status = status_json(&state);
        assert_eq!((&status["link"], &status["entry_id"], &status["queued_by"]), (&json!(""), &Value::Null, &Value::Null));
        assert_eq!(state.queue_version, 2);
    }

    #[tokio::test]
    async fn socket_sends_the_queue_after_the_status_and_on_changes() {
        let state = new_state(Some("https://youtu.be/now"));
        state.lock().await.set_queue(&queue_of(&[1, 2]));
        let addr = serve(Arc::clone(&state)).await;
        let path = socket_connect(addr).await;

        // connect: the status, then the full queue
        let events = socket_events(addr, &path, has_queue).await;
        assert_eq!(events[0].0, "status");
        let queue = &events.iter().find(|(name, _)| name == "queue").unwrap().1;
        assert_eq!((entry_ids(queue), &queue["total"]), (vec![1, 2], &json!(2)));
        let entry = &queue["entries"][0];
        assert_eq!((&entry["link"], &entry["queued_by"]), (&json!("https://youtu.be/1"), &json!("tester")));
        assert!(chrono::DateTime::parse_from_rfc3339(entry["queued_at"].as_str().unwrap()).is_ok());

        // a change goes out right away
        let start = Instant::now();
        state.lock().await.set_queue(&queue_of(&[2]));
        let events = socket_events(addr, &path, has_queue).await;
        assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
        assert_eq!(entry_ids(&events.iter().find(|(name, _)| name == "queue").unwrap().1), vec![2]);

        // an unchanged queue is not sent again, statuses keep coming
        let start = Instant::now();
        let events = socket_events(addr, &path, |_| start.elapsed() > Duration::from_millis(1500)).await;
        assert!(events.iter().all(|(name, _)| name == "status"), "{:?}", events);
    }

    #[tokio::test]
    async fn a_song_change_sends_the_status_before_the_queue_without_it() {
        let state = new_state(Some("https://youtu.be/now"));
        let mut queue = queue_of(&[1, 2]);
        state.lock().await.set_queue(&queue);
        let addr = serve(Arc::clone(&state)).await;
        let path = socket_connect(addr).await;
        socket_events(addr, &path, has_queue).await;

        let next = queue.pop_front().unwrap();
        state.lock().await.set_song(Some(&next), &queue);
        let events = socket_events(addr, &path, has_queue).await;
        let status_at = events.iter().position(|(name, v)| name == "status" && v["entry_id"] == 1).unwrap();
        let queue_at = events.iter().position(|(name, _)| name == "queue").unwrap();
        assert!(status_at < queue_at, "{:?}", events);
        assert_eq!(events[status_at].1["link"], "https://youtu.be/1");
        assert_eq!(entry_ids(&events[queue_at].1), vec![2]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socket_status_task_ends_on_disconnect() {
        let addr = serve(new_state(None)).await;
        let metrics = tokio::runtime::Handle::current().metrics();
        let baseline = metrics.num_alive_tasks();
        for _ in 0..3 {
            let path = socket_connect(addr).await;
            http(addr, "GET", &path, "").await;
            // engine.io close packet, like a closed browser tab
            http(addr, "POST", &path, "1").await;
        }
        // each status loop notices the disconnect on its next 1s tick
        let start = Instant::now();
        while metrics.num_alive_tasks() > baseline {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "{} tasks alive, baseline {}",
                metrics.num_alive_tasks(),
                baseline
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
