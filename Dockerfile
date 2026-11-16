# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
RUN rustup target add wasm32-unknown-unknown && cargo install --locked dioxus-cli@0.7.10
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p bitflow-server && cp target/release/bitflow /bitflow \
 && cd app && dx bundle --web --release --out-dir /web

FROM debian:bookworm-slim
# ffmpeg Debian : VAAPI et NVENC inclus.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -r -u 1000 bitflow && mkdir /data && chown bitflow /data
COPY --from=build /bitflow /usr/local/bin/bitflow
COPY --from=build /web/public /app/web
ENV BITFLOW_DATA=/data BITFLOW_WEB=/app/web BITFLOW_LISTEN=0.0.0.0:8080
USER bitflow
VOLUME /data
EXPOSE 8080
CMD ["bitflow"]
