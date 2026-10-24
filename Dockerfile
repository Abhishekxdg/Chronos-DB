# A container running `chronos serve`: Postgres protocol on 5432, HTTP API on 7070, the database
# in the volume /data, as an unprivileged user.
#
#   docker build -t chronos .
#   export CHRONOS_TOKEN=$(openssl rand -hex 24) CHRONOS_ADMIN_TOKEN=$(openssl rand -hex 24)
#   docker run -d --name chronos -e CHRONOS_TOKEN -e CHRONOS_ADMIN_TOKEN -p 127.0.0.1:5432:5432 -p 127.0.0.1:7070:7070 -v chronos-data:/data chronos
#   psql "postgres://app:$CHRONOS_ADMIN_TOKEN@127.0.0.1:5432/main"   # the database's own user
#   psql "postgres://bot:$CHRONOS_TOKEN@127.0.0.1:5432/main"         # the guest: forks and changes its own worlds
#
# CHRONOS_TOKEN is required: the server listens on 0.0.0.0 inside the container, and `chronos serve`
# refuses to start on an address that isn't loopback without one, or with one shorter than 24
# characters. It is the Postgres password (any user name) and the HTTP bearer token that lets a
# client in as the guest; CHRONOS_ADMIN_TOKEN is the database's own user, for you and the application
# that owns the data (keep it away from agents). Pass both in the environment (`-e`, which takes them
# from yours), never on a command line. The ports are
# published on the host's loopback only, since the default command runs without TLS
# (--insecure-no-tls: the container's own network is the only one it crosses). To serve other
# machines, mount a certificate and key and run
#   serve /data --pg 0.0.0.0:5432 --listen 0.0.0.0:7070 --tls-cert /certs/cert.pem --tls-key /certs/key.pem
# instead. Stop with `docker stop` (SIGTERM closes the database cleanly).
#
# Base images are pinned by version. TODO(owner): pin them by digest too (docker buildx imagetools
# inspect <image>) before publishing an image, as Cross.toml pins the release build's images.

# --- the Studio's web UI, embedded in the binary by build.rs (without it a placeholder page stands in)
FROM node:22.20.0-bookworm-slim AS studio
WORKDIR /src/studio
COPY studio/package.json studio/package-lock.json ./
RUN npm ci
COPY studio/ ./
RUN npm run build

# --- the binary
FROM rust:1.95.0-slim-bookworm AS build
# --build-arg JOBS=8 to build on fewer cores than the machine has
ARG JOBS
WORKDIR /src
COPY . .
COPY --from=studio /src/studio/dist studio/dist
RUN CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked ${JOBS:+-j "$JOBS"}

# --- what runs: Debian slim (glibc), tzdata for named time zones, CA certificates for
# `chronos import` over TLS with sslmode=verify-full
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /data --no-create-home --shell /usr/sbin/nologin chronos \
    && mkdir /data && chown chronos:chronos /data && chmod 0700 /data
COPY --from=build /src/target/release/chronos /usr/local/bin/chronos
USER chronos:chronos
VOLUME ["/data"]
EXPOSE 5432 7070
ENTRYPOINT ["chronos"]
CMD ["serve", "/data", "--pg", "0.0.0.0:5432", "--listen", "0.0.0.0:7070", "--insecure-no-tls"]
