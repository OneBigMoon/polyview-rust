FROM rust:1.90-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 polyview \
    && useradd --uid 1000 --gid polyview --no-create-home --shell /usr/sbin/nologin polyview \
    && install -d -o polyview -g polyview /data /media \
    && install -d -o polyview -g polyview -m 0700 /data/telegram
COPY --from=builder /build/target/release/polyview /usr/local/bin/polyview
COPY LICENSE /usr/share/doc/polyview/LICENSE
COPY web/vendor/hls.LICENSE /usr/share/doc/polyview/hls.LICENSE
COPY web/vendor/three.LICENSE /usr/share/doc/polyview/three.LICENSE
ENV POLYVIEW_BIND=0.0.0.0:8080 \
    POLYVIEW_DATA_DIR=/data \
    POLYVIEW_MEDIA_DIR=/media
USER polyview
EXPOSE 8080
VOLUME ["/data", "/media"]
ENTRYPOINT ["polyview"]
