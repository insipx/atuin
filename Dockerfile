FROM rust:1.68.0 AS builder
WORKDIR app

COPY . .

# Ensure working C compile setup (not installed by default in arm64 images)
RUN apt update && apt install build-essential -y
RUN cargo build --release --bin atuin

FROM debian:bullseye-20230320-slim AS runtime

RUN useradd -c 'atuin user' atuin && mkdir /config && chown atuin:atuin /config
RUN apt update && apt install ca-certificates -y # so that webhooks work
WORKDIR app

USER atuin

ENV TZ=Etc/UTC
ENV RUST_LOG=atuin::api=info
ENV ATUIN_CONFIG_DIR=/config

COPY --from=builder /app/target/release/atuin /usr/local/bin
ENTRYPOINT ["/usr/local/bin/atuin"]
