# Stage 1: build. lnurlcash-kernel compiles Bitcoin Core's kernel from source
# (CMake, a C++20 compiler, Boost headers), which only this stage carries.
#
# Base images are pinned by digest, not just tag: a moved tag would bake
# whatever it points at into every deployer's binary unnoticed. Update
# deliberately: `docker pull rust:1-trixie` / `docker pull debian:trixie-slim`,
# then swap in the digest each reports.
FROM rust:1-trixie@sha256:5d05167b28cef0fa3a6c781cd77949386848191f3382e82cf53bd1277a47a98f AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake ninja-build libboost-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
# the cache mounts keep the registry and the (slow) kernel build between
# image builds; the binary is copied out of the cached target dir
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --profile optimized \
    && cp target/optimized/lnurl-mint target/optimized/lnurl-mint-cli /usr/local/bin/


# Stage 2: runtime. The binary links Bitcoin Core statically; libc and the
# C++ runtime, both in the slim image already, are all it needs.
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

COPY --from=builder /usr/local/bin/lnurl-mint /usr/local/bin/lnurl-mint-cli /usr/local/bin/

# non-root: a compromise of this process gains this UID, not root
RUN groupadd --gid 1000 mint \
    && useradd --uid 1000 --gid mint --no-create-home --shell /usr/sbin/nologin mint \
    && mkdir /data && chown mint:mint /data
USER mint

# the seed, the node's channel state, the wallet and the notes: mount a
# volume here and back it up as a whole (see the README)
VOLUME /data
ENV DATA_DIR=/data \
    LISTEN=0.0.0.0:8111 \
    LN_LISTEN=0.0.0.0:9735
# the admin API stays on the container's loopback unless ADMIN_LISTEN says
# otherwise; with --network host that is the host's loopback
EXPOSE 8111 9735

# SIGTERM stops the node gracefully, writing its channel state - give it time
# (`docker stop -t 60`), a SIGKILL can force-close a channel
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/lnurl-mint"]
