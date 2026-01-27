"""Input validation functions for the YoLab installer."""

import re


def validate_hostname(value: str) -> bool:
    """
    Validate hostname format.
    Must be alphanumeric with hyphens, 3-20 characters.
    Cannot start or end with a hyphen.
    """
    if not value or len(value) < 3 or len(value) > 20:
        return False

    # Must start and end with alphanumeric
    if not value[0].isalnum() or not value[-1].isalnum():
        return False

    # Only alphanumeric and hyphens allowed
    pattern = r"^[a-z0-9-]+$"
    return bool(re.match(pattern, value.lower()))


def validate_ssh_key(value: str) -> bool:
    """
    Validate SSH public key format.
    Must start with ssh-ed25519, ssh-rsa, ecdsa-sha2-nistp256, or ecdsa-sha2-nistp384.
    """
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
    """
    Validate git URL format.
    Must be http, https, or git protocol.
    """
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
    """
    Validate that disk exists in available disks list and is not mounted.
    """
    if not disk:
        return False

    for d in available_disks:
        if d["name"] == disk:
            return not d["mounted"]

    return False


def validate_timezone(value: str) -> bool:
    """
    Validate timezone format.
    Basic check for common timezone patterns.
    """
    if not value:
        return False

    # Common timezone patterns: UTC, America/New_York, Europe/London, etc.
    pattern = r"^[A-Z][a-zA-Z_/+-]*$"
    return bool(re.match(pattern, value))
