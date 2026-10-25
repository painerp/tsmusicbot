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
use tsclientlib::{ClientId, Connection, Identity, MessageTarget, OutCommandExt};
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

pub fn send_ts_message(con: &mut Connection, target: MessageTarget, msg: &str) -> () {
    let state = con.get_state().unwrap_or_else(|e| {
        panic!("Unable to get state: {}", e);
    });

    if let Err(e) = state.send_message(target, &msg).send_with_result(con) {
        error!("Message sending error: {}", e);
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

    if split_vec[0] == "!stop" {
        info!("Stopping all tracks (requested by {})", user_id);
        return Action::Stop;
    }

    if split_vec[0] == "!pause" || split_vec[0] == "!p" {
        return Action::Pause;
    }

    if split_vec[0] == "!continue"
        || split_vec[0] == "!c"
        || split_vec[0] == "!resume"
        || split_vec[0] == "!r"
    {
        return Action::Resume;
    }

    if split_vec[0] == "!next" || split_vec[0] == "!n" {
        if split_vec.len() > 1 {
            info!("Queueing: {} (requested by {})", split_vec[1], user_id);
            return Action::QueueNextAudio(split_vec[1].to_string(), user_id);
        }
        return Action::Skip;
    }

    if split_vec[0] == "!skip" || split_vec[0] == "!s" {
        return Action::Skip;
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
        info!("Playing: {} (requested by {})", split_vec[1], user_id);
        return Action::PlayAudio(split_vec[1].to_string(), user_id);
    }

    Action::None
}

pub async fn get_status(State(state): State<Arc<Mutex<PlaybackState>>>) -> Json<serde_json::Value> {
    let playback_state = state.lock().await.clone();
    // while audio flows, extrapolate from the last frame so time is exact at timestamp
    let mut time = playback_state.time_passed;
    if playback_state.link.is_some() && !playback_state.paused && !playback_state.loading {
        time += playback_state.updated.elapsed().as_secs_f64();
    }

    Json(json!({
        "time": time,
        "timestamp": Utc::now().to_rfc3339(),
        "paused": playback_state.paused,
        "loading": playback_state.loading,
        "duration": playback_state.duration,
        "link": playback_state.link.unwrap_or_default(),
    }))
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
            loop {
                // every second, and right away when the link, loading or paused changes
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = changed.notified() => {}
                }
                if !s.connected() {
                    break;
                }
                let status = get_status(State(Arc::clone(&socket_state))).await;
                s.emit("status", &status.to_string()).ok();
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
    use serde_json::Value;
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
            ("!next", Action::Skip),
            ("!n", Action::Skip),
            ("!skip", Action::Skip),
            ("!s", Action::Skip),
            ("!pause", Action::Pause),
            ("!p", Action::Pause),
            ("!resume", Action::Resume),
            ("!r", Action::Resume),
            ("!continue", Action::Resume),
            ("!c", Action::Resume),
            ("!stop", Action::Stop),
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
    }

    #[test]
    fn ytdlp_args_contract() {
        let args = ytdlp_args("-x", false);
        // play_file reads audio from stdout and get_status the duration from -.info.json
        assert!(args.windows(2).any(|w| w == ["--output", "-"]));
        assert!(args.contains(&"--write-info-json"));
        // without it a Mix link (watch?v=..&list=..) streams the whole playlist as one song
        assert!(args.contains(&"--no-playlist"));
        // "--" keeps a link like "-x" from being parsed as an option
        assert_eq!(args[args.len() - 2..], ["--", "-x"]);
        assert!(!args.contains(&"--cookies"));
        assert!(ytdlp_args("x", true).windows(2).any(|w| w == ["--cookies", "cookies.txt"]));
    }

    // playing `link` from now on, or idle with None
    fn new_state(link: Option<&str>) -> Arc<Mutex<PlaybackState>> {
        let mut state = PlaybackState::new();
        state.set_link(link.map(String::from));
        state.loading = false;
        Arc::new(Mutex::new(state))
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
                    assert_eq!(name, "status");
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
        state.lock().await.set_link(Some("https://youtu.be/y".to_string()));
        let (_, body) = http(addr, "GET", &path, "").await;
        assert!(body.contains("youtu.be/y") && body.contains(r#"\"loading\":true"#), "{}", body);
        assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
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
