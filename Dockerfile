ARG RUST_IMAGE=rust:1.99.0-alpine
ARG BINARY_STAGE=build
FROM ${RUST_IMAGE} AS build
WORKDIR /build
ARG RUST_TARGET=x86_64-unknown-linux-musl
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples/demo.mp4 ./examples/demo.mp4
COPY deploy-compose.yaml ./deploy-compose.yaml
RUN cargo build --release --offline --locked --target "$RUST_TARGET" \
    && cp "target/$RUST_TARGET/release/mynou" /mynou

FROM scratch AS prebuilt
COPY .ci-image/mynou /mynou

FROM ${BINARY_STAGE} AS binary

FROM ${RUST_IMAGE} AS trust

FROM scratch
COPY --from=binary /mynou /mynou
# This bundle contains trust data, not a library or executable.
COPY --from=trust /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
USER 1000:1000
WORKDIR /data
ENV MYNOU_CA_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 8787/tcp 6881/tcp
ENTRYPOINT ["/mynou"]
CMD ["serve", "--config", "/config/mynou.json"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/mynou", "healthcheck", "--config", "/config/mynou.json"]
