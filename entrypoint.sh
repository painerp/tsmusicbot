#!/bin/sh
# YouTube breaks yt-dlp often, so track nightly; each song spawns a fresh yt-dlp, so updates apply without a restart.
update() {
    pip install --no-cache-dir --disable-pip-version-check -q -U --pre "yt-dlp[default]" \
        || echo "entrypoint: yt-dlp update failed, continuing with installed version" >&2
}

update
while sleep 86400; do update; done &

exec "$@"
