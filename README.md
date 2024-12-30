# tsmusicbot

A lightweight yet powerful TeamSpeak 3 music bot built using [tsclientlib](https://github.com/ReSpeak/tsclientlib),
leveraging `ffmpeg` and `yt-dlp` for seamless audio downloading and manipulation.

This project is a fork of [BojanoN's tsmusicbot](https://github.com/BojanoN/tsmusicbot), with additional updates and
improvements.

---

## ✨ Features

- Stream and queue high-quality audio in your TeamSpeak 3 server.
- Simple and intuitive command system for playback control.
- Built for Linux environments with minimal dependencies — `ffmpeg` and `yt-dlp`.
- Configurable via a simple JSON file.
- **Docker-ready** for easy setup and deployment.

---

## 🚀 Getting Started

### Requirements

- A Linux-based operating system.
- Installed versions of `ffmpeg` and `yt-dlp`.

### Development Setup

1. **Download or Build the Bot**:
    - Clone the repository:
      ```bash
      git clone https://github.com/painerp/tsmusicbot.git
      ```
    - Build the bot:
      ```bash
      cargo build
      ```

2. **Create a Configuration File**:  
   In the same directory as the bot executable, create a file named `config.json` and add the necessary configuration
   parameters (see the example below).

3. **Run the Program**:  
   Execute the bot to start using it in your TeamSpeak 3 server.
     ```bash
     RUST_LOG=warn,tsmusicbot=debug cargo run
     ```

---

## 🐳 Docker Setup

Running `tsmusicbot` via Docker is the fastest and easiest way to get up and running.

### Prerequisites

- Install [Docker](https://docs.docker.com/get-docker/) on your system.

### Steps to Run with Docker

1. Create a `config.json` file:
    - This file will define the bot's configuration (see the [Configuration](#configuration) section for details and an
      example).

2. Pull and run the Docker image:
   ```bash
   docker run -d --name tsmusicbot \
       -v $(pwd)/config.json:/app/config.json \
       ghcr.io/painerp/tsmusicbot:latest
   ```

   **Explanation of the command:**
    - `-v $(pwd)/config.json:/app/config.json`: Mounts your local `config.json` into the container.
    - `ghcr.io/painerp/tsmusicbot:latest`: Specifies to use the prebuilt image from the GitHub container registry.

3. (Optional) Check logs to verify everything is running correctly:
   ```bash
   docker logs tsmusicbot
   ```

---

## 📜 Configuration

The bot is configured using a simple JSON file containing the following parameters:

- `host` - The address of your TeamSpeak server (e.g., `example.ts3server.com`).
- `password` - Server password (if any).
- `name` - Nickname for the bot.
- `id` - Base64-encoded unique user ID.

### Example `config.json`:

```json
{
  "host": "example.ts3server.com",
  "password": "",
  "name": "MusicBot",
  "id": "<base64 string>"
}
```

---

## 🎵 Commands

Use the following commands in your TeamSpeak 3 server to control the bot.

| Command                                 | Description                                   |
|-----------------------------------------|-----------------------------------------------|
| `!play <media_url>` / `!yt <media_url>` | Play audio from the provided URL or queue it. |
| `!next <media_url>` / `!n <media_url>`  | Queue a track to play next.                   |
| `!pause` / `!p`                         | Pause the current track.                      |
| `!resume` / `!r` / `!continue` / `!c`   | Resume paused playback.                       |
| `!skip` / `!s` / `!next` / `!n`         | Skip the current track.                       |
| `!stop`                                 | Stop playback and clear the queue.            |
| `!volume <modifier>` / `!v <modifier>`  | Adjust playback volume (0-100).               |
| `!info` / `!i`                          | Display information about the current track.  |
| `!help` / `!h`                          | Display a summary of all available commands.  |
| `!quit` / `!q`                          | Cleanly shut down the bot.                    |

---

## ❤️ Acknowledgments

This project is a fork of [BojanoN's tsmusicbot](https://github.com/BojanoN/tsmusicbot). A huge thanks to all
contributors of the original project and the maintainers of:

- [tsclientlib](https://github.com/ReSpeak/tsclientlib) for their great TeamSpeak library.
- `ffmpeg` and `yt-dlp` for their powerful audio processing capabilities.
