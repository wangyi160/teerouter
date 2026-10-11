FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests/memory_cli.rs ./tests/memory_cli.rs
ARG ROUTER_FEATURES=dstack-tdx
RUN cargo build --release --locked --no-default-features --features ${ROUTER_FEATURES} -j 2 && \
    cargo test --release --locked --no-default-features --features ${ROUTER_FEATURES} --test memory_cli -j 2
# Copy only the router and its ELF runtime; no shell, package manager or SSH.
RUN mkdir -p /runtime/usr/local/bin /runtime/var/run && \
    cp target/release/teellm /runtime/usr/local/bin/teellm && \
    ldd target/release/teellm | awk '/=> \// { print $3 } /^\s*\// { print $1 }' | \
    while IFS= read -r lib; do cp --parents "$lib" /runtime; done && \
    cp --parents /lib64/ld-linux-x86-64.so.2 /runtime

FROM scratch
COPY --from=build /runtime/ /
COPY --from=build /usr/share/doc/libc6/copyright /usr/share/doc/libc6/copyright
COPY --from=build /usr/share/doc/libgcc-s1/copyright /usr/share/doc/libgcc-s1/copyright
COPY LICENSE NOTICE /usr/share/doc/teellm/
COPY LICENSES /usr/share/doc/teellm/LICENSES
# Root is required by dstack's guest-agent socket on some production releases.
# The compose removes all capabilities; there is no Docker socket or host mount.
USER 0:0
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/teellm"]
CMD ["--admin-key-env", "--bind", "127.0.0.1:4000", "--tls-bind", "0.0.0.0:8443"]
