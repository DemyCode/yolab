"""Password hashing utilities."""

import crypt
import secrets
import string


def hash_password(password: str) -> str:
    """
    Hash a password using SHA-512 (Linux standard).

    Args:
        password: Plain text password

    Returns:
        str: Hashed password in crypt format ($6$salt$hash)
    """
    # Generate a random salt
    salt_chars = string.ascii_letters + string.digits
    salt = ''.join(secrets.choice(salt_chars) for _ in range(16))

    # Hash with SHA-512 (method $6$)
    hashed = crypt.crypt(password, f"$6${salt}$")

    return hashed


def validate_password_strength(password: str) -> tuple[bool, str]:
    """
    Validate password meets minimum security requirements.

    Args:
        password: Password to validate

    Returns:
        tuple: (is_valid, error_message)
    """
    if len(password) < 8:
        return False, "Password must be at least 8 characters"

    if not any(c.isupper() for c in password):
        return False, "Password must contain at least one uppercase letter"

    if not any(c.islower() for c in password):
        return False, "Password must contain at least one lowercase letter"

    if not any(c.isdigit() for c in password):
        return False, "Password must contain at least one number"

    return True, ""
