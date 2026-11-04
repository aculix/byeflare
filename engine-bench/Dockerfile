# Build from the repository root: docker build -f engine-bench/Dockerfile .
# Builds natively for linux/amd64 or linux/arm64.
FROM rust:1.98.0-slim-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake g++ make pkg-config clang libclang-dev git \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY engine-bench/Cargo.toml ./engine-bench/Cargo.toml
COPY engine-bench/src ./engine-bench/src
COPY scripts ./scripts
RUN ./scripts/fetch-cfsolve.sh
RUN cargo build --release -p engine-bench

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
COPY --from=build /src/vendor/cfsolve/profiles/safari-26-ios.json /usr/local/share/cfsolve/profiles/safari-26-ios.json
COPY --from=build /src/vendor/cfsolve/profiles/safari-26-ios /usr/local/share/cfsolve/profiles/safari-26-ios
ENV HOME=/tmp
ENTRYPOINT ["engine-bench"]
# Hosted services restart anything that exits, so the default does nothing.
CMD ["idle"]
