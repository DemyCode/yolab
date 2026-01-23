import logging
from socketserver import ThreadingUDPServer

import httpx
from devtools import pprint
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

    def model_post_init(self, __context):
        pprint(self)


settings = DNSServerSettings()  # ty: ignore[missing-argument]


class APIResolver(BaseResolver):
    def __init__(self):
        self.domain_suffix = f".{settings.domain}"
        self.main_ipv6 = settings.frps_server_ipv6
        self.api_url = settings.registration_api_url
        logger.info(f"DNS Resolver initialized for domain: {settings.domain}")
        logger.info(f"Main server IPv6: {self.main_ipv6}")
        logger.info(f"Registration API: {self.api_url}")

    def resolve(self, request, handler):
        reply = request.reply()
        qname = str(request.q.qname).rstrip(".")
        qtype = QTYPE[request.q.qtype]

        logger.info(f"DNS Query: {qname} ({qtype})")

        # Root domain always returns main server IPv6
        if qname == settings.domain:
            if qtype == "AAAA" or qtype == "ANY":
                reply.add_answer(
                    RR(qname, QTYPE.AAAA, rdata=AAAA(self.main_ipv6), ttl=300)
                )
                logger.info(f"Resolved root domain {qname} → {self.main_ipv6}")
            return reply

        # Subdomain resolution via API
        if qname.endswith(self.domain_suffix):
            subdomain = qname[: -len(self.domain_suffix)]

            try:
                # Call Registration API synchronously (dnslib doesn't support async)
                with httpx.Client(timeout=2.0) as client:
                    response = client.get(
                        f"{self.api_url}/internal/dns/resolve/{subdomain}"
                    )

                    if response.status_code == 200:
                        result = response.json()

                        if result.get("found"):
                            ipv6_address = result.get("ipv6_address")
                            service_id = result.get("service_id")

                            if qtype == "AAAA" or qtype == "ANY":
                                reply.add_answer(
                                    RR(
                                        qname,
                                        QTYPE.AAAA,
                                        rdata=AAAA(ipv6_address),
                                        ttl=60,
                                    )
                                )
                                logger.info(
                                    f"Resolved {qname} → {ipv6_address} (service_id: {service_id})"
                                )
                        elif result.get("fallback_to_main"):
                            # Unknown subdomain, return main server
                            if qtype == "AAAA" or qtype == "ANY":
                                reply.add_answer(
                                    RR(
                                        qname,
                                        QTYPE.AAAA,
                                        rdata=AAAA(self.main_ipv6),
                                        ttl=300,
                                    )
                                )
                                logger.info(
                                    f"No service found for {qname}, returning main server: {self.main_ipv6}"
                                )
                    else:
                        logger.error(
                            f"API returned error {response.status_code}, returning main server"
                        )
                        if qtype == "AAAA" or qtype == "ANY":
                            reply.add_answer(
                                RR(
                                    qname,
                                    QTYPE.AAAA,
                                    rdata=AAAA(self.main_ipv6),
                                    ttl=60,
                                )
                            )

            except httpx.TimeoutException:
                logger.error(f"API timeout for {qname}, returning main server")
                if qtype == "AAAA" or qtype == "ANY":
                    reply.add_answer(
                        RR(qname, QTYPE.AAAA, rdata=AAAA(self.main_ipv6), ttl=60)
                    )
            except httpx.RequestError as e:
                logger.error(
                    f"API connection error for {qname}: {e}, returning main server"
                )
                if qtype == "AAAA" or qtype == "ANY":
                    reply.add_answer(
                        RR(qname, QTYPE.AAAA, rdata=AAAA(self.main_ipv6), ttl=60)
                    )
            except Exception as e:
                logger.error(
                    f"Unexpected error for {qname}: {e}, returning main server"
                )
                if qtype == "AAAA" or qtype == "ANY":
                    reply.add_answer(
                        RR(qname, QTYPE.AAAA, rdata=AAAA(self.main_ipv6), ttl=60)
                    )

        return reply


if __name__ == "__main__":
    resolver = APIResolver()
    server = DNSServer(resolver, port=53, address="::", server=ThreadingUDPServer)

    logger.info("Starting DNS server on [::]:53 (IPv6)...")
    logger.info(f"Resolving *.{settings.domain} via Registration API")

    try:
        server.start()
    except KeyboardInterrupt:
        logger.info("Shutting down DNS server...")
        server.stop()
    except PermissionError:
        logger.error(
            "Permission denied: Port 53 requires root privileges. Run with sudo."
        )
    except Exception as e:
        logger.error(f"DNS server error: {e}")
