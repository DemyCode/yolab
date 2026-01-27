"""SSH key generation utilities."""

import subprocess
import tempfile
from pathlib import Path


def generate_ssh_keypair() -> tuple[str, str]:
    """
    Generate an SSH ed25519 key pair.

    Returns:
        tuple: (private_key, public_key)
    """
    with tempfile.TemporaryDirectory() as tmpdir:
        key_path = Path(tmpdir) / "id_ed25519"

        # Generate SSH key without passphrase
        subprocess.run(
            [
                "ssh-keygen",
                "-t",
                "ed25519",
                "-f",
                str(key_path),
                "-N",
                "",  # No passphrase
                "-C",
                "yolab-generated-key",
            ],
            check=True,
            capture_output=True,
        )

        # Read private and public keys
        private_key = key_path.read_text().strip()
        public_key = (key_path.parent / "id_ed25519.pub").read_text().strip()

        return private_key, public_key
