FROM ghcr.io/casey/just:latest AS just

FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo build --locked --release

FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        ca-certificates \
        git \
        libgcc-s1 \
        tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /source/target/release/pipeline /usr/local/bin/pipeline
COPY --from=just /just /usr/local/bin/just
COPY entrypoint.sh /usr/local/bin/pipeline-action
RUN chmod 0755 /usr/local/bin/pipeline /usr/local/bin/just /usr/local/bin/pipeline-action

# Keep the ordinary image Just-like: arguments after the image name are passed
# directly to Pipeline. action.yml selects the workspace-aware wrapper instead.
ENTRYPOINT ["/usr/bin/tini", "-g", "--", "/usr/local/bin/pipeline"]
