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

# ── nvtop, the tool the agent reads GPU load and processes from ──
# 3.4.0 is the first release whose snapshot gives the memory in use (`mem_used`/`mem_total`)
# on every NVIDIA card; Debian 13 only packages 3.2.0. nvtop is GPL-3 and runs as a separate
# process; its licence and source pointer ship in /usr/share/doc/nvtop. libudev is used
# instead of libsystemd to keep the runtime set small.
FROM debian:trixie-slim AS nvtop-build
ARG NVTOP_VERSION=3.4.0
ARG NVTOP_SHA256=7979f425dc2b748ff191e3999a0994af6154795e0e8640fae1e177684a296e08
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates cmake curl \
        libdrm-dev libncurses-dev libudev-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl -fsSL -o nvtop.tar.gz "https://github.com/Syllo/nvtop/archive/refs/tags/${NVTOP_VERSION}.tar.gz" \
    && echo "${NVTOP_SHA256}  nvtop.tar.gz" | sha256sum -c - \
    && tar -xzf nvtop.tar.gz \
    && cmake -S "nvtop-${NVTOP_VERSION}" -B out -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/opt/nvtop \
        -DNVIDIA_SUPPORT=ON -DAMDGPU_SUPPORT=ON -DINTEL_SUPPORT=ON \
        -DAPPLE_SUPPORT=OFF -DTPU_SUPPORT=OFF \
        -DUSE_LIBUDEV_OVER_LIBSYSTEMD=ON \
    && cmake --build out --parallel "$(nproc)" \
    && cmake --install out \
    && mkdir -p /opt/nvtop/share/doc/nvtop \
    && cp "nvtop-${NVTOP_VERSION}/COPYING" /opt/nvtop/share/doc/nvtop/COPYING \
    && echo "nvtop ${NVTOP_VERSION}, GPL-3. Source: https://github.com/Syllo/nvtop/archive/refs/tags/${NVTOP_VERSION}.tar.gz" \
        > /opt/nvtop/share/doc/nvtop/SOURCE

# The distroless runtime ships glibc but not the libraries nvtop loads, so collect them
# next to a nil root and copy them into the final image.
FROM debian:trixie-slim AS nvtop-runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends libcap2 libdrm2 libncursesw6 libudev1 \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /opt/runtime \
    && for lib in \
        /usr/lib/*/libcap.so.2 \
        /usr/lib/*/libdrm.so.2 \
        /usr/lib/*/libncursesw.so.6 \
        /usr/lib/*/libtinfo.so.6 \
        /usr/lib/*/libudev.so.1; do \
        [ -e "$lib" ] || continue; \
        mkdir -p "/opt/runtime$(dirname "$lib")"; \
        cp -L "$lib" "/opt/runtime$(dirname "$lib")/"; \
    done

# ── Agent runtime: no shell, no package manager, nvtop next to the agent ──
# The agent binary is static musl; nvtop needs glibc, which distroless provides. The
# NVIDIA container runtime injects the driver library nvtop loads. The relay image stays
# on scratch.
FROM gcr.io/distroless/cc-debian13:nonroot AS agent
COPY --from=nvtop-runtime /opt/runtime/ /
COPY --from=nvtop-build /opt/nvtop/ /usr/local/
COPY --from=builder /src/target/release/reverse-agent /reverse-agent
COPY --from=builder --chown=65532:65532 /state/agent /var/lib/reverse-agent
# The NVIDIA container runtime reads this: the agent only needs the monitoring library.
ENV NVIDIA_DRIVER_CAPABILITIES=utility
USER 65532:65532
ENTRYPOINT ["/reverse-agent"]
