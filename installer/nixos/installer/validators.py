"""Input validation functions for the YoLab installer."""

import re


def validate_hostname(value: str) -> bool:
    if not value or len(value) < 3 or len(value) > 20:
        return False

    if not value[0].isalnum() or not value[-1].isalnum():
        return False

    pattern = r"^[a-z0-9-]+$"
    return bool(re.match(pattern, value.lower()))


def validate_ssh_key(value: str) -> bool:
    if not value:
        return False

    value = value.strip()
    valid_prefixes = [
        "ssh-ed25519",
        "ssh-rsa",
        "ecdsa-sha2-nistp256",
        "ecdsa-sha2-nistp384",
        "ecdsa-sha2-nistp521",
    ]

    return any(value.startswith(prefix) for prefix in valid_prefixes)


def validate_git_url(value: str) -> bool:
    if not value:
        return False
    value = value.strip()
    valid_patterns = [
        r"^https?://",  # http or https
        r"^git@",  # git@github.com:user/repo.git
        r"^git://",  # git protocol
    ]
    return any(re.match(pattern, value) for pattern in valid_patterns)


def validate_disk_available(disk: str, available_disks: list[dict]) -> bool:
    if not disk:
        return False
    for d in available_disks:
        if d["name"] == disk:
            return not d["mounted"]
    return False


def validate_timezone(value: str) -> bool:
    if not value:
        return False
    pattern = r"^[A-Z][a-zA-Z_/+-]*$"
    return bool(re.match(pattern, value))
