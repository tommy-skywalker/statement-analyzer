# ---- build stage ----
FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Cache dependencies first
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main(){}" > src/main.rs \
    && cargo build --release 2>/dev/null || true
RUN rm -rf src

# Build the real binary
COPY . .
RUN touch src/main.rs && cargo build --release

# ---- runtime stage ----
FROM debian:bookworm-slim
# ca-certificates for TLS; p7zip-full + unar enable RAR/7z archive extraction; tar is built in
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates p7zip-full unar \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/statement-analyzer /usr/local/bin/statement-analyzer

# Railway/most PaaS inject PORT; the app reads it (defaults to 8000).
ENV PORT=8000
EXPOSE 8000
CMD ["statement-analyzer"]
