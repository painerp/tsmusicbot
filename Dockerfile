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
RUN apk add --update --no-cache ffmpeg opus python3 deno

# Set user and group
ARG user=bot
ARG group=bot
ARG uid=2000

RUN adduser --uid=${uid} --disabled-password --gecos="" ${user} \
    && install -d -o ${uid} -g ${uid} /app

USER ${uid}:${uid}
WORKDIR /app

# venv owned by the bot user so entrypoint.sh can keep yt-dlp updated at runtime
ENV PATH="/app/venv/bin:$PATH"
RUN python3 -m venv /app/venv \
    && pip install --no-cache-dir --disable-pip-version-check -U --pre "yt-dlp[default]"

COPY --from=builder /root/.cargo/bin/tsmusicbot /app/tsmusicbot
COPY --chmod=755 entrypoint.sh /entrypoint.sh

ENV RUST_LOG="error,tsmusicbot=info"

EXPOSE 3000
ENTRYPOINT ["/entrypoint.sh"]
CMD ["/app/tsmusicbot"]