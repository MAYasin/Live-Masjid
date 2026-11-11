# syntax=docker/dockerfile:1
#
# Live Masjid as a container, for a home server or cluster node. (The Raspberry Pi keeps its
# native install: setup.sh / deploy/install.sh.) See docs/DOCKER.md.
#
# The binary is cross-built on the build machine as a static musl executable, linked with
# rust-lld (the same pure-Rust setup as the Pi builds), so multi-arch images build quickly with
# no emulated compile. Like local builds it talks plain HTTP, which every endpoint supports.

FROM --platform=$BUILDPLATFORM rust:1-slim-trixie AS build
ARG TARGETARCH
ARG TARGETVARIANT
ARG LIVEMASJID_BUILD=docker
WORKDIR /src
RUN case "$TARGETARCH$TARGETVARIANT" in \
      amd64) t=x86_64-unknown-linux-musl ;; \
      arm64) t=aarch64-unknown-linux-musl ;; \
      armv7) t=armv7-unknown-linux-musleabihf ;; \
      armv6) t=arm-unknown-linux-musleabihf ;; \
      *) echo "unsupported platform: $TARGETARCH$TARGETVARIANT" >&2; exit 1 ;; \
    esac \
 && echo "$t" > /rust-target \
 && rustup target add "$t"
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src
COPY ui ui
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=livemasjid-target-$TARGETARCH$TARGETVARIANT \
    t=$(cat /rust-target) \
 && LIVEMASJID_BUILD=$LIVEMASJID_BUILD cargo build --release --locked --target "$t" \
 && cp "target/$t/release/livemasjid" /livemasjid

FROM debian:trixie-slim
# aplay (ALSA output), curl (salah times, recordings, Quran downloads), date/tzdata (local time),
# and the Bluetooth client tools, used only when the host's BlueZ is shared (see docs/DOCKER.md).
RUN apt-get update \
 && apt-get install -y --no-install-recommends alsa-utils curl ca-certificates tzdata bluez bluez-alsa-utils \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --gid audio --home-dir /data --no-create-home livemasjid \
 && mkdir -p /data && chown livemasjid:audio /data
COPY --from=build /livemasjid /usr/local/bin/livemasjid
ENV LIVEMASJID_PLATFORM=container \
    LIVEMASJID_DATA=/data \
    LIVEMASJID_PORT=8080
USER livemasjid
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
  CMD curl -fsS "http://127.0.0.1:${LIVEMASJID_PORT}/manifest.webmanifest" >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/livemasjid"]
