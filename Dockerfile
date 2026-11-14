# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1.100-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/spokes /spokes \
    && mkdir /data

# ---- runtime ----
# Distroless with glibc: no shell, no package manager, runs as uid 65532 (nonroot).
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /spokes /usr/local/bin/spokes
COPY --from=build --chown=65532:65532 /data /data
ENV SPOKES_BIND=0.0.0.0:8787 \
    SPOKES_DB_PATH=/data/spokes.db
USER 65532:65532
VOLUME ["/data"]
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/spokes", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/spokes"]
