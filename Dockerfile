FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src ./src
COPY templates ./templates
RUN cargo build --release

FROM debian:bookworm-slim
ARG APP_UID=1000
ARG APP_GID=1000
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid "${APP_GID}" app \
    && useradd --create-home --uid "${APP_UID}" --gid app app
WORKDIR /app
COPY --from=builder /build/target/release/genesleuth /usr/local/bin/genesleuth
RUN mkdir -p /app/data /data/records /data/output/stories \
    && chown -R app:app /app/data /data
USER app
ENV DATABASE_PATH=/app/data/genealogy.sqlite3 \
    RECORDS_DIR=/data/records \
    OUTPUT_DIR=/data/output \
    BIND_ADDR=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["genesleuth"]
CMD ["serve"]
