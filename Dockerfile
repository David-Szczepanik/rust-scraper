# Stage 1: Build the binary
FROM rust:1.83-alpine AS builder
WORKDIR /app

# Install build dependencies
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig binutils curl

# Copy source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build for the native target (supports both amd64 and arm64)
RUN cargo build --release

# Strip the binary to minimize size
RUN strip target/release/rust-scraper

# Stage 2: Final minimal runtime
FROM alpine:3.19
RUN apk add --no-cache ca-certificates curl
COPY --from=builder /app/target/release/rust-scraper /usr/local/bin/rust-scraper

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rust-scraper"]
