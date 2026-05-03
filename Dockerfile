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
FROM rust:alpine AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release --bin hc-yolink

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3

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
