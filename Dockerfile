# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Build args (override with --build-arg)
# ---------------------------------------------------------------------------
ARG RUST_VERSION=1.99
ARG KICAD_9_TAG=9.0.9
ARG KICAD_10_TAG=10.0.8
# Set to 1 to also bundle the stock KiCad 3D models (kicad-packages3D) under
# /opt/kicad/<ver>/3dmodels. WARNING: this adds SEVERAL GB to the image and
# makes the build very slow. Off by default.
ARG WITH_STOCK_3D=0

# ---------------------------------------------------------------------------
# Stage 1: static musl release binary, with dependency-layer caching
# ---------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
ENV CARGO_TERM_COLOR=never
# Dependencies only: dummy crate sources, so this layer stays cached until
# Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked --target x86_64-unknown-linux-musl \
    && rm -rf src \
       target/x86_64-unknown-linux-musl/release/construct-feeder* \
       target/x86_64-unknown-linux-musl/release/deps/construct_feeder* \
       target/x86_64-unknown-linux-musl/release/deps/libconstruct_feeder* \
       target/x86_64-unknown-linux-musl/release/.fingerprint/construct-feeder*
COPY src ./src
RUN cargo build --release --locked --target x86_64-unknown-linux-musl \
    && cp target/x86_64-unknown-linux-musl/release/construct-feeder /construct-feeder

# ---------------------------------------------------------------------------
# Stage 2/3: stock KiCad libraries (symbols + footprints only, no VCS data)
# ---------------------------------------------------------------------------
FROM alpine/git:latest AS libs9
ARG KICAD_9_TAG
ARG WITH_STOCK_3D
RUN set -eu; \
    mkdir -p /out/symbols /out/footprints; \
    git clone --depth 1 --branch "${KICAD_9_TAG}" https://gitlab.com/kicad/libraries/kicad-symbols.git /tmp/sym; \
    git clone --depth 1 --branch "${KICAD_9_TAG}" https://gitlab.com/kicad/libraries/kicad-footprints.git /tmp/fp; \
    find /tmp/sym -maxdepth 1 \( -name '*.kicad_sym' -o -name '*.kicad_symdir' \) -exec mv {} /out/symbols/ \; ; \
    # KiCad 10 ships symbols as <Lib>.kicad_symdir/ (one file per symbol); fail loudly if empty.
    { [ -e /out/symbols/Device.kicad_sym ] || [ -d /out/symbols/Device.kicad_symdir ]; } || { echo 'no Device symbol library'; exit 1; }; \
    find /tmp/fp -maxdepth 1 -name '*.pretty' -type d -exec mv {} /out/footprints/ \; ; \
    [ -d /out/footprints/Resistor_SMD.pretty ] || { echo 'no footprint libraries'; exit 1; }; \
    if [ "${WITH_STOCK_3D}" = "1" ]; then \
      git clone --depth 1 --branch "${KICAD_9_TAG}" https://gitlab.com/kicad/libraries/kicad-packages3D.git /tmp/3d; \
      mkdir -p /out/3dmodels; \
      find /tmp/3d -maxdepth 1 -name '*.3dshapes' -type d -exec mv {} /out/3dmodels/ \; ; \
    fi; \
    rm -rf /tmp/sym /tmp/fp /tmp/3d

FROM alpine/git:latest AS libs10
ARG KICAD_10_TAG
ARG WITH_STOCK_3D
RUN set -eu; \
    mkdir -p /out/symbols /out/footprints; \
    git clone --depth 1 --branch "${KICAD_10_TAG}" https://gitlab.com/kicad/libraries/kicad-symbols.git /tmp/sym; \
    git clone --depth 1 --branch "${KICAD_10_TAG}" https://gitlab.com/kicad/libraries/kicad-footprints.git /tmp/fp; \
    find /tmp/sym -maxdepth 1 \( -name '*.kicad_sym' -o -name '*.kicad_symdir' \) -exec mv {} /out/symbols/ \; ; \
    # KiCad 10 ships symbols as <Lib>.kicad_symdir/ (one file per symbol); fail loudly if empty.
    { [ -e /out/symbols/Device.kicad_sym ] || [ -d /out/symbols/Device.kicad_symdir ]; } || { echo 'no Device symbol library'; exit 1; }; \
    find /tmp/fp -maxdepth 1 -name '*.pretty' -type d -exec mv {} /out/footprints/ \; ; \
    [ -d /out/footprints/Resistor_SMD.pretty ] || { echo 'no footprint libraries'; exit 1; }; \
    if [ "${WITH_STOCK_3D}" = "1" ]; then \
      git clone --depth 1 --branch "${KICAD_10_TAG}" https://gitlab.com/kicad/libraries/kicad-packages3D.git /tmp/3d; \
      mkdir -p /out/3dmodels; \
      find /tmp/3d -maxdepth 1 -name '*.3dshapes' -type d -exec mv {} /out/3dmodels/ \; ; \
    fi; \
    rm -rf /tmp/sym /tmp/fp /tmp/3d

# ---------------------------------------------------------------------------
# Final image
# ---------------------------------------------------------------------------
FROM debian:stable-slim
ARG KICAD_9_TAG
ARG KICAD_10_TAG
LABEL org.opencontainers.image.title="construct-feeder" \
      org.opencontainers.image.description="Finds custom KiCad 9/10 components in a repository and exports them as a zip" \
      org.opencontainers.image.source="https://github.com/Alacrity-Education/ConstructFeeder" \
      io.constructfeeder.kicad-9-tag="${KICAD_9_TAG}" \
      io.constructfeeder.kicad-10-tag="${KICAD_10_TAG}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && git config --system --add safe.directory '*'

COPY --from=libs9  /out/ /opt/kicad/9/
COPY --from=libs10 /out/ /opt/kicad/10/
COPY --from=build  /construct-feeder /usr/local/bin/construct-feeder

ENV CONSTRUCT_FEEDER_STOCK_9=/opt/kicad/9 \
    CONSTRUCT_FEEDER_STOCK_10=/opt/kicad/10

WORKDIR /github/workspace
ENTRYPOINT ["construct-feeder"]
