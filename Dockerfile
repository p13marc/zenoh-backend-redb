# Multi-stage Dockerfile for zenoh-backend-redb with zenohd integration testing
# Builds both zenohd and the plugin with the same Zenoh version and compiler

# Build stage - compile zenohd and plugin from source
FROM rust:1.85.0-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    git \
    clang \
    cmake \
    && rm -rf /var/lib/apt/lists/*

# Create build directory
WORKDIR /build

# First, build zenohd from source with the matching version
# This ensures zenohd and plugin use the exact same Zenoh version
ARG ZENOH_VERSION=1.6.2
RUN git clone --depth 1 --branch ${ZENOH_VERSION} https://github.com/eclipse-zenoh/zenoh.git zenoh-src

WORKDIR /build/zenoh-src

# Build zenohd and required plugins
RUN cargo build --release -p zenohd -p zenoh-plugin-rest -p zenoh-plugin-storage-manager

# Install zenohd and plugins to known locations
# Put plugins in /usr/local/lib where zenohd searches by default
RUN cp target/release/zenohd /usr/local/bin/zenohd && \
    cp target/release/libzenoh_plugin_rest.so /usr/local/lib/ && \
    cp target/release/libzenoh_plugin_storage_manager.so /usr/local/lib/

# Now build our plugin
WORKDIR /build/plugin

# Copy manifests first for better caching
COPY Cargo.toml Cargo.lock ./

# Create a dummy src to cache dependencies
RUN mkdir -p src && echo "fn main() {}" > src/lib.rs
RUN cargo build --release --features plugin || true
RUN rm -rf src

# Copy actual source code
COPY src ./src
COPY examples ./examples
COPY config ./config
COPY benches ./benches

# Build the actual plugin
RUN cargo build --release --features plugin

# Verify plugin was built correctly
RUN test -f target/release/libzenoh_backend_redb.so || \
    (echo "ERROR: Plugin library not found!" && exit 1)

# Copy tests for integration testing
COPY tests ./tests

# Build test binaries
RUN cargo test --no-run --test integration_zenohd

# Runtime stage for production use
FROM debian:bookworm-slim as runtime

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
    && chown -R zenoh:zenoh /var/lib/zenoh /etc/zenoh

# Copy zenohd binary from builder
COPY --from=builder /usr/local/bin/zenohd /usr/local/bin/zenohd

# Copy zenoh plugins (rest and storage-manager) from builder to /usr/local/lib
COPY --from=builder /build/zenoh-src/target/release/libzenoh_plugin_rest.so /usr/local/lib/
COPY --from=builder /build/zenoh-src/target/release/libzenoh_plugin_storage_manager.so /usr/local/lib/

# Copy our redb plugin library from builder
COPY --from=builder /build/plugin/target/release/libzenoh_backend_redb.so /usr/local/lib/

# Copy example configuration
COPY --from=builder /build/plugin/config/zenoh-redb-example.json5 /etc/zenoh/zenoh.json5

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
LABEL org.opencontainers.image.version="0.2.0"
LABEL org.opencontainers.image.licenses="Apache-2.0 OR MIT"

# Test stage - includes everything needed to run integration tests
FROM rust:1.85.0-slim as test

# Install runtime and test dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy zenohd binary
COPY --from=builder /usr/local/bin/zenohd /usr/local/bin/zenohd

# Copy zenoh plugins to /usr/local/lib where zenohd searches
COPY --from=builder /build/zenoh-src/target/release/libzenoh_plugin_rest.so /usr/local/lib/
COPY --from=builder /build/zenoh-src/target/release/libzenoh_plugin_storage_manager.so /usr/local/lib/

# Copy plugin source and build artifacts
WORKDIR /app
COPY --from=builder /build/plugin ./

# Ensure zenohd is in PATH and executable
RUN chmod +x /usr/local/bin/zenohd && zenohd --version

# Set environment for testing
ENV RUST_BACKTRACE=1
ENV RUST_LOG=debug

# Run integration tests including zenohd tests
CMD ["cargo", "test", "--test", "integration_zenohd", "--", "--test-threads=1", "--nocapture"]
