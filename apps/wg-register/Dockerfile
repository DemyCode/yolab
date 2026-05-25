FROM alpine:3.21
RUN apk add --no-cache wireguard-tools curl jq
COPY setup.sh /setup.sh
RUN chmod +x /setup.sh
ENTRYPOINT ["/bin/sh", "/setup.sh"]
