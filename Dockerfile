# Stage 1: Compute a recipe file
FROM lukemathwalker/cargo-chef:latest-rust-1.75-alpine AS planner
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 2: Cache dependencies
FROM lukemathwalker/cargo-chef:latest-rust-1.75-alpine AS cacher
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json

# Stage 3: Build the actual binary
FROM rust:1.75-alpine AS builder
WORKDIR /app
COPY . .
# Copy over the cached dependencies from the cacher stage
COPY --from=cacher /app/target /app/target
COPY --from=cacher /usr/local/cargo /usr/local/cargo

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig binutils

# Build for MUSL (Static linking)
RUN cargo build --release --target x86_64-unknown-linux-musl

# Minify the binary
RUN strip target/x86_64-unknown-linux-musl/release/rust-scraper

# Stage 4: Final minimal runtime
FROM alpine:3.19
RUN apk add --no-cache ca-certificates wget
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/rust-scraper /usr/local/bin/rust-scraper

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rust-scraper"]
