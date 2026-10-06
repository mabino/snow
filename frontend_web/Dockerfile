# Snow web frontend + snow-bridge
#
# Build from the repository root:
#
#   docker build -f frontend_web/Dockerfile -t snow-web .                 # runtime image
#   docker run --rm -p 8080:8080 snow-web                                  # http://localhost:8080/
#
#   docker build -f frontend_web/Dockerfile --target test -t snow-web-test .
#   docker run --rm snow-web-test                                          # full test suite
#   docker run --rm -v /path/to/media:/media -e SNOW_ROM=/media/Mac-SE.rom \
#       -e SNOW_DISK=/media/System6.dsk snow-web-test                       # + AppleTalk e2e
#
# The LocalTalk LAN relay (LToUDP multicast) needs host networking
# (`docker run --network host`); without it, browser tabs connected to the
# same bridge still network with each other.

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
ENTRYPOINT ["snow-bridge", "--www", "/srv/www"]
CMD ["--port", "8080"]
