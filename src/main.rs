extern crate audiopus;
extern crate serde;
extern crate serde_json;
mod helper;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use futures::prelude::*;
use log::{debug, error, info, warn};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::Arc;
use std::{env, fs};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::signal::unix::SignalKind;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::{sleep_until, Duration, Instant};

use crate::helper::{
    app, check_dependencies, cleanup_process, connect_to_ts, failure_message, format_duration, is_link,
    now_playing_message, parse_command, read_config, read_info_json, resolve_host,
    send_ts_message, ytdlp_args,
};
use tsclientlib::events::Event;
use tsclientlib::{
    ClientId, CommandError, Connection, DisconnectOptions, MessageHandle, MessageTarget, StreamItem,
    TsError,
};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};

#[derive(Debug, Deserialize)]
struct Config {
    host: String,
    password: String,
    name: String,
    id: String,
    // show yt-dlp's exact error in chat, not only the short reason
    #[serde(default)]
    detailed_errors: bool,
}

#[derive(Debug, Deserialize)]
struct InfoJson {
    title: String,
    // direct links have no channel, livestreams no duration
    #[serde(default)]
    channel: String,
    duration: Option<f64>,
    // the page yt-dlp played; for a search that's the video it found
    webpage_url: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Action {
    PlayAudio(String, ClientId),
    QueueNextAudio(String, ClientId),
    Skip(ClientId),
    Pause(ClientId),
    Resume(ClientId),
    Stop(ClientId),
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
    // the first audio is ready, with yt-dlp's info.json if it wrote one
    Started(Option<InfoJson>),
    // error is yt-dlp's message when the song failed, None when it ended or was stopped
    Finished { played: bool, error: Option<String> },
}

// a link someone asked for
struct Song {
    link: String,
    requester: ClientId,
    title: Option<String>,
    // set when it enters the queue; the queue event and status.entry_id refer to it
    id: Option<u64>,
    queued_by: Option<String>,
    queued_at: DateTime<Utc>,
}

impl Song {
    fn new(link: String, requester: ClientId, queued_by: String) -> Self {
        Song {
            link,
            requester,
            title: None,
            id: None,
            queued_by: Some(queued_by),
            queued_at: Utc::now(),
        }
    }

    fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.link)
    }
}

// what the queue event shows of a queued song
#[derive(Clone, PartialEq)]
struct QueueEntry {
    id: u64,
    link: String,
    queued_by: Option<String>,
    queued_at: DateTime<Utc>,
}

// Private chat replies, one at a time: the next waits until the server accepted the last, so a
// reply the anti-flood rejected is sent again later without losing its place.
struct Outbox {
    queue: VecDeque<(ClientId, String)>,
    in_flight: Option<(MessageHandle, Instant)>,
    next: Instant,
    backoff: Duration,
}

impl Outbox {
    fn new() -> Self {
        Outbox {
            queue: VecDeque::new(),
            in_flight: None,
            next: Instant::now(),
            backoff: FLOOD_BACKOFF,
        }
    }

    fn push(&mut self, user_id: ClientId, msg: String) {
        self.queue.push_back((user_id, msg));
    }

    // when send should run next, None while there is nothing to send
    fn due(&self) -> Option<Instant> {
        if self.queue.is_empty() {
            return None;
        }
        Some(match self.in_flight {
            Some((_, sent)) => sent + ANSWER_TIMEOUT,
            None => self.next,
        })
    }

    fn send(&mut self, send: impl FnOnce(ClientId, &str) -> Option<MessageHandle>) {
        if self.in_flight.take().is_some() {
            warn!("No answer to a chat message, sending the next one");
            self.queue.pop_front();
        }
        if let Some((user_id, msg)) = self.queue.front() {
            self.in_flight = send(*user_id, msg).map(|handle| (handle, Instant::now()));
            if self.in_flight.is_none() {
                self.queue.pop_front();
            }
        }
        self.next = Instant::now() + REPLY_INTERVAL;
    }

    fn answered(&mut self, handle: MessageHandle, result: Result<(), CommandError>) {
        if self.in_flight.map(|(h, _)| h) != Some(handle) {
            return;
        }
        self.in_flight = None;
        match result {
            Err(e) if e.error == TsError::ClientIsFlooding => {
                self.next = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(MAX_FLOOD_BACKOFF);
                return;
            }
            Err(e) => warn!("Chat message rejected: {}", e),
            Ok(()) => {}
        }
        self.queue.pop_front();
        self.backoff = FLOOD_BACKOFF;
    }
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
    // the current song's queue entry id (None when it was played directly) and requester
    entry_id: Option<u64>,
    queued_by: Option<String>,
    // the first MAX_QUEUE_ENTRIES of the queue, its full length, and a counter that changes with them
    queue: Vec<QueueEntry>,
    queue_total: usize,
    queue_version: u64,
    // wakes the socket clients so they get a status (and queue) right away
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
            entry_id: None,
            queued_by: None,
            queue: Vec::new(),
            queue_total: 0,
            queue_version: 0,
            changed: Arc::new(Notify::new()),
        }
    }

    // seconds into the song as of now; the clock only runs while audio goes out
    fn position(&self) -> f64 {
        if self.link.is_some() && !self.paused && !self.loading {
            self.time_passed + self.updated.elapsed().as_secs_f64()
        } else {
            self.time_passed
        }
    }

    // A new song starts loading (None: nothing plays) together with the queue without it. One
    // update, so no socket client sees the next song gone from the queue but not yet playing.
    fn set_song(&mut self, song: Option<&Song>, queue: &VecDeque<Song>) {
        self.loading = song.is_some();
        self.link = song.map(|song| song.link.clone());
        self.entry_id = song.and_then(|song| song.id);
        self.queued_by = song.and_then(|song| song.queued_by.clone());
        self.time_passed = 0.0;
        self.updated = Instant::now();
        self.paused = false;
        self.duration = 0;
        self.update_queue(queue);
        self.changed.notify_waiters();
    }

    // the queue changed while the current song plays on
    fn set_queue(&mut self, queue: &VecDeque<Song>) {
        if self.update_queue(queue) {
            self.changed.notify_waiters();
        }
    }

    // returns whether the snapshot changed; only then does queue_version move
    fn update_queue(&mut self, queue: &VecDeque<Song>) -> bool {
        let entries: Vec<QueueEntry> = queue
            .iter()
            .take(MAX_QUEUE_ENTRIES)
            .filter_map(|song| {
                Some(QueueEntry {
                    id: song.id?,
                    link: song.link.clone(),
                    queued_by: song.queued_by.clone(),
                    queued_at: song.queued_at,
                })
            })
            .collect();
        if entries == self.queue && queue.len() == self.queue_total {
            return false;
        }
        self.queue = entries;
        self.queue_total = queue.len();
        self.queue_version += 1;
        true
    }
}

const DEFAULT_VOLUME: f32 = 0.2;
// n0g shows at most this many; the queue event says how many there are in total
const MAX_QUEUE_ENTRIES: usize = 100;
const FRAME_SIZE: usize = 960;
const FRAME: Duration = Duration::from_millis(20); // FRAME_SIZE samples at 48 kHz
const MAX_PACKET_SIZE: usize = 3 * 1276;
// a frame this late means yt-dlp stalled
const STALL: Duration = Duration::from_millis(100);
// TeamSpeak's anti-flood rejects a client's chat messages when it sends too many
const REPLY_INTERVAL: Duration = Duration::from_secs(1);
// rejected attempts count too, so the pause doubles until the server accepts again
const FLOOD_BACKOFF: Duration = Duration::from_secs(2);
const MAX_FLOOD_BACKOFF: Duration = Duration::from_secs(30);
// the server answers every message; without an answer (e.g. a reconnect) move on
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

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

    // Extract Audio from Youtube using yt-dlp and pipe the output to stdout
    let args = ytdlp_args(&link, fs::metadata("cookies.txt").is_ok());

    let mut ytdlp = match Command::new("yt-dlp")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Err(e) => {
            error!("couldn't spawn yt-dlp: {}", e);
            send_finished(&pkt_send, false, Some(format!("couldn't spawn yt-dlp: {}", e))).await;
            return;
        }
        Ok(process) => process,
    };

    // keep yt-dlp's output in the log and its last ERROR to tell the requester
    let mut ytdlp_stderr = BufReader::new(ytdlp.stderr.take().unwrap()).lines();
    let ytdlp_error = tokio::spawn(async move {
        let mut error = None;
        while let Ok(Some(line)) = ytdlp_stderr.next_line().await {
            warn!("yt-dlp: {}", line);
            if let Some(message) = line.strip_prefix("ERROR: ") {
                error = Some(message.to_string());
            }
        }
        error
    });

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
        Err(e) => {
            error!("couldn't spawn ffmpeg: {}", e);
            cleanup_process(&mut ytdlp, "yt-dlp").await;
            send_finished(&pkt_send, false, Some(format!("couldn't spawn ffmpeg: {}", e))).await;
            return;
        }
        Ok(process) => process,
    };

    // read up to a second ahead so a slow pipe never delays a send
    let (frame_send, mut frames) = mpsc::channel::<Pcm>(50);
    let mut ffmpeg_stdout = ffmpeg.stdout.take().unwrap();
    let reader_state = Arc::clone(&playback_state);
    let started_send = pkt_send.clone();
    let reader_link = link.clone();
    let reader = tokio::spawn(async move {
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
                let info_json = read_info_json().ok();
                if let Some(info_json) = &info_json {
                    let mut state = reader_state.lock().await;
                    state.duration = info_json.duration.unwrap_or(0.0) as u32;
                    // before the first frame, so the status that ends loading already has it; play_file
                    // finishes before the main loop changes songs, so this can't hit the next song
                    if let Some(url) = resolved_link(&reader_link, info_json) {
                        state.link = Some(url);
                    }
                }
                let _ = started_send.send(AudioPacket::Started(info_json)).await;
            }
            if frame_send.send(pcm).await.is_err() {
                break;
            }
        }
    });

    let sent = stream_frames(&mut frames, &pkt_send, &mut cmd_recv, volume, &playback_state).await;

    debug!("Cleanup...");
    cmd_recv.close();
    cleanup_process(&mut ytdlp, "yt-dlp").await;
    cleanup_process(&mut ffmpeg, "ffmpeg").await;

    // the reader may still announce a start; let it finish so that can't come after Finished
    drop(frames);
    let _ = reader.await;
    // yt-dlp has exited, so its stderr is complete
    let ytdlp_error = ytdlp_error.await.unwrap_or(None);
    let (played, error) = match sent {
        // stopped by a command
        None => (true, None),
        Some(0) => (false, Some(ytdlp_error.unwrap_or_else(|| "yt-dlp delivered no audio".to_string()))),
        Some(_) => (true, ytdlp_error),
    };
    send_finished(&pkt_send, played, error).await;
}

// A search (anything but an http(s) link) shows as the page yt-dlp found; links stay as queued.
fn resolved_link(link: &str, info_json: &InfoJson) -> Option<String> {
    if is_link(link) {
        return None;
    }
    info_json.webpage_url.clone()
}

async fn send_finished(pkt_send: &mpsc::Sender<AudioPacket>, played: bool, error: Option<String>) {
    if let Err(e) = pkt_send.send(AudioPacket::Finished { played, error }).await {
        error!("Status packet sending error: {}", e);
    }
}

// makes song the current one (queue is what's left waiting) and starts play_file for it; returns
// the channel to control it
async fn start_song(
    song: &Song,
    queue: &VecDeque<Song>,
    pkt_send: &mpsc::Sender<AudioPacket>,
    volume: f32,
    playback_state: &Arc<Mutex<PlaybackState>>,
) -> mpsc::Sender<PlayTaskCmd> {
    // only the main loop changes songs, so a song change is one update with no idle gap between
    playback_state.lock().await.set_song(Some(song), queue);
    let (cmd_send, cmd_recv) = mpsc::channel(4);
    tokio::spawn(play_file(
        song.link.clone(),
        pkt_send.clone(),
        cmd_recv,
        volume,
        Arc::clone(playback_state),
    ));
    cmd_send
}

// Sends frame n at start + n * 20 ms. A fixed schedule keeps the audio and the reported time on the
// wall clock; sleeping a fixed time after each frame drifted by seconds per minute under load.
// Returns how many frames went out, or None when a command stopped it.
async fn stream_frames(
    frames: &mut mpsc::Receiver<Pcm>,
    pkt_send: &mpsc::Sender<AudioPacket>,
    cmd_recv: &mut mpsc::Receiver<PlayTaskCmd>,
    mut volume: f32,
    playback_state: &Mutex<PlaybackState>,
) -> Option<u32> {
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
                None | Some(PlayTaskCmd::Stop) => return None,
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
                    None => return Some(sent),
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
                    return None;
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

    let detailed_errors = config_json.detailed_errors;
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
    let (result_send, mut result_recv) = mpsc::channel(64);
    let mut paused: bool = false;
    let mut volume: f32 = DEFAULT_VOLUME;
    // the song play_file is working on, None when idle
    let mut current: Option<Song> = None;

    let (mut cmd_send, _cmd_recv) = mpsc::channel(4);
    let mut play_queue: VecDeque<Song> = VecDeque::new();
    let mut next_entry_id: u64 = 1;
    let mut outbox = Outbox::new();

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
                                let action = parse_command(&message, user.id);
                                if let Err(e) = status_send.send((action, user.name.to_string())).await
                                {
                                    error!("Status packet sending error: {}", e);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                StreamItem::MessageResult(handle, result) => {
                    let _ = result_send.send((handle, result)).await;
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
                    Some((action, nickname)) => {
                        match action {
                            Action::QueueNextAudio(link, user_id) if current.is_some() => {
                                debug!("Queued next");
                                outbox.push(user_id, format!("Playing next: {}", link));
                                let mut song = Song::new(link, user_id, nickname);
                                song.id = Some(next_entry_id);
                                next_entry_id += 1;
                                play_queue.push_front(song);
                                playback_state.lock().await.set_queue(&play_queue);
                            },
                            // when idle, queueing next means playing now
                            Action::PlayAudio(link, user_id) | Action::QueueNextAudio(link, user_id) => {
                                let mut song = Song::new(link, user_id, nickname);
                                let msg = if current.is_none() {
                                    debug!("Playing");
                                    paused = false;
                                    cmd_send = start_song(&song, &play_queue, &pkt_send, volume, &playback_state).await;
                                    let msg = format!("Loading {} ...", song.link);
                                    current = Some(song);
                                    msg
                                } else {
                                    debug!("Queued");
                                    let msg = format!("Queued as #{}: {}", play_queue.len() + 1, song.link);
                                    song.id = Some(next_entry_id);
                                    next_entry_id += 1;
                                    play_queue.push_back(song);
                                    playback_state.lock().await.set_queue(&play_queue);
                                    msg
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::ChangeVolume {modifier, user_id} => {
                                debug!("Change volume");
                                let msg = if modifier > 0.0 && modifier <= 1.0 {
                                    volume = modifier;
                                    if current.is_some() { let _ = cmd_send.send(PlayTaskCmd::ChangeVolume {modifier}).await; };
                                    format!("Volume set to {}%", (modifier * 100.0).round())
                                } else {
                                    format!("Volume is {}%", (volume * 100.0).round())
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::Skip(user_id) => {
                                debug!("Skip");
                                let msg = match &current {
                                    Some(song) => {
                                        paused = false;
                                        let _ = cmd_send.send(PlayTaskCmd::Stop).await;
                                        format!("Skipped {}", song.name())
                                    }
                                    None => "Nothing is playing".to_string(),
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::Resume(user_id) => {
                                debug!("Resume");
                                let msg = match &current {
                                    Some(song) if paused => {
                                        paused = false;
                                        let _ = cmd_send.send(PlayTaskCmd::Resume).await;
                                        format!("Resumed {}", song.name())
                                    }
                                    Some(_) => "Not paused".to_string(),
                                    None => "Nothing is playing".to_string(),
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::Pause(user_id) => {
                                debug!("Pause");
                                let msg = match &current {
                                    Some(song) if !paused => {
                                        paused = true;
                                        let _ = cmd_send.send(PlayTaskCmd::Pause).await;
                                        format!("Paused {}", song.name())
                                    }
                                    Some(_) => "Already paused".to_string(),
                                    None => "Nothing is playing".to_string(),
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::Stop(user_id) => {
                                debug!("Stop");
                                let msg = if current.is_some() {
                                    paused = false;
                                    let cleared = play_queue.len();
                                    play_queue.clear();
                                    playback_state.lock().await.set_queue(&play_queue);
                                    let _ = cmd_send.send(PlayTaskCmd::Stop).await;
                                    match cleared {
                                        0 => "Stopped".to_string(),
                                        1 => "Stopped and cleared 1 queued song".to_string(),
                                        n => format!("Stopped and cleared {} queued songs", n),
                                    }
                                } else {
                                    "Nothing is playing".to_string()
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::Info(user_id) => {
                                debug!("Info");
                                let msg = match &current {
                                    Some(song) => {
                                        let state = playback_state.lock().await.clone();
                                        let mut progress = if state.loading {
                                            "Loading ...".to_string()
                                        } else if state.duration > 0 {
                                            format!("{} / {}", format_duration(state.position()), format_duration(state.duration as f64))
                                        } else {
                                            format_duration(state.position())
                                        };
                                        if paused {
                                            progress += " (paused)";
                                        }
                                        let mut msg = format!("\nNow playing: {}\n{}\n{}", song.name(), progress, song.link);
                                        if !play_queue.is_empty() {
                                            msg += &format!("\n{} more in the queue", play_queue.len());
                                        }
                                        msg
                                    }
                                    None => "Nothing is playing".to_string(),
                                };
                                outbox.push(user_id, msg);
                            },
                            Action::Help(user_id) => {
                                debug!("Help");
                                let msg = "\nCommands:\n!play <link or search> or !yt <link or search> - Play a link or the first YouTube result, or queue it if already playing\n!next <link or search> or !n <link or search> - Queue a track as the next track\n!pause or !p - Pause current track\n!resume, !r, !continue, or !c - Resume current track\n!skip, !s, !next, or !n - Skip current track\n!stop - Stop all tracks\n!volume <modifier> or !v <modifier> - Change volume (modifier should be a number from 0 to 100)\n!info or !i - Get info about current track\n!help or !h - Get this message\n!quit or !q - Quit\n".to_owned();
                                outbox.push(user_id, msg);
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
                    Some(AudioPacket::Payload(pkt)) => {
                        if current.is_some() {
                            if let Err(e) = init_con.send_audio(pkt) {
                                error!("Audio packet sending error: {}", e);
                                break;
                            }
                        }
                    },
                    Some(AudioPacket::Started(info)) => {
                        if let Some(song) = current.as_mut() {
                            let msg = now_playing_message(&song.link, info.as_ref());
                            song.title = info.map(|info| info.title);
                            outbox.push(song.requester, msg);
                        }
                    },
                    Some(AudioPacket::Finished { played, error }) => {
                        if let (Some(song), Some(error)) = (current.take(), error) {
                            let msg = failure_message(song.name(), played, &error, detailed_errors);
                            outbox.push(song.requester, msg);
                        }
                        match play_queue.pop_front() {
                            Some(song) => {
                                paused = false;
                                cmd_send = start_song(&song, &play_queue, &pkt_send, volume, &playback_state).await;
                                current = Some(song);
                            }
                            None => playback_state.lock().await.set_song(None, &play_queue),
                        }
                    },
                }
            }

            _ = sleep_until(outbox.due().unwrap_or_else(Instant::now)), if outbox.due().is_some() => {
                outbox.send(|user_id, msg| send_ts_message(&mut init_con, MessageTarget::Client(user_id), msg));
            }

            Some((handle, result)) = result_recv.recv() => outbox.answered(handle, result),

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
        let song = Song::new("https://youtu.be/x".to_string(), ClientId(1), "tester".to_string());
        state.set_song(Some(&song), &VecDeque::new());
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

    #[test]
    fn searches_show_the_page_yt_dlp_found() {
        let found = InfoJson {
            title: "Tetris Theme".to_string(),
            channel: String::new(),
            duration: Some(90.0),
            webpage_url: Some("https://www.youtube.com/watch?v=NmCCQxVBfyM".to_string()),
        };
        for link in ["ytsearch:tetris", "tetris"] {
            assert_eq!(resolved_link(link, &found).as_deref(), Some("https://www.youtube.com/watch?v=NmCCQxVBfyM"), "{}", link);
        }
        // links stay exactly as queued, t= and all
        for link in ["https://www.youtube.com/watch?v=NmCCQxVBfyM&t=30s", "http://media/song.mp3"] {
            assert_eq!(resolved_link(link, &found), None, "{}", link);
        }
        assert_eq!(resolved_link("ytsearch:x", &InfoJson { webpage_url: None, ..found }), None);
    }

    fn flooding() -> Result<(), CommandError> {
        Err(CommandError { error: TsError::ClientIsFlooding, missing_permission: None })
    }

    #[tokio::test(start_paused = true)]
    async fn outbox_resends_flood_rejected_replies_in_order() {
        let mut outbox = Outbox::new();
        let mut sent = Vec::new();
        let mut send = |outbox: &mut Outbox| {
            let mut handle = None;
            outbox.send(|_, msg| {
                sent.push(msg.to_string());
                handle = Some(MessageHandle(sent.len() as u16));
                handle
            });
            handle.unwrap()
        };
        outbox.push(ClientId(1), "a".to_string());
        outbox.push(ClientId(1), "b".to_string());
        assert_eq!(outbox.due(), Some(Instant::now()));

        let a = send(&mut outbox);
        // waits for the server's answer before sending b
        assert_eq!(outbox.due(), Some(Instant::now() + ANSWER_TIMEOUT));
        outbox.answered(a, flooding());
        assert_eq!(outbox.due(), Some(Instant::now() + FLOOD_BACKOFF));
        let a = send(&mut outbox);
        outbox.answered(a, flooding());
        // rejected again: the pause doubles
        assert_eq!(outbox.due(), Some(Instant::now() + FLOOD_BACKOFF * 2));
        let a = send(&mut outbox);
        outbox.answered(a, Ok(()));
        assert_eq!(outbox.due(), Some(Instant::now() + REPLY_INTERVAL));
        let b = send(&mut outbox);
        outbox.answered(b, Ok(()));
        assert_eq!(outbox.due(), None);
        assert_eq!(sent, ["a", "a", "a", "b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn outbox_moves_on_without_an_answer() {
        let mut outbox = Outbox::new();
        let mut sent = Vec::new();
        outbox.push(ClientId(1), "a".to_string());
        outbox.push(ClientId(1), "b".to_string());
        outbox.send(|_, msg| {
            sent.push(msg.to_string());
            Some(MessageHandle(1))
        });
        // e.g. lost in a reconnect
        sleep(ANSWER_TIMEOUT).await;
        outbox.send(|_, msg| {
            sent.push(msg.to_string());
            Some(MessageHandle(2))
        });
        assert_eq!(sent, ["a", "b"]);
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
