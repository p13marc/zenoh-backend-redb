# Multi-stage Dockerfile for zenoh-backend-redb
# Optimized for size and security

# Build stage
FROM rust:1.75-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /build

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src
COPY examples ./examples
COPY config ./config

# Build release binary
RUN cargo build --release --features plugin

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create zenoh user for security
RUN useradd -m -u 1000 -s /bin/bash zenoh

# Create necessary directories
RUN mkdir -p /var/lib/zenoh/redb \
    /etc/zenoh \
    /usr/local/lib/zenoh/plugins \
    && chown -R zenoh:zenoh /var/lib/zenoh /etc/zenoh

# Copy plugin library from builder
COPY --from=builder /build/target/release/libzenoh_backend_redb.so /usr/local/lib/zenoh/plugins/

# Copy example configuration
COPY --from=builder /build/config/zenoh-redb-example.json5 /etc/zenoh/zenoh.json5

# Set environment variables
ENV ZENOH_BACKEND_REDB_ROOT=/var/lib/zenoh/redb
ENV RUST_LOG=info
ENV RUST_BACKTRACE=1

# Switch to zenoh user
USER zenoh

# Set working directory
WORKDIR /home/zenoh

# Expose Zenoh ports
# 7447 - Default Zenoh port
# 8000 - REST API port (if enabled)
EXPOSE 7447 8000

# Volume for persistent storage
VOLUME ["/var/lib/zenoh/redb"]

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD zenohd --version || exit 1

# Default command
CMD ["zenohd", "-c", "/etc/zenoh/zenoh.json5"]

# Labels
LABEL org.opencontainers.image.title="Zenoh Backend redb"
LABEL org.opencontainers.image.description="Zenoh storage backend using redb embedded database"
LABEL org.opencontainers.image.url="https://github.com/yourusername/zenoh-backend-redb"
LABEL org.opencontainers.image.source="https://github.com/yourusername/zenoh-backend-redb"
LABEL org.opencontainers.image.version="0.1.0"
LABEL org.opencontainers.image.licenses="Apache-2.0 OR EPL-2.0"
