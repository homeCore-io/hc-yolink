# =============================================================================
# hc-yolink — HomeCore YoLink Plugin
# Alpine Linux — minimal, static-friendly runtime
# =============================================================================
#
# Build:
#   docker build -t hc-yolink:latest .
#
# Run:
#   docker run -d \
#     -v ./config/config.toml:/opt/hc-yolink/config/config.toml:ro \
#     -v hc-yolink-logs:/opt/hc-yolink/logs \
#     hc-yolink:latest
#
# Volumes:
#   /opt/hc-yolink/config   config.toml (credentials)
#   /opt/hc-yolink/logs     rolling log files
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — Build
# -----------------------------------------------------------------------------
FROM rust:1.95-alpine3.23@sha256:606fd313a0f49743ee2a7bd49a0914bab7deedb12791f3a846a34a4711db7ed2 AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release --bin hc-yolink

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3.23@sha256:5b10f432ef3da1b8d4c7eb6c487f2f5a8f096bc91145e68878dd4a5019afde11

# `apk upgrade` first pulls CVE patches for packages baked into the
# alpine:3 base since the upstream image was last rebuilt. Defense
# in depth — without this, `apk add --no-cache` only refreshes the
# named packages, leaving busybox/musl/etc. on the base's frozen
# versions.
RUN apk upgrade --no-cache && \
    apk add --no-cache \
        ca-certificates \
        libssl3 \
        tzdata

RUN adduser -D -h /opt/hc-yolink hcyolink

COPY --from=builder /build/target/release/hc-yolink /usr/local/bin/hc-yolink
RUN chmod 755 /usr/local/bin/hc-yolink

RUN mkdir -p /opt/hc-yolink/config /opt/hc-yolink/logs

COPY config/config.toml.example /opt/hc-yolink/config/config.toml.example

RUN chown -R hcyolink:hcyolink /opt/hc-yolink

USER hcyolink
WORKDIR /opt/hc-yolink

VOLUME ["/opt/hc-yolink/config", "/opt/hc-yolink/logs"]

ENV RUST_LOG=info

ENTRYPOINT ["hc-yolink"]
