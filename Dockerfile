# syntax=docker/dockerfile:1

FROM node:24-bookworm-slim AS web
WORKDIR /src/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm exec tsc -b && pnpm exec vite build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY --from=web /src/web/dist ./web/dist
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app tavily \
    && mkdir -p /app/data && chown tavily /app/data
WORKDIR /app
COPY --from=build /src/target/release/tavily-lb /usr/local/bin/tavily-lb
USER tavily
ENV LISTEN_ADDR=0.0.0.0:8080 DATA_FILE=/app/data/state.json
EXPOSE 8080
VOLUME ["/app/data"]
ENTRYPOINT ["tavily-lb"]
