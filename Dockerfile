# Aphelion node.
#
# Two stages: a builder with the full toolchain, and a slim runtime that carries
# only the binary and the Stellar CLI it shells out to for submissions.

FROM rust:1.91-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Manifests first, so a source-only change does not re-download the dependency
# graph on every rebuild.
COPY Cargo.toml Cargo.lock* ./
COPY crates/aphelion-core/Cargo.toml crates/aphelion-core/
COPY crates/aphelion-node/Cargo.toml crates/aphelion-node/
RUN mkdir -p crates/aphelion-core/src crates/aphelion-node/src \
    && echo "fn main() {}" > crates/aphelion-node/src/main.rs \
    && touch crates/aphelion-core/src/lib.rs \
    && cargo build --release --workspace 2>/dev/null || true

COPY crates ./crates
COPY migrations ./migrations
COPY tests ./tests
# Touch so cargo does not reuse the stub artifacts built above.
RUN touch crates/aphelion-core/src/lib.rs crates/aphelion-node/src/main.rs \
    && cargo build --release -p aphelion-node

# ---------------------------------------------------------------------------

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# The node builds and signs transactions by driving the Stellar CLI rather than
# reimplementing XDR assembly, so the runtime image needs it.
ARG STELLAR_CLI_VERSION=23.1.4
RUN curl -sSfL \
      "https://github.com/stellar/stellar-cli/releases/download/v${STELLAR_CLI_VERSION}/stellar-cli-${STELLAR_CLI_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
      | tar -xz -C /usr/local/bin stellar \
    && stellar --version

# Never run as root: this process holds a signing key.
RUN useradd --system --create-home --uid 10001 aphelion

COPY --from=builder /build/target/release/aphelion-node /usr/local/bin/aphelion-node

USER aphelion
WORKDIR /home/aphelion

ENV APHELION_CONFIG=/etc/aphelion/aphelion.toml \
    APHELION_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://localhost:8080/health || exit 1

ENTRYPOINT ["aphelion-node"]
CMD ["run"]
