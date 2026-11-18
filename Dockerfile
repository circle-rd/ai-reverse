# syntax=docker/dockerfile:1

FROM rust:1.89-alpine3.21 AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/reverse-core/Cargo.toml crates/reverse-core/Cargo.toml
COPY crates/reverse-agent/Cargo.toml crates/reverse-agent/Cargo.toml
RUN mkdir -p crates/reverse-core/src crates/reverse-agent/src \
    && touch crates/reverse-core/src/lib.rs crates/reverse-agent/src/lib.rs \
    && echo 'fn main() {}' > crates/reverse-agent/src/main.rs \
    && cargo build --release --locked -p reverse-agent
RUN rm -rf crates
COPY crates crates
RUN touch crates/*/src/*.rs && cargo build --release --locked -p reverse-agent
RUN mkdir -p /state/agent

FROM scratch
COPY --from=builder /src/target/release/reverse-agent /reverse-agent
COPY --from=builder --chown=65532:65532 /state/agent /var/lib/reverse-agent
USER 65532:65532
ENTRYPOINT ["/reverse-agent"]
