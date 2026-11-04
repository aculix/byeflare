# Builds natively for linux/amd64 or linux/arm64.
FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake g++ make pkg-config
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# Lightpanda 1.0.2, pinned by digest. Both architectures' binaries match the
# checksums published with its GitHub release.
FROM lightpanda/browser:1.0.2@sha256:5d1d724c476957d8c2f8393a51fd089477523b0e5aa9c783c48688785d934fb9 AS lightpanda

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates chromium chromium-headless-shell fonts-liberation procps xauth xvfb \
 && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix

COPY --from=lightpanda /usr/bin/lightpanda /usr/local/bin/lightpanda
COPY --from=build /src/target/release/engine-bench /usr/local/bin/engine-bench
ENV HOME=/tmp
ENTRYPOINT ["engine-bench"]
# Hosted services restart anything that exits, so the default does nothing.
CMD ["idle"]
