import shutil
import subprocess
import tomllib
from pathlib import Path

import httpx
import tomli_w


def read_config(config_path: Path) -> dict:
    if not config_path.exists():
        raise FileNotFoundError(f"Config file not found at {config_path}")
    with open(config_path, "rb") as f:
        return tomllib.load(f)


def write_config(config_path: Path, config: dict) -> None:
    config_path.parent.mkdir(parents=True, exist_ok=True)
    with open(config_path, "wb") as f:
        tomli_w.dump(config, f)


def init_config(config_path: Path, example_path: Path) -> None:
    if not example_path.exists():
        raise FileNotFoundError(f"Example config not found at {example_path}")
    if config_path.exists():
        raise FileExistsError(f"Config already exists at {config_path}")
    shutil.copy(example_path, config_path)


def validate_config(config_path: Path) -> tuple[bool, list[str]]:
    errors = []
    try:
        config = read_config(config_path)
    except Exception as e:
        return False, [f"Failed to read config: {e}"]

    required_sections = ["homelab", "client_ui", "docker", "frpc"]
    for section in required_sections:
        if section not in config:
            errors.append(f"Missing required section: [{section}]")

    if "homelab" in config:
        required_homelab = ["hostname", "timezone", "locale", "ssh_port"]
        for field in required_homelab:
            if field not in config["homelab"]:
                errors.append(f"Missing required field: [homelab].{field}")

    return len(errors) == 0, errors


def list_available_services(platform_api_url: str) -> list[dict]:
    with httpx.Client() as client:
        response = client.get(f"{platform_api_url}/api/templates")
        response.raise_for_status()
        return response.json()


def list_downloaded_services(services_dir: Path) -> list[dict]:
    if not services_dir.exists():
        return []

    services = []
    for service_dir in services_dir.iterdir():
        if service_dir.is_dir():
            services.append(
                {
                    "name": service_dir.name,
                    "has_compose": (service_dir / "docker-compose.yml").exists(),
                    "has_caddy": (service_dir / "Caddyfile").exists(),
                }
            )
    return services


def download_service(
    services_dir: Path, platform_api_url: str, service_name: str
) -> None:
    with httpx.Client() as client:
        response = client.get(f"{platform_api_url}/api/templates/{service_name}")
        response.raise_for_status()
        data = response.json()

    service_dir = services_dir / service_name
    service_dir.mkdir(parents=True, exist_ok=True)
    (service_dir / "docker-compose.yml").write_text(data["docker_compose"])
    (service_dir / "Caddyfile").write_text(data["caddyfile"])


def delete_service(services_dir: Path, service_name: str) -> None:
    service_dir = services_dir / service_name
    if not service_dir.exists():
        raise FileNotFoundError(f"Service not found: {service_name}")
    shutil.rmtree(service_dir)


def rebuild_system(flake_path: str, timeout: int = 300) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["nixos-rebuild", "switch", "--flake", flake_path],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def test_system(flake_path: str, timeout: int = 300) -> subprocess.CompletedProcess:
    result = subprocess.run(
        ["nixos-rebuild", "test", "--flake", flake_path],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return result
