FROM alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf

SHELL ["/bin/ash", "-eo", "pipefail", "-c"]

ARG TARGETARCH
ARG RADICLE_VERSION=1.10.3
ARG HTTPD_VERSION=0.30.0

WORKDIR /tmp

RUN apk add --no-cache git jq ca-certificates \
    && apk add --no-cache --virtual .fetch curl xz \
    && case "$TARGETARCH" in \
        amd64) arch=x86_64 \
            radicle_sha=3d78e3ffc17f9e6eca7fcd3acc439f2252060532eb457cf17f2747fed4cc6751 \
            httpd_sha=dd16f6f966e62fafee5a6547d1e31c8857c607c5d955380872c9e7a06500285f ;; \
        arm64) arch=aarch64 \
            radicle_sha=822c1ea1da7dd3a2a7f98da6d93b95359b5b9b0e302b8535bea377a2441e596f \
            httpd_sha=ab6ab9f073f9ebc58b3132f9686b65d882250dc434fb4fea32806481f665dd80 ;; \
        *) echo "no Radicle release for $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o radicle.tar.xz "https://files.radicle.dev/releases/${RADICLE_VERSION}/radicle-${RADICLE_VERSION}-${arch}-unknown-linux-musl.tar.xz" \
    && curl -fsSL -o httpd.tar.xz "https://files.radicle.dev/releases/radicle-httpd/${HTTPD_VERSION}/radicle-httpd-${HTTPD_VERSION}-${arch}-unknown-linux-musl.tar.xz" \
    && echo "${radicle_sha}  radicle.tar.xz" | sha256sum -c - \
    && echo "${httpd_sha}  httpd.tar.xz" | sha256sum -c - \
    && tar -xJf radicle.tar.xz --strip-components=1 -C /usr/local \
    && tar -xJf httpd.tar.xz --strip-components=1 -C /usr/local \
    && rm -f radicle.tar.xz httpd.tar.xz \
    && apk del .fetch \
    && rad --version \
    && radicle-httpd --version

ENV RAD_HOME=/radicle
