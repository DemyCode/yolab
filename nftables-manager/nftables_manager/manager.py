import subprocess
import time
import hashlib
from pathlib import Path
from typing import List

import requests

from pydantic_settings import BaseSettings, SettingsConfigDict
from devtools import pprint
import httpx


class EnvironmentSettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=str(Path(__file__).parent.parent / ".env"),
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
    )

    backend_url: str
    poll_interval: int
    nftables_file: str  # We'll repurpose this as haproxy_config_file
    log_level: str

    def model_post_init(self, __context):
        pprint(self)


settings = EnvironmentSettings()


def fetch_rules() -> List[dict]:
    response = httpx.get(f"http://{settings.backend_url}/services")
    return response.json()


# Store last config hash to detect changes
last_config_hash = None


def generate_haproxy_config(services: list, server_ipv4: str = "127.0.0.1") -> str:
    config = """global
    log /dev/log local0
    log /dev/log local1 notice
    stats socket /run/haproxy/admin.sock mode 660 level admin
    stats timeout 30s

defaults
    log global
    mode tcp
    option tcplog
    option socket-stats
    timeout connect 5000ms
    timeout client 50000ms
    timeout server 50000ms
    option dontlognull

"""

    for service in services:
        sid = service.get("service_id", 0)
        ipv6 = service["sub_ipv6"]
        port = service["client_port"]
        protocol = service["protocol"].lower()
        internal_port = service["frps_internal_port"]

        config += f"""# Service {sid}: [{ipv6}]:{port} → {server_ipv4}:{internal_port}
frontend service_{sid}_ipv6
    bind [{ipv6}]:{port}
    mode {protocol}
    option {protocol}log
    default_backend backend_to_local_{internal_port}

"""

        config += f"""backend backend_to_local_{internal_port}
    mode {protocol}
    option {protocol}-check
    server local_instance {server_ipv4}:{internal_port} check

"""

    return config


def write_config(config: str, config_path: str) -> bool:
    Path(config_path).parent.mkdir(parents=True, exist_ok=True)
    Path(config_path).write_text(config)


def reload_haproxy():
    print("[*] Reloading HAProxy...")
    subprocess.run(["systemctl", "reload", "haproxy"], check=True)


def reconcile_haproxy(desired_rules: list):
    config_path = settings.nftables_file  # Repurposed variable
    config = generate_haproxy_config(desired_rules)
    write_config(config, config_path)
    reload_haproxy()


def main_loop():
    """Main service loop: fetch rules, reconcile HAProxy config."""
    print("Starting nftables-manager service (HAProxy mode)")
    print(f"Backend URL: {settings.backend_url}")
    print(f"Poll interval: {settings.poll_interval} seconds")
    print(f"HAProxy config: {settings.nftables_file}")

    consecutive_errors = 0
    max_consecutive_errors = 5

    while True:
        try:
            rules = fetch_rules()
            print(f"Fetched {len(rules)} active service rules")

            reconcile_haproxy(rules)
            print(f"Successfully reconciled HAProxy with {len(rules)} services")

            consecutive_errors = 0

        except requests.RequestException as e:
            consecutive_errors += 1
            print(
                f"Backend API error ({consecutive_errors}/{max_consecutive_errors}): {e}"
            )

        except (subprocess.CalledProcessError, OSError) as e:
            consecutive_errors += 1
            print(f"HAProxy error ({consecutive_errors}/{max_consecutive_errors}): {e}")

        except Exception as e:
            print(f"Unexpected error: {e}")
            consecutive_errors += 1

        time.sleep(settings.poll_interval)
