FROM scratch
ARG MYNOU_VERSION
ARG MYNOU_REVISION
LABEL org.opencontainers.image.source="https://github.com/alecerf/mynou" \
      org.opencontainers.image.version="$MYNOU_VERSION" \
      org.opencontainers.image.revision="$MYNOU_REVISION"
COPY .ci-image/mynou /mynou
# This bundle contains trust data, not a library or executable.
COPY .ci-image/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
USER 1000:1000
WORKDIR /data
ENV MYNOU_CA_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 8787/tcp 6881/tcp
ENTRYPOINT ["/mynou"]
CMD ["serve", "--config", "/config/mynou.json"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/mynou", "healthcheck", "--config", "/config/mynou.json"]
