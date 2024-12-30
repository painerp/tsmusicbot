FROM alpine:latest as builder

RUN apk add --update --no-cache cargo rust rust-gdb rust-src rust-lldb

RUN apk add --update --no-cache musl-dev pkgconfig openssl-dev opus-dev

WORKDIR /usr/src/tsmusicbot
COPY . .

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/usr/src/tsmusicbot/target \
    cargo install --path .

FROM alpine:latest as final

# Switch to root
USER root
RUN apk add --update --no-cache ffmpeg yt-dlp opus

# Set user and group
ARG user=bot
ARG group=bot
ARG uid=2000

RUN adduser --uid=${uid} --disabled-password --gecos="" ${user}

USER ${uid}:${uid}
WORKDIR /app

COPY --from=builder /root/.cargo/bin/tsmusicbot /app/tsmusicbot
CMD ["chown -R ${uid}:${uid} /app"]

ENV RUST_LOG="error,tsmusicbot=info"
ENTRYPOINT ["/app/tsmusicbot"]