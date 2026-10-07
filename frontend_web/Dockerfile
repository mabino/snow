# Snow web frontend + snow-bridge
#
# Build from the repository root:
#
#   docker build -f frontend_web/Dockerfile -t snow-web .                 # runtime image
#   docker run --rm -p 127.0.0.1:8080:8080 snow-web                        # http://127.0.0.1:8080/
#
#   docker build -f frontend_web/Dockerfile --target test -t snow-web-test .
#   docker run --rm snow-web-test                                          # full test suite
#   docker run --rm -v /path/to/media:/media -e SNOW_ROM=/media/Mac-SE.rom \
#       -e SNOW_DISK=/media/System6.dsk snow-web-test                       # + AppleTalk e2e
#
# The LocalTalk LAN relay (`--lan`, LToUDP multicast) needs host networking
# (`docker run --network host`); without it, browser tabs in the same room
# still network with each other.

ARG RUST_VERSION=1.95.0
ARG EMSDK_VERSION=4.0.22

FROM rust:${RUST_VERSION}-bookworm AS build
ARG EMSDK_VERSION
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 https://github.com/emscripten-core/emsdk.git /emsdk \
    && /emsdk/emsdk install ${EMSDK_VERSION} \
    && /emsdk/emsdk activate ${EMSDK_VERSION}
# (emsdk_env.sh needs bash; set the environment it would set directly)
ENV EMSDK=/emsdk PATH=/emsdk:/emsdk/upstream/emscripten:$PATH
RUN rustup target add wasm32-unknown-emscripten && emcc --version
WORKDIR /src
COPY . .
RUN cargo build --release -p snow_bridge
RUN cargo build --release -p snow_frontend_web --bin snow_web --target wasm32-unknown-emscripten \
    && cp target/wasm32-unknown-emscripten/release/snow_web.js \
          target/wasm32-unknown-emscripten/release/snow_web.wasm frontend_web/www/

FROM build AS test
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src/frontend_web/tests
RUN npm ci && npx playwright install --with-deps chromium-headless-shell
WORKDIR /src
CMD ["sh", "frontend_web/tests/run-all.sh"]

FROM debian:bookworm-slim AS runtime
COPY --from=build /src/target/release/snow_bridge /usr/local/bin/snow-bridge
COPY --from=build /src/frontend_web/www /srv/www
EXPOSE 8080
# Inside a container the bridge must listen on all interfaces to be
# reachable through the published port; everything else keeps the safe
# defaults (no LAN relay, no Ethernet/NAT, same-origin only). Add options
# after the image name, e.g. `docker run ... snow-web --ethernet`.
ENTRYPOINT ["snow-bridge", "--www", "/srv/www", "--addr", "0.0.0.0", "--port", "8080"]
