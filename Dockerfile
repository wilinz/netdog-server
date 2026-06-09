# ---- build ----
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/netdog-server /usr/local/bin/netdog-server
EXPOSE 8688
ENTRYPOINT ["/usr/local/bin/netdog-server"]
