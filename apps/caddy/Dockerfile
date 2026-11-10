FROM --platform=$BUILDPLATFORM caddy:2.11.9-builder@sha256:3fddbec729442a56b942d4c9eaef1df80893dbf2cf5b86701bfae6aad1408eeb AS build
ARG TARGETOS
ARG TARGETARCH
RUN CGO_ENABLED=0 GOOS=$TARGETOS GOARCH=$TARGETARCH \
    xcaddy build --with github.com/caddy-dns/acmedns@v0.7.0

FROM caddy:2.11.9@sha256:0c2243efcf437ab7f7a978f1027fcf0d4f9be8a75df27b1b3fbc5dbe0ffa8cf0
COPY --from=build /usr/bin/caddy /usr/bin/caddy
RUN caddy list-modules > /tmp/modules \
 && grep -qx dns.providers.acmedns /tmp/modules \
 && rm /tmp/modules
