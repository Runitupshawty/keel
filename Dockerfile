# Build the browser bundle first: keel-daemon embeds it at compile time.
FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN rustup target add wasm32-unknown-unknown \
    && cargo install --locked wasm-bindgen-cli --version "$(cargo pkgid wasm-bindgen | sed 's/.*@//')" \
    && bash scripts/build-web.sh \
    && cargo build --release --locked -p keel-daemon

FROM debian:bookworm-slim
# keel-daemon refuses a non-loopback --web (plain http) unless the profile's config.toml
# accepts it; inside a container 0.0.0.0 is the only bind a published port reaches, and
# `docker run -p` decides who can connect. A new named volume starts with this file.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home keel \
    && mkdir -p /var/lib/keel/data /var/lib/keel/config/profiles/default \
    && printf '[web]\nallow_plaintext = true\n' > /var/lib/keel/config/profiles/default/config.toml \
    && chown -R keel:keel /var/lib/keel
COPY --from=build /build/target/release/keel-daemon /usr/local/bin/keel-daemon
ENV KEEL_DATA_DIR=/var/lib/keel/data \
    KEEL_CONFIG_DIR=/var/lib/keel/config \
    RUST_LOG=info
USER keel
VOLUME ["/var/lib/keel"]
EXPOSE 7421
ENTRYPOINT ["keel-daemon"]
CMD ["--web", "0.0.0.0:7421", "--ws-allow-remote", "--web-host", "localhost", "--web-host", "127.0.0.1"]
