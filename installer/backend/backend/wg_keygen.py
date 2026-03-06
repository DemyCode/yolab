"""WireGuard key generation utilities."""

import subprocess


def generate_wg_keypair() -> tuple[str, str]:
    """
    Generate a WireGuard key pair using wg(8).

    Returns:
        tuple: (private_key, public_key) as base64 strings
    """
    private_key = subprocess.run(
        ["wg", "genkey"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    public_key = subprocess.run(
        ["wg", "pubkey"],
        input=private_key,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    return private_key, public_key
