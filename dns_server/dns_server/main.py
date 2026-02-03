import logging
from socketserver import ThreadingUDPServer

import httpx
from dnslib import AAAA, QTYPE, RR
from dnslib.server import BaseResolver, DNSServer
from pydantic_settings import BaseSettings, SettingsConfigDict

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


class DNSServerSettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
        cli_parse_args=True,
    )

    registration_api_url: str
    domain: str
    frps_server_ipv6: str


settings = DNSServerSettings()


class APIResolver(BaseResolver):
    def __init__(self):
        self.domain_suffix = f".{settings.domain}"
        self.main_ipv6 = settings.frps_server_ipv6
        self.api_url = settings.registration_api_url
        logger.info(f"DNS Resolver: {settings.domain} -> {self.api_url}")

    def resolve(self, request, handler):
        reply = request.reply()
        qname = str(request.q.qname).rstrip(".")
        qtype = QTYPE[request.q.qtype]

        if qtype not in ("AAAA", "ANY"):
            return reply

        if qname == settings.domain:
            reply.add_answer(RR(qname, QTYPE.AAAA, rdata=AAAA(self.main_ipv6), ttl=300))
            logger.info(f"{qname} -> {self.main_ipv6}")
            return reply

        if qname.endswith(self.domain_suffix):
            subdomain = qname[: -len(self.domain_suffix)]

            try:
                with httpx.Client(timeout=2.0) as client:
                    response = client.get(f"{self.api_url}/dns/resolve/{subdomain}")

                    if response.status_code == 200:
                        result = response.json()
                        if result.get("found"):
                            ipv6_address = result.get("ipv6_address")
                            reply.add_answer(
                                RR(qname, QTYPE.AAAA, rdata=AAAA(ipv6_address), ttl=60)
                            )
                            logger.info(f"{qname} -> {ipv6_address}")
                        else:
                            logger.info(f"{qname} -> NXDOMAIN")
                    else:
                        logger.error(f"{qname} -> API error {response.status_code}")
            except Exception as e:
                logger.error(f"{qname} -> {e}")

        return reply


if __name__ == "__main__":
    resolver = APIResolver()
    server = DNSServer(resolver, port=53, address="0.0.0.0", server=ThreadingUDPServer)

    logger.info("Starting DNS server on 0.0.0.0:53")

    try:
        server.start()
    except KeyboardInterrupt:
        logger.info("Shutting down DNS server...")
        server.stop()
    except PermissionError:
        logger.error("Port 53 requires root privileges")
    except Exception as e:
        logger.error(f"DNS server error: {e}")
