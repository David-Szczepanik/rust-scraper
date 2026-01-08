# Stage 1: Build the binary
FROM rust:1.83-alpine AS builder
WORKDIR /app

# Install build dependencies
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig binutils curl

# Create a dummy project and build dependencies to cache them
RUN mkdir src && echo "fn main() {}" > src/main.rs
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release

# Remove the dummy source and copy the actual source code
RUN rm -rf src
COPY src ./src

# Touch the main file to ensure cargo rebuilds it
RUN touch src/main.rs

# Build the actual application
RUN cargo build --release

# Strip the binary to minimize size
RUN strip target/release/rust-scraper

# Stage 2: Final minimal runtime
FROM alpine:3.19
RUN apk add --no-cache ca-certificates curl
COPY --from=builder /app/target/release/rust-scraper /usr/local/bin/rust-scraper

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rust-scraper"]
