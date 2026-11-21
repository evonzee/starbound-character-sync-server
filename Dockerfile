FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN useradd -r -u 10001 -d /data sbsync && mkdir /data && chown sbsync /data
COPY --from=build /src/target/release/starbound-sync-server /usr/local/bin/
USER sbsync
ENV SYNC_DATA_DIR=/data SYNC_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["starbound-sync-server"]
CMD ["serve"]
