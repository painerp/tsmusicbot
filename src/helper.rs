use crate::{Action, Config, InfoJson, PlaybackState};
use anyhow::{Context, Result};
use axum::extract::State;
use axum::Json;
use chrono::Utc;
use log::{error, info};
use serde_json::json;
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
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

pub async fn cleanup_process(process: &mut std::process::Child, name: &str) -> () {
    if let Err(e) = process.kill() {
        error!("Failed to kill {}: {}", name, e);
    }
    match process.wait() {
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

    if &sanitized[..=0] != "!" {
        return Action::None;
    }

    let split_vec: Vec<&str> = sanitized.split(' ').collect();

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
    let playback_state = state.lock().await;
    let mut duration: u32 = 0;

    if fs::metadata("-.info.json").is_ok() && playback_state.link.clone().unwrap_or_default() != ""
    {
        duration = match read_info_json() {
            Ok(info_json) => info_json.duration,
            Err(err) => {
                error!("Failed to read info JSON: {}", err);
                0
            }
        };
    }

    Json(json!({
        "time": playback_state.time_passed,
        "timestamp": Utc::now().to_rfc3339(),
        "paused": playback_state.paused,
        "duration": duration,
        "link": playback_state.link.clone().unwrap_or_default(),
    }))
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
