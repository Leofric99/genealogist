FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src ./src
COPY templates ./templates
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 app
WORKDIR /app
COPY --from=builder /build/target/release/genealogy-ocr-studio /usr/local/bin/genealogy-ocr-studio
RUN mkdir -p /app/data /data/records /data/output/stories \
    && chown -R app:app /app/data /data
USER app
ENV DATABASE_PATH=/app/data/genealogy.sqlite3 \
    RECORDS_DIR=/data/records \
    OUTPUT_DIR=/data/output \
    BIND_ADDR=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["genealogy-ocr-studio"]
CMD ["serve"]
