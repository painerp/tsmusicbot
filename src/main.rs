extern crate audiopus;
extern crate serde;
extern crate serde_json;
mod helper;

use anyhow::{bail, Result};
use futures::prelude::*;
use log::{debug, error, info, warn};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::Arc;
use std::{env, fs};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::signal::unix::SignalKind;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::{sleep_until, Duration, Instant};

use crate::helper::{
    app, check_dependencies, cleanup_process, connect_to_ts, parse_command, read_config,
    read_info_json, resolve_host, send_ts_message, ytdlp_args,
};
use tsclientlib::events::Event;
use tsclientlib::{ClientId, Connection, DisconnectOptions, MessageTarget, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

#[derive(Debug, Deserialize)]
struct Config {
    host: String,
    password: String,
    name: String,
    id: String,
}

#[derive(Debug, Deserialize)]
struct InfoJson {
    title: String,
    // direct links have no channel, livestreams no duration
    #[serde(default)]
    channel: String,
    duration: Option<f64>,
}

#[derive(Debug, PartialEq)]
enum Action {
    PlayAudio(String, ClientId),
    QueueNextAudio(String, ClientId),
    Skip,
    Pause,
    Resume,
    Stop,
    ChangeVolume { modifier: f32, user_id: ClientId },
    Info(ClientId),
    Help(ClientId),
    Quit,
    None,
}

#[derive(Debug)]
enum PlayTaskCmd {
    Pause,
    Resume,
    Stop,
    ChangeVolume { modifier: f32 },
}

#[derive(Debug)]
enum AudioPacket {
    Payload(OutPacket),
    None,
}

#[derive(Clone)]
struct PlaybackState {
    time_passed: f64,
    // when time_passed was true, so get_status can report the exact position
    updated: Instant,
    paused: bool,
    // a link is set but no audio is going out yet: yt-dlp is starting or stalled
    loading: bool,
    duration: u32,
    link: Option<String>,
    // wakes the socket clients so they get a status right away
    changed: Arc<Notify>,
}

impl PlaybackState {
    fn new() -> Self {
        PlaybackState {
            time_passed: 0.0,
            updated: Instant::now(),
            paused: false,
            loading: false,
            duration: 0,
            link: None,
            changed: Arc::new(Notify::new()),
        }
    }

    // a new song starts loading, or nothing plays with None
    fn set_link(&mut self, link: Option<String>) {
        self.loading = link.is_some();
        self.link = link;
        self.time_passed = 0.0;
        self.updated = Instant::now();
        self.paused = false;
        self.duration = 0;
        self.changed.notify_waiters();
    }
}

const DEFAULT_VOLUME: f32 = 0.2;
const FRAME_SIZE: usize = 960;
const FRAME: Duration = Duration::from_millis(20); // FRAME_SIZE samples at 48 kHz
const MAX_PACKET_SIZE: usize = 3 * 1276;
// a frame this late means yt-dlp stalled
const STALL: Duration = Duration::from_millis(100);

// one frame of s16be stereo pcm
type Pcm = [u8; FRAME_SIZE * 4];

async fn play_file(
    link: String,
    pkt_send: mpsc::Sender<AudioPacket>,
    mut cmd_recv: mpsc::Receiver<PlayTaskCmd>,
    volume: f32,
    playback_state: Arc<Mutex<PlaybackState>>,
) {
    // otherwise the previous song's info.json reports its duration for this one
    let _ = fs::remove_file("-.info.json");
    playback_state.lock().await.set_link(Some(link.clone()));

    // Extract Audio from Youtube using yt-dlp and pipe the output to stdout
    let args = ytdlp_args(&link, fs::metadata("cookies.txt").is_ok());

    let mut ytdlp = match Command::new("yt-dlp")
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
    {
        Err(why) => {
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                error!("Status packet sending error: {}", e);
            }
            panic!("couldn't spawn yt-dlp: {}", why);
        }
        Ok(process) => process,
    };

    let ytdlp_stdout: Stdio = ytdlp
        .stdout
        .take()
        .and_then(|stdout| stdout.try_into().ok())
        .unwrap_or_else(|| panic!("Failed to get stdout of yt-dlp"));

    let mut ffmpeg = match Command::new("ffmpeg")
        .args(&[
            "-loglevel",
            "quiet",
            "-i",
            "pipe:0",
            "-f",
            "opus",
            "-c:a",
            "pcm_s16be",
            "-f",
            "s16be",
            "pipe:1",
        ])
        .stdin(ytdlp_stdout)
        .stdout(Stdio::piped())
        .spawn()
    {
        Err(e) => panic!("couldn't spawn ffmpeg: {}", e),
        Ok(process) => process,
    };

    // read up to a second ahead so a slow pipe never delays a send
    let (frame_send, mut frames) = mpsc::channel::<Pcm>(50);
    let mut ffmpeg_stdout = ffmpeg.stdout.take().unwrap();
    let reader_state = Arc::clone(&playback_state);
    tokio::spawn(async move {
        let mut pcm: Pcm = [0; FRAME_SIZE * 4];
        let mut first_frame = true;
        loop {
            if let Err(e) = ffmpeg_stdout.read_exact(&mut pcm).await {
                if e.kind() == ErrorKind::UnexpectedEof {
                    debug!("ffmpeg_stdout: EOF");
                } else {
                    error!("Error ffmpeg_stdout: {}", e);
                }
                break;
            }
            if first_frame {
                first_frame = false;
                // yt-dlp writes this song's info.json before it starts downloading
                if let Ok(info_json) = read_info_json() {
                    reader_state.lock().await.duration = info_json.duration.unwrap_or(0.0) as u32;
                }
            }
            if frame_send.send(pcm).await.is_err() {
                break;
            }
        }
    });

    stream_frames(&mut frames, &pkt_send, &mut cmd_recv, volume, &playback_state).await;

    playback_state.lock().await.set_link(None);

    debug!("Cleanup...");
    if let Err(e) = pkt_send.send(AudioPacket::None).await {
        error!("Status packet sending error: {}", e);
        return;
    }
    cmd_recv.close();

    cleanup_process(&mut ytdlp, "yt-dlp").await;
    cleanup_process(&mut ffmpeg, "ffmpeg").await;
}

// Sends frame n at start + n * 20 ms. A fixed schedule keeps the audio and the reported time on the
// wall clock; sleeping a fixed time after each frame drifted by seconds per minute under load.
async fn stream_frames(
    frames: &mut mpsc::Receiver<Pcm>,
    pkt_send: &mpsc::Sender<AudioPacket>,
    cmd_recv: &mut mpsc::Receiver<PlayTaskCmd>,
    mut volume: f32,
    playback_state: &Mutex<PlaybackState>,
) {
    let encoder = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
    .expect("Could not create encoder");

    let mut pcm_in: [i16; FRAME_SIZE * 2] = [0; FRAME_SIZE * 2];
    let mut opus_pkt: [u8; MAX_PACKET_SIZE] = [0; MAX_PACKET_SIZE];
    let mut sent: u32 = 0;
    let mut paused = false;
    let mut loading = true;
    let mut next = Instant::now();

    loop {
        tokio::select! {
            biased;
            cmd = cmd_recv.recv() => match cmd {
                None | Some(PlayTaskCmd::Stop) => break,
                Some(PlayTaskCmd::ChangeVolume { modifier }) => volume = modifier,
                Some(PlayTaskCmd::Pause) => {
                    paused = true;
                    report(playback_state, sent, paused, loading).await;
                }
                Some(PlayTaskCmd::Resume) => {
                    paused = false;
                    next = Instant::now();
                    report(playback_state, sent, paused, loading).await;
                }
            },
            pcm = frames.recv(), if !paused => {
                let pcm = match pcm {
                    Some(pcm) => pcm,
                    None => break,
                };
                if loading {
                    // first audio of the song or after a stall: the schedule starts now
                    loading = false;
                    next = Instant::now();
                }

                // adjust volume and encode in opus
                for (sample, bytes) in pcm_in.iter_mut().zip(pcm.chunks_exact(2)) {
                    *sample = (i16::from_be_bytes([bytes[0], bytes[1]]) as f32 * (volume * 0.2)) as i16;
                }
                let len = encoder
                    .encode(&pcm_in, &mut opus_pkt[..])
                    .unwrap_or_else(|e| {
                        error!("Encoding error: {}", e);
                        0
                    });
                let packet = OutAudio::new(&AudioData::C2S {
                    id: 0,
                    codec: CodecType::OpusMusic,
                    data: &opus_pkt[..len],
                });

                sleep_until(next).await;
                // this frame starts playing now, at position sent * 20 ms
                report(playback_state, sent, paused, loading).await;
                if let Err(e) = pkt_send.send(AudioPacket::Payload(packet)).await {
                    error!("Audio packet sending error: {}", e);
                    break;
                }
                sent += 1;
                next += FRAME;
                if Instant::now() > next + STALL {
                    // starved of CPU: catching up would burst the backlog into TeamSpeak
                    warn!("Audio fell {:?} behind, restarting the schedule", Instant::now() - next);
                    next = Instant::now();
                }
            },
            _ = sleep_until(next + STALL), if !paused && !loading => {
                loading = true;
                report(playback_state, sent, paused, loading).await;
            },
        }
    }
}

async fn report(playback_state: &Mutex<PlaybackState>, sent: u32, paused: bool, loading: bool) {
    let mut state = playback_state.lock().await;
    let changed = state.paused != paused || state.loading != loading;
    state.time_passed = sent as f64 * FRAME.as_secs_f64();
    state.updated = Instant::now();
    state.paused = paused;
    state.loading = loading;
    if changed {
        state.changed.notify_waiters();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    real_main().await
}

async fn real_main() -> Result<()> {
    env_logger::init();

    check_dependencies();

    let mut config_json: Config = read_config("config.json");

    let pre_resolve = env::var("PRE_RESOLVE_HOST")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    if pre_resolve {
        config_json.host = resolve_host(&config_json.host).await?;
    }

    let mut init_con: Connection = connect_to_ts(config_json);

    let r = init_con
        .events()
        .try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
        .next()
        .await;
    if let Some(r) = r {
        r?;
    }

    info!("Connected to TeamSpeak Server");

    let (pkt_send, mut pkt_recv) = mpsc::channel(64);
    let (status_send, mut status_recv) = mpsc::channel(64);
    let mut playing: bool = false;
    let mut paused: bool = false;
    let mut volume: f32 = DEFAULT_VOLUME;
    let mut current_playing_link: Option<String> = None;

    let (mut cmd_send, _cmd_recv) = mpsc::channel(4);
    let mut play_queue: VecDeque<String> = VecDeque::new();

    let playback_state = Arc::new(Mutex::new(PlaybackState::new()));

    let app = app(Arc::clone(&playback_state));
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
            .await
            .unwrap_or_else(|e| panic!("Failed to bind to 0.0.0.0:3000: {}", e));
        info!("HTTP server started on 0.0.0.0:3000");
        axum::serve(listener, app)
            .await
            .unwrap_or_else(|e| panic!("Failed to start http server: {}", e));
    });

    loop {
        let events = init_con.events().try_for_each(|e| async {
            match e {
                StreamItem::BookEvents(msg_vec) => {
                    for msg in msg_vec {
                        match msg {
                            Event::Message {
                                invoker: user,
                                target: _,
                                message,
                            } => {
                                if let Err(e) =
                                    status_send.send(parse_command(&message, user.id)).await
                                {
                                    error!("Status packet sending error: {}", e);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            };
            Ok(())
        });

        let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;

        tokio::select! {
            val = status_recv.recv() => {
                match val {
                    None => {
                    },
                    Some(action) => {
                        match action {
                            Action::QueueNextAudio(link, user_id) if playing => {
                                debug!("Queued");
                                play_queue.push_front(link);
                                send_ts_message(&mut init_con, MessageTarget::Client(user_id), "Queued Link");
                            },
                            // when idle, queueing next means playing now
                            Action::PlayAudio(link, user_id) | Action::QueueNextAudio(link, user_id) => {
                                debug!("Playing");
                                let msg: String;
                                if !playing {
                                    playing = true;
                                    paused = false;
                                    let audio_task_pkt_send = pkt_send.clone();

                                    let (task_cmd_send,  task_cmd_recv) = mpsc::channel(4);

                                    cmd_send = task_cmd_send;

                                    current_playing_link = Some(link.clone());
                                    let playback_state_clone = Arc::clone(&playback_state);
                                    tokio::spawn(async move {
                                        play_file(link, audio_task_pkt_send, task_cmd_recv, volume, playback_state_clone).await;
                                    });
                                    msg = "Playing Link".to_string();
                                } else {
                                    play_queue.push_back(link);
                                    msg = "Queued Link".to_string();
                                }
                                send_ts_message(&mut init_con, MessageTarget::Client(user_id), &msg);
                            },
                            Action::ChangeVolume {modifier, user_id} => {
                                debug!("Change volume");
                                let msg: String;
                                if modifier > 0.0 && modifier <= 1.0 {
                                    volume = modifier;
                                    if playing { let _ = cmd_send.send(PlayTaskCmd::ChangeVolume {modifier}).await; };
                                    msg = format!("Volume set to: {}", (modifier * 100.0).floor());
                                } else {
                                    msg = format!("Current Volume: {}", (volume * 100.0).floor());
                                }
                                send_ts_message(&mut init_con, MessageTarget::Client(user_id), &msg);
                            },
                            Action::Skip => {
                                debug!("Skip");
                                if playing {
                                    paused = false;
                                    let _ = cmd_send.send(PlayTaskCmd::Stop).await;
                                };
                            },
                            Action::Resume => {
                                debug!("Resume");
                                if playing && paused {
                                    paused = false;
                                    let _ = cmd_send.send(PlayTaskCmd::Resume).await;
                                };
                            },
                            Action::Pause => {
                                debug!("Pause");
                                if playing && !paused {
                                    paused = true;
                                    let _ = cmd_send.send(PlayTaskCmd::Pause).await;
                                };
                            },
                            Action::Stop => {
                                debug!("Stop");
                                if playing {
                                    paused = false;
                                    play_queue.clear();
                                    let _ = cmd_send.send(PlayTaskCmd::Stop).await;
                                };
                            },
                            Action::Info(user_id) => {
                                debug!("Info");
                                let mut msg = "\nCurrently Playing:\n".to_owned();
                                if playing {
                                    let link = current_playing_link.clone().unwrap_or_default();
                                    match read_info_json() {
                                        Ok(info_json) => {
                                            msg += &format!("Title: {}\nChannel: {}\nLink: {}", info_json.title, info_json.channel, link);
                                        }
                                        Err(_) => {
                                            msg += &format!("{}", link);
                                        }
                                    }
                                } else {
                                    msg += &"Nothing".to_owned();
                                }
                                send_ts_message(&mut init_con, MessageTarget::Client(user_id), &msg);
                            },
                            Action::Help(user_id) => {
                                debug!("Help");
                                let msg = "\nCommands:\n!play <link> or !yt <link> - Play audio from link or queue if already playing\n!next <link> or !n <link> - Queue a track as the next track\n!pause or !p - Pause current track\n!resume, !r, !continue, or !c - Resume current track\n!skip, !s, !next, or !n - Skip current track\n!stop - Stop all tracks\n!volume <modifier> or !v <modifier> - Change volume (modifier should be a number from 0 to 100)\n!info or !i - Get info about current track\n!help or !h - Get this message\n!quit or !q - Quit\n".to_owned();
                                send_ts_message(&mut init_con, MessageTarget::Client(user_id), &msg);
                            },
                            Action::Quit => {
                                debug!("Quit");
                                break;
                            },
                            _ => {},
                        }
                    }
                }
            }

            val = pkt_recv.recv() => {
                match val {
                    None => {},
                    Some(msg) => {
                        if playing {

                            match msg {
                                AudioPacket::Payload(pkt) => {
                                    if let Err(e) = init_con.send_audio(pkt) {
                                        error!("Audio packet sending error: {}", e);
                                        break;
                                    }
                                },
                                AudioPacket::None => {
                                    if play_queue.is_empty(){
                                        playing = false;
                                    } else {
                                        let link = play_queue.pop_front().unwrap();
                                        let audio_task_pkt_send = pkt_send.clone();

                                        let (task_cmd_send,  task_cmd_recv) = mpsc::channel(4);

                                        cmd_send = task_cmd_send;

                                        current_playing_link = Some(link.clone());
                                        let playback_state_clone = Arc::clone(&playback_state);
                                        tokio::spawn(async move {
                                            play_file(link, audio_task_pkt_send, task_cmd_recv, volume, playback_state_clone).await;
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C signal, shutting down...");
                break;
            },
            _ = sigterm.recv() => {
                info!("Received SIGTERM signal, shutting down...");
                break;
            },
            r = events => {
                r?;
                init_con.disconnect(DisconnectOptions::new())?;
                bail!("Disconnected");
            }
        }
    }

    // Disconnect
    init_con.disconnect(DisconnectOptions::new())?;
    init_con.events().for_each(|_| future::ready(())).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::get_status;
    use axum::extract::State;
    use tokio::time::sleep;

    // stream_frames as play_file runs it, fed by the test instead of ffmpeg; time runs virtually
    struct Player {
        state: Arc<Mutex<PlaybackState>>,
        frames: mpsc::Sender<Pcm>,
        packets: mpsc::Receiver<AudioPacket>,
        cmds: mpsc::Sender<PlayTaskCmd>,
        task: tokio::task::JoinHandle<()>,
    }

    fn player() -> Player {
        let mut state = PlaybackState::new();
        state.set_link(Some("https://youtu.be/x".to_string()));
        let state = Arc::new(Mutex::new(state));
        let (frames, mut frame_recv) = mpsc::channel(1000);
        let (pkt_send, packets) = mpsc::channel(1000);
        let (cmds, mut cmd_recv) = mpsc::channel(4);
        let task_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            stream_frames(&mut frame_recv, &pkt_send, &mut cmd_recv, DEFAULT_VOLUME, &task_state).await;
        });
        Player { state, frames, packets, cmds, task }
    }

    impl Player {
        async fn feed(&self, n: usize) {
            for _ in 0..n {
                self.frames.send([0; FRAME_SIZE * 4]).await.unwrap();
            }
        }

        // when each of the next n packets went out
        async fn sent(&mut self, n: usize) -> Vec<Instant> {
            let mut at = Vec::new();
            while at.len() < n {
                match self.packets.recv().await {
                    Some(AudioPacket::Payload(_)) => at.push(Instant::now()),
                    other => panic!("expected audio, got {:?}", other),
                }
            }
            at
        }

        // (time, paused, loading) as the socket clients see it
        async fn status(&self) -> (f64, bool, bool) {
            let status = get_status(State(Arc::clone(&self.state))).await.0;
            (
                status["time"].as_f64().unwrap(),
                status["paused"].as_bool().unwrap(),
                status["loading"].as_bool().unwrap(),
            )
        }
    }

    fn position(frames: u32) -> f64 {
        frames as f64 * FRAME.as_secs_f64()
    }

    #[tokio::test(start_paused = true)]
    async fn frames_go_out_every_20ms_and_time_follows_them() {
        let mut p = player();
        // yt-dlp still starting: loading, clock held at 0
        sleep(Duration::from_secs(3)).await;
        assert_eq!(p.status().await, (0.0, false, true));

        p.feed(500).await;
        let at = p.sent(500).await;
        for (n, t) in at.iter().enumerate() {
            assert_eq!(*t - at[0], FRAME * n as u32, "frame {}", n);
        }
        assert_eq!(p.status().await, (position(499), false, false));
        sleep(Duration::from_millis(10)).await;
        assert_eq!(p.status().await, (position(499) + 0.010, false, false));
    }

    #[tokio::test(start_paused = true)]
    async fn stall_holds_the_clock_and_audio_continues_without_a_jump() {
        let mut p = player();
        p.feed(50).await;
        p.sent(50).await;
        // yt-dlp stops delivering: loading, at the position where the audio stopped
        sleep(Duration::from_secs(2)).await;
        assert_eq!(p.status().await, (position(50), false, true));

        p.feed(10).await;
        let at = p.sent(10).await;
        // the schedule restarts when audio returns, time continues from there
        assert_eq!(at[9] - at[0], FRAME * 9);
        assert_eq!(p.status().await, (position(59), false, false));
    }

    #[tokio::test(start_paused = true)]
    async fn pause_holds_the_clock_and_resume_continues_from_there() {
        let mut p = player();
        p.feed(200).await;
        p.sent(20).await;
        p.cmds.send(PlayTaskCmd::Pause).await.unwrap();
        sleep(Duration::from_secs(5)).await;
        // a frame already waiting for its send time may still go out
        let mut played = 20;
        while p.packets.try_recv().is_ok() {
            played += 1;
        }
        assert!(played <= 21, "{} frames played while paused", played);
        assert_eq!(p.status().await, (position(played), true, false));

        p.cmds.send(PlayTaskCmd::Resume).await.unwrap();
        let at = p.sent(10).await;
        assert_eq!(at[9] - at[0], FRAME * 9);
        assert_eq!(p.status().await, (position(played + 9), false, false));
    }

    #[tokio::test(start_paused = true)]
    async fn stop_works_while_yt_dlp_is_still_starting() {
        let p = player();
        sleep(Duration::from_secs(1)).await;
        p.cmds.send(PlayTaskCmd::Stop).await.unwrap();
        // no waiting for audio that may never come
        tokio::time::timeout(Duration::from_millis(1), p.task).await.unwrap().unwrap();
    }
}
