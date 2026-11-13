FROM alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
RUN apk add --no-cache wireguard-tools iproute2 nftables
COPY entrypoint.sh /entrypoint.sh
COPY firewall.sh /firewall.sh
RUN chmod +x /entrypoint.sh /firewall.sh
ENTRYPOINT ["/entrypoint.sh"]
