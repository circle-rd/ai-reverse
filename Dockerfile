# syntax=docker/dockerfile:1

FROM rust:1.89-alpine3.21 AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/reverse-core/Cargo.toml crates/reverse-core/Cargo.toml
COPY crates/reverse-agent/Cargo.toml crates/reverse-agent/Cargo.toml
COPY crates/reverse-relay/Cargo.toml crates/reverse-relay/Cargo.toml
RUN mkdir -p crates/reverse-core/src crates/reverse-agent/src crates/reverse-relay/src \
    && touch crates/reverse-core/src/lib.rs crates/reverse-agent/src/lib.rs crates/reverse-relay/src/lib.rs \
    && echo 'fn main() {}' > crates/reverse-agent/src/main.rs \
    && cargo build --release --locked -p reverse-agent
RUN rm -rf crates
COPY crates crates
RUN touch crates/*/src/*.rs && cargo build --release --locked -p reverse-agent
RUN mkdir -p /state/agent

# ── GPU variant: the same agent next to nvtop, the tool it reads GPU load from ──
# Build it with `docker build --target gpu -t reverse-agent-gpu .`; the default target below
# stays the scratch image. nvtop is GPL-3 and runs as a separate process; its licence and
# source pointer ship in /usr/share/doc/nvtop. 3.4.0 is the first release whose snapshot gives
# the memory in use (`mem_used`/`mem_total`) on every NVIDIA card; Debian 13 only has 3.2.0.
FROM debian:trixie-slim AS nvtop-build
ARG NVTOP_VERSION=3.4.0
ARG NVTOP_SHA256=7979f425dc2b748ff191e3999a0994af6154795e0e8640fae1e177684a296e08
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates cmake curl \
        libdrm-dev libncurses-dev libsystemd-dev libudev-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl -fsSL -o nvtop.tar.gz "https://github.com/Syllo/nvtop/archive/refs/tags/${NVTOP_VERSION}.tar.gz" \
    && echo "${NVTOP_SHA256}  nvtop.tar.gz" | sha256sum -c - \
    && tar -xzf nvtop.tar.gz \
    && cmake -S "nvtop-${NVTOP_VERSION}" -B out -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/opt/nvtop \
        -DNVIDIA_SUPPORT=ON -DAMDGPU_SUPPORT=ON -DINTEL_SUPPORT=ON \
        -DAPPLE_SUPPORT=OFF -DTPU_SUPPORT=OFF \
    && cmake --build out --parallel "$(nproc)" \
    && cmake --install out \
    && mkdir -p /opt/nvtop/share/doc/nvtop \
    && cp "nvtop-${NVTOP_VERSION}/COPYING" /opt/nvtop/share/doc/nvtop/COPYING \
    && echo "nvtop ${NVTOP_VERSION}, GPL-3. Source: https://github.com/Syllo/nvtop/archive/refs/tags/${NVTOP_VERSION}.tar.gz" \
        > /opt/nvtop/share/doc/nvtop/SOURCE

FROM debian:trixie-slim AS gpu
RUN apt-get update \
    && apt-get install -y --no-install-recommends libdrm2 libncursesw6 libsystemd0 libudev1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=nvtop-build /opt/nvtop/ /usr/local/
COPY --from=builder /src/target/release/reverse-agent /reverse-agent
COPY --from=builder --chown=65532:65532 /state/agent /var/lib/reverse-agent
# The NVIDIA container runtime reads this: the agent only needs the monitoring library.
ENV NVIDIA_DRIVER_CAPABILITIES=utility
USER 65532:65532
ENTRYPOINT ["/reverse-agent"]

# ── Default image: scratch, no GPU tool ──
FROM scratch
COPY --from=builder /src/target/release/reverse-agent /reverse-agent
COPY --from=builder --chown=65532:65532 /state/agent /var/lib/reverse-agent
USER 65532:65532
ENTRYPOINT ["/reverse-agent"]
