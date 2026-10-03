# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock VERSION ./
COPY src src
COPY web web
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/popup-relay /popup-relay

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data relay \
 && mkdir /data && chown relay:relay /data
COPY --from=build /popup-relay /usr/local/bin/popup-relay
USER relay
ENV DATA_DIR=/data \
    RELAY_BIND=0.0.0.0:45443 \
    ADMIN_BIND=0.0.0.0:8080
VOLUME /data
EXPOSE 45443 8080
STOPSIGNAL SIGINT
ENTRYPOINT ["popup-relay"]
