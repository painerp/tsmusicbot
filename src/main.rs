extern crate audiopus;
extern crate byteorder;
extern crate serde;
extern crate serde_json;
mod helper;

use anyhow::{bail, Result};
use byteorder::{BigEndian, ReadBytesExt};
use futures::prelude::*;
use log::{debug, error, info};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::process::{Command, Stdio};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};

use crate::helper::{
    check_dependencies, cleanup_process, connect_to_ts, parse_command, read_config, read_info_json,
};
use tsclientlib::events::Event;
use tsclientlib::{
    ClientId, Connection, DisconnectOptions, MessageTarget, OutCommandExt, StreamItem,
};
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
    id: String,
    title: String,
    channel: String,
    duration: u32,
    view_count: u64,
    webpage_url: String,
}

#[derive(Debug)]
enum Action {
    PlayAudio(String),
    QueueNextAudio(String),
    Skip,
    Pause,
    Resume,
    Stop,
    ChangeVolume { modifier: f32 },
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

const DEFAULT_VOLUME: f32 = 0.2;

async fn play_file(
    link: String,
    pkt_send: mpsc::Sender<AudioPacket>,
    mut cmd_recv: mpsc::Receiver<PlayTaskCmd>,
    volume: f32,
) {
    const FRAME_SIZE: usize = 960;
    const MAX_PACKET_SIZE: usize = 3 * 1276;

    let codec = CodecType::OpusMusic;
    let mut current_volume = volume;
    let mut paused = false;

    // Extract Audio from Youtube using yt-dlp and pipe the output to stdout
    let mut ytdlp = match Command::new("yt-dlp")
        .args(&[
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
            "--output",
            "-",
            &link,
        ])
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
        .stdin(
            ytdlp
                .stdout
                .take()
                .unwrap_or_else(|| panic!("Failed to get stdout of yt-dlp")),
        )
        .stdout(Stdio::piped())
        .spawn()
    {
        Err(e) => panic!("couldn't spawn ffmpeg: {}", e),
        Ok(process) => process,
    };

    // Setup Encoder
    let encoder = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
    .expect("Could not create encoder");

    let mut pcm_in_be: [i16; FRAME_SIZE * 2] = [0; FRAME_SIZE * 2];
    let mut opus_pkt: [u8; MAX_PACKET_SIZE] = [0; MAX_PACKET_SIZE];

    let ffmpeg_stdout = &mut ffmpeg.stdout.take().unwrap();

    loop {
        let cmd: Option<PlayTaskCmd> = timeout(Duration::from_micros(1), cmd_recv.recv())
            .await
            .unwrap_or_else(|_| None);

        match cmd {
            None => {}
            Some(PlayTaskCmd::ChangeVolume { modifier }) => {
                current_volume = modifier;
            }
            Some(PlayTaskCmd::Stop) => {
                break;
            }
            Some(PlayTaskCmd::Pause) => {
                paused = true;
            }
            Some(PlayTaskCmd::Resume) => {
                paused = false;
            }
        };

        if paused {
            debug!("Paused wait...");
            sleep(Duration::from_millis(500)).await;
            continue;
        }

        match ffmpeg_stdout.read_i16_into::<BigEndian>(&mut pcm_in_be) {
            Err(e) => {
                if e.kind() == ErrorKind::UnexpectedEof {
                    debug!("ffmpeg_stdout: EOF");
                } else {
                    error!("Error ffmpeg_stdout: {}", e);
                }
                break;
            }
            Ok(_) => {}
        };

        // adjust volume and encode in opus
        for i in 0..FRAME_SIZE * 2 {
            pcm_in_be[i] = (pcm_in_be[i] as f32 * current_volume) as i16;
        }
        let len = encoder
            .encode(&pcm_in_be, &mut opus_pkt[..])
            .unwrap_or_else(|e| {
                error!("Encoding error: {}", e);
                0
            });

        let packet = OutAudio::new(&AudioData::C2S {
            id: 0,
            codec,
            data: &opus_pkt[..len],
        });

        if let Err(e) = pkt_send.send(AudioPacket::Payload(packet)).await {
            error!("Audio packet sending error: {}", e);
            if let Err(e) = pkt_send.send(AudioPacket::None).await {
                error!("Status packet sending error: {}", e);
                return;
            }
            break;
        }

        sleep(Duration::from_micros(17000)).await;
    }

    debug!("Cleanup...");
    if let Err(e) = pkt_send.send(AudioPacket::None).await {
        error!("Status packet sending error: {}", e);
        return;
    }
    cmd_recv.close();

    cleanup_process(&mut ytdlp, "yt-dlp").await;
    cleanup_process(&mut ffmpeg, "ffmpeg").await;
}

#[tokio::main]
async fn main() -> Result<()> {
    real_main().await
}

async fn real_main() -> Result<()> {
    env_logger::init();

    check_dependencies();

    let config_json: Config = read_config("config.json");

    let mut init_con: Connection = connect_to_ts(config_json);

    let r = init_con
        .events()
        .try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
        .next()
        .await;
    if let Some(r) = r {
        r?;
    }

    let (pkt_send, mut pkt_recv) = mpsc::channel(64);
    let (status_send, mut status_recv) = mpsc::channel(64);
    let mut playing: bool = false;
    let mut paused: bool = false;
    let mut volume: f32 = DEFAULT_VOLUME;
    let mut current_playing_link: Option<String> = None;

    let (mut cmd_send, _cmd_recv) = mpsc::channel(4);
    let mut play_queue: VecDeque<String> = VecDeque::new();

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

        tokio::select! {
            val = status_recv.recv() => {
                match val {
                    None => {
                    },
                    Some(action) => {
                        match action {
                            Action::PlayAudio(link) => {
                                debug!("Playing");
                                if !playing {
                                    playing = true;
                                    paused = false;
                                    let audio_task_pkt_send = pkt_send.clone();

                                    let (task_cmd_send,  task_cmd_recv) = mpsc::channel(4);

                                    cmd_send = task_cmd_send;

                                    current_playing_link = Some(link.clone());
                                    tokio::spawn(async move {
                                        play_file(link, audio_task_pkt_send, task_cmd_recv, volume).await;
                                    });
                                } else {
                                    play_queue.push_back(link);
                                }
                            },
                            Action::ChangeVolume {modifier} => {
                                debug!("Change volume");
                                volume = modifier;
                                if playing { let _ = cmd_send.send(PlayTaskCmd::ChangeVolume {modifier}).await; };
                            },
                            Action::QueueNextAudio(link) => {
                                debug!("Queued");
                                if playing {
                                    play_queue.push_front(link);
                                } else {
                                    Action::PlayAudio(link);
                                }
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

                                let state = init_con.get_state().unwrap_or_else(|e| {
                                    panic!("Unable to get state: {}", e);
                                });

                                match state.send_message(MessageTarget::Client(user_id), &msg).send_with_result(&mut init_con) {
                                    Ok(_) => (),
                                    Err(e) => error!("Message sending error: {}", e),
                                };
                            },
                            Action::Help(user_id) => {
                                debug!("Help");
                                let msg = "\nCommands:\n!play <link> or !yt <link> - Play audio from link or queue if already playing\n!next <link>, !n <link>, !next, or !n - Queue a track as next or skip current track\n!pause or !p - Pause current track\n!resume, !r, !continue, or !c - Resume current track\n!skip or !s - Skip current track\n!stop - Stop all tracks\n!volume <modifier> or !v <modifier> - Change volume\n!info or !i - Get info about current track\n!help or !h - Get this message\n!quit or !q - Quit\n".to_owned();

                                let state = init_con.get_state().unwrap_or_else(|e| {
                                    panic!("Unable to get state: {}", e);
                                });
                                if let Err(e) = state.send_message(MessageTarget::Client(user_id), &msg).send_with_result(&mut init_con)
                                {
                                    error!("Message sending error: {}", e);
                                };
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
                                        tokio::spawn(async move {
                                            play_file(link, audio_task_pkt_send, task_cmd_recv, volume).await;
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => { break; }
            r = events => {
                r?;
                init_con.disconnect(DisconnectOptions::new())?;
                bail!("Disconnected");
            }
        };
    }

    // Disconnect
    init_con.disconnect(DisconnectOptions::new())?;
    init_con.events().for_each(|_| future::ready(())).await;

    Ok(())
}
