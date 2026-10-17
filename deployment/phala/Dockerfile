FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS egress-build
# Pin and verify the Go toolchain as well as Xray's complete module graph.
RUN curl -fsSLo /tmp/go.tar.gz https://dl.google.com/go/go1.27.2.linux-amd64.tar.gz && \
    echo "ecbadb99091a3f46e31f5f934b068b1864eafa7995211b39eaddf76996045fe5  /tmp/go.tar.gz" | sha256sum -c - && \
    tar -C /usr/local -xzf /tmp/go.tar.gz && rm /tmp/go.tar.gz
ENV PATH=/usr/local/go/bin:${PATH} GOTOOLCHAIN=local CGO_ENABLED=0
WORKDIR /egress
COPY egress/go.mod egress/go.sum ./
RUN go mod download
COPY egress ./
RUN go build -mod=readonly -trimpath -ldflags="-s -w" -o /teerouter-egress . && \
    go mod vendor && mkdir -p /notices && \
    find vendor -type f \( -iname 'license*' -o -iname 'copying*' -o -iname 'notice*' -o -iname 'authors*' -o -iname 'patents*' -o -name modules.txt \) -exec cp --parents {} /notices \; && \
    cp /usr/local/go/LICENSE /notices/Go-LICENSE && \
    cp go.mod go.sum /notices/

FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests/memory_cli.rs ./tests/memory_cli.rs
ARG ROUTER_FEATURES=dstack-tdx
ARG TEEROUTER_WORKBUDDY_BASE_URL=http://127.0.0.1:8001/v1
ENV TEEROUTER_WORKBUDDY_BASE_URL=${TEEROUTER_WORKBUDDY_BASE_URL}
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
COPY --from=egress-build /teerouter-egress /usr/local/bin/teerouter-egress
COPY --from=egress-build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=egress-build /notices/ /usr/share/doc/teerouter-egress/
COPY --from=build /usr/share/doc/libc6/copyright /usr/share/doc/libc6/copyright
COPY --from=build /usr/share/doc/libgcc-s1/copyright /usr/share/doc/libgcc-s1/copyright
COPY LICENSE NOTICE /usr/share/doc/teellm/
# Root is required by dstack's guest-agent socket on some production releases.
# The compose removes all capabilities; there is no Docker socket or host mount.
USER 0:0
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/teellm"]
CMD ["--admin-key-env", "--bind", "127.0.0.1:4000", "--tls-bind", "0.0.0.0:8443"]
