# tsmusicbot

This is a fork from [BojanoN](https://github.com/BojanoN/tsmusicbot).

A simple TeamSpeak3 music bot built using [tsclientlib](https://github.com/ReSpeak/tsclientlib). Uses `ffmpeg` and
`yt-dlp` for audio download and manipulation.

## Requirements

A Linux-based OS, `ffmpeg` and `yt-dlp`.

## Overview

### Getting started

After building or downloading the precompiled program, create a `config.json` file in the current directory and fill out
the desired configuration parameters.
Proceed to execute the program afterwards.

### Building

```
git clone --recurse-submodules https://github.com/BojanoN/tsmusicbot.git
cargo build --release
```

### Supported commands

* `!play <media_url>` or `!yt <media_url>` - Play audio from the provided link or queue it if already playing.
* `!next <media_url>`, `!n <media_url>` - Queue a track as the next track.
* `!pause` or `!p` - Pause the current track playback.
* `!resume`, `!r`, `!continue`, or `!c` - Resume playback of the paused track.
* `!skip`, `!s`, `!next`, or `!n` - Skip the current track.
* `!stop` - Stop all playback and clear the queue.
* `!volume <modifier>` or `!v <modifier>` - Adjust the playback volume. Modifier should be a number between 0 and 100.
* `!info` or `!i` - Display information about the currently playing track.
* `!help` or `!h` - Displays the list of available commands.
* `!quit` or `!q` - Quit the program.

### Configuration parameters

The configuration is stored in a json file.

* `host` - host domain name
* `password` - server password
* `name` - bot nickname
* `id` - base64 encoded id

#### Example configuration file

```
$ cat config.json
{
"host": "a.teamspeak.server.org",
"password": "",
"name": "MusicBot",
"id": "<base64 string>"
}
```
