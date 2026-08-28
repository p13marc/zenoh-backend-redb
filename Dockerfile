# Multi-stage Dockerfile for zenoh-backend-redb with zenohd integration testing
# Builds both zenohd and the plugin with the same Zenoh version and compiler

# Build stage - compile zenohd and plugin from source
FROM rust:1.97-slim as builder

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
ARG ZENOH_VERSION=1.10.0
RUN git clone --depth 1 --branch ${ZENOH_VERSION} https://github.com/eclipse-zenoh/zenoh.git zenoh-src

WORKDIR /build/zenoh-src

# Build zenohd and required plugins
RUN cargo build --release -p zenohd -p zenoh-plugin-rest -p zenoh-plugin-storage-manager

# Install zenohd and plugins to known locations
# Put plugins in /usr/local/lib where zenohd searches by default
RUN cp target/release/zenohd /usr/local/bin/zenohd && \
    cp target/release/libzenoh_plugin_rest.so /usr/local/lib/ && \
    cp target/release/libzenoh_plugin_storage_manager.so /usr/local/lib/

# Build our plugin as a REAL member of the zenoh workspace.
#
# Copying the crate into the tree is NOT enough. Carrying its own Cargo.lock makes
# cargo treat it as a separate workspace with its own resolution and its own
# target/ — which is precisely the configuration that produces "Incompatible Zenoh
# feature sets": zenoh-plugin-storage-manager takes zenoh_backend_traits with
# default-features = false, and two independent resolutions can disagree on the
# compiled feature string. zenohd then loads, logs one ERROR line, and serves no
# storage.
#
# Registering the crate as a workspace member AND patching crates.io to the local
# zenoh sources makes the storage manager and this backend resolve one feature set
# from one lockfile, against the very code zenohd above was built from.
WORKDIR /build/zenoh-src

RUN mkdir -p zenoh-backend-redb
COPY Cargo.toml zenoh-backend-redb/
COPY src zenoh-backend-redb/src
COPY examples zenoh-backend-redb/examples
COPY config zenoh-backend-redb/config
COPY benches zenoh-backend-redb/benches
COPY tests zenoh-backend-redb/tests

# Deliberately NOT copying our Cargo.lock: the workspace lockfile governs.
RUN sed -i 's|^members = \[|members = [\n  "zenoh-backend-redb",|' Cargo.toml && \
    printf '\n[patch.crates-io]\n\
zenoh = { path = "zenoh" }\n\
zenoh-plugin-trait = { path = "plugins/zenoh-plugin-trait" }\n\
zenoh_backend_traits = { path = "plugins/zenoh-backend-traits" }\n\
zenoh-util = { path = "commons/zenoh-util" }\n\
zenoh-ext = { path = "zenoh-ext" }\n' >> Cargo.toml && \
    grep -A 3 '^members' Cargo.toml && tail -8 Cargo.toml

# Build from the workspace root so the member (and the patch table) apply.
RUN cargo build --release -p zenoh-backend-redb --features plugin

# Verify plugin was built correctly
RUN test -f target/release/libzenoh_backend_redb.so || \
    (echo "ERROR: Plugin library not found!" && exit 1)

# Build test binaries
RUN cargo test --no-run -p zenoh-backend-redb --test integration_zenohd

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
# Copy zenoh plugins to /usr/local/lib where zenohd searches
COPY --from=builder /build/zenoh-src/target/release/libzenoh_plugin_rest.so /usr/local/lib/
COPY --from=builder /build/zenoh-src/target/release/libzenoh_plugin_storage_manager.so /usr/local/lib/

# Copy our redb plugin library from builder
COPY --from=builder /build/zenoh-src/target/release/libzenoh_backend_redb.so /usr/local/lib/

# Copy example configuration
COPY --from=builder /build/zenoh-src/zenoh-backend-redb/config/zenoh-redb-example.json5 /etc/zenoh/zenoh.json5

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
LABEL org.opencontainers.image.url="https://git.marcpardo.eu/marcpardo/zenoh-backend-redb"
LABEL org.opencontainers.image.source="https://git.marcpardo.eu/marcpardo/zenoh-backend-redb"
LABEL org.opencontainers.image.version="0.4.0"
LABEL org.opencontainers.image.licenses="Apache-2.0 OR MIT"

# Test stage - includes everything needed to run integration tests
FROM rust:1.97-slim as test

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

# Carry the whole zenoh workspace across, not just our crate: the lockfile, the
# `[patch.crates-io]` table and the compiled target/ all live at the workspace
# root now. Copying the member alone would make cargo re-resolve against
# crates.io and rebuild a plugin that no longer matches the zenohd beside it.
WORKDIR /app
COPY --from=builder /build/zenoh-src ./

# Ensure zenohd is in PATH and executable
RUN chmod +x /usr/local/bin/zenohd && zenohd --version

# Set environment for testing
ENV RUST_BACKTRACE=1
ENV RUST_LOG=debug

# Run integration tests including zenohd tests
CMD ["cargo", "test", "-p", "zenoh-backend-redb", "--test", "integration_zenohd", "--", "--test-threads=1", "--nocapture"]
