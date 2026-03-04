FROM alpine:3.21

RUN apk add --no-cache wireguard-tools iproute2

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
