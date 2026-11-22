FROM --platform=$BUILDPLATFORM caddy:2.11.9-builder@sha256:f9beaa5ddbe7df6f26ebbcf9cbf44e05a1917b9bba464f0c3229038d7e2e6e13 AS build
ARG TARGETOS
ARG TARGETARCH
RUN CGO_ENABLED=0 GOOS=$TARGETOS GOARCH=$TARGETARCH \
    xcaddy build --with github.com/caddy-dns/acmedns@v0.7.0

FROM caddy:2.11.9@sha256:4851e54a46b206517c574821bb5cc67f170d2156041e9fbc4c9193b45e84a75b
COPY --from=build /usr/bin/caddy /usr/bin/caddy
RUN caddy list-modules > /tmp/modules \
 && grep -qx dns.providers.acmedns /tmp/modules \
 && rm /tmp/modules
