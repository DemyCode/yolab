#!/usr/bin/env python3
import http.server
import json
import socketserver
import subprocess
import urllib.parse
from pathlib import Path

PORT = 8000


def test_internet():
    try:
        result = subprocess.run(
            ["ping", "-c", "1", "-W", "2", "1.1.1.1"],
            capture_output=True,
            timeout=3,
        )
        return result.returncode == 0
    except (subprocess.TimeoutExpired, OSError):
        return False


def scan_wifi_networks():
    try:
        subprocess.run(
            ["nmcli", "device", "wifi", "rescan"], capture_output=True, timeout=10
        )
        result = subprocess.run(
            ["nmcli", "-t", "-f", "SSID,SIGNAL,SECURITY", "device", "wifi", "list"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        networks = []
        for line in result.stdout.strip().split("\n"):
            if line:
                parts = line.split(":")
                if len(parts) >= 3 and parts[0]:
                    networks.append(
                        {
                            "ssid": parts[0],
                            "signal": parts[1],
                            "security": parts[2],
                        }
                    )
        return sorted(
            networks, key=lambda x: int(x["signal"]) if x["signal"] else 0, reverse=True
        )
    except (subprocess.TimeoutExpired, OSError):
        return []


def connect_wifi(ssid, password):
    try:
        if password:
            result = subprocess.run(
                ["nmcli", "device", "wifi", "connect", ssid, "password", password],
                capture_output=True,
                text=True,
                timeout=30,
            )
        else:
            result = subprocess.run(
                ["nmcli", "device", "wifi", "connect", ssid],
                capture_output=True,
                text=True,
                timeout=30,
            )
        return result.returncode == 0
    except (subprocess.TimeoutExpired, OSError):
        return False


def get_wifi_config():
    try:
        result = subprocess.run(
            ["nmcli", "-t", "-f", "NAME,TYPE", "connection", "show", "--active"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        for line in result.stdout.strip().split("\n"):
            if "802-11-wireless" in line:
                ssid = line.split(":")[0]
                psk_result = subprocess.run(
                    [
                        "nmcli",
                        "-s",
                        "-g",
                        "802-11-wireless-security.psk",
                        "connection",
                        "show",
                        ssid,
                    ],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                return {"ssid": ssid, "psk": psk_result.stdout.strip()}
        return None
    except (subprocess.TimeoutExpired, OSError):
        return None


def detect_disks():
    result = subprocess.run(
        ["lsblk", "-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINT"],
        capture_output=True,
        text=True,
    )
    data = json.loads(result.stdout)
    disks = []
    for device in data.get("blockdevices", []):
        if device.get("type") == "disk":
            disks.append(
                {
                    "name": f"/dev/{device['name']}",
                    "size": device["size"],
                    "mounted": bool(device.get("mountpoint")),
                }
            )
    return disks


def detect_ram_size():
    try:
        with open("/proc/meminfo", "r") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    gb = kb // (1024 * 1024)
                    return min(gb, 32)
    except (OSError, ValueError, IndexError):
        return 8
    return 8


def generate_config_toml(
    disk, hostname, timezone, root_ssh_key, swap_size, git_remote, wifi_config
):
    wifi_section = ""
    if wifi_config:
        wifi_section = f'''
[wifi]
ssid = "{wifi_config["ssid"]}"
psk = "{wifi_config["psk"]}"
'''

    return f'''[homelab]
hostname = "{hostname}"
timezone = "{timezone}"
locale = "en_US.UTF-8"
ssh_port = 22
root_ssh_key = "{root_ssh_key}"
git_remote = "{git_remote}"
allowed_ssh_keys = []

[disk]
device = "{disk}"
esp_size = "500M"
swap_size = "{swap_size}G"
{wifi_section}
[client_ui]
enabled = true
port = 8080
platform_api_url = ""

[docker]
enabled = false
compose_url = ""

[frpc]
enabled = false
server_addr = ""
server_port = 7000
account_token = ""
'''


def generate_wifi_html(networks, error=None, success=None):
    network_options = "\n".join(
        [
            f'<option value="{n["ssid"]}">{n["ssid"]} - {n["signal"]}% {"🔒" if n["security"] else "🔓"}</option>'
            for n in networks
        ]
    )

    error_html = f'<div class="error">{error}</div>' if error else ""
    success_html = f'<div class="success">{success}</div>' if success else ""

    return f"""<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>WiFi Setup - Homelab Installer</title>
    <style>
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}
        body {{ font-family: monospace; background: #1a1a1a; color: #00ff00; padding: 20px; }}
        .container {{ max-width: 800px; margin: 0 auto; }}
        h1 {{ margin-bottom: 20px; border-bottom: 2px solid #00ff00; padding-bottom: 10px; }}
        .section {{ margin: 20px 0; padding: 15px; border: 1px solid #333; background: #222; }}
        .section h2 {{ margin-bottom: 10px; color: #00ff00; }}
        .form-group {{ margin: 15px 0; }}
        label {{ display: block; margin-bottom: 5px; font-weight: bold; }}
        input, select {{ width: 100%; padding: 10px; background: #333; color: #00ff00; border: 1px solid #555; font-family: monospace; font-size: 14px; }}
        button {{ padding: 15px 30px; background: #00ff00; color: #000; border: none; cursor: pointer; font-family: monospace; font-weight: bold; font-size: 16px; margin-top: 10px; }}
        button:hover {{ background: #00cc00; }}
        .error {{ background: #4d1a1a; color: #ff6666; padding: 15px; margin: 15px 0; border-left: 4px solid #ff0000; }}
        .success {{ background: #1a4d1a; color: #66ff66; padding: 15px; margin: 15px 0; border-left: 4px solid #00ff00; }}
        .warning {{ background: #4d4d1a; color: #ffff66; padding: 15px; margin: 15px 0; border-left: 4px solid #ffff00; }}
        .info {{ background: #1a1a4d; color: #6666ff; padding: 10px; margin: 10px 0; border-left: 4px solid #0000ff; }}
    </style>
</head>
<body>
    <div class="container">
        <h1>📡 WiFi Setup Required</h1>

        <div class="warning">⚠️ Internet connection is required for installation. Please connect to WiFi or plug in an ethernet cable.</div>

        {error_html}
        {success_html}

        <form method="POST" action="/wifi/connect">
            <div class="section">
                <h2>Connect to WiFi</h2>
                <div class="form-group">
                    <label>Network:</label>
                    <select name="ssid" required>
                        <option value="">-- Select a network --</option>
                        {network_options}
                    </select>
                </div>
                <div class="form-group">
                    <label>Password (leave empty for open networks):</label>
                    <input type="password" name="password" placeholder="WiFi password">
                </div>
                <button type="submit">🔌 Connect</button>
            </div>
        </form>

        <div class="section">
            <h2>Alternative</h2>
            <div class="info">💡 If you've plugged in an ethernet cable, click below to check connection:</div>
            <form method="GET" action="/">
                <button type="submit">🔄 Check Connection & Continue</button>
            </form>
        </div>
    </div>
</body>
</html>"""


def generate_html(disks, error=None, success=None, internet_connected=True):
    disk_options = "\n".join(
        [
            f'<option value="{d["name"]}">{d["name"]} - {d["size"]} {"(MOUNTED)" if d["mounted"] else ""}</option>'
            for d in disks
        ]
    )

    error_html = f'<div class="error">{error}</div>' if error else ""
    success_html = f'<div class="success">{success}</div>' if success else ""
    internet_status = "🟢 Connected" if internet_connected else "🔴 No Internet"
    internet_class = "success" if internet_connected else "error"

    return f"""<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>Homelab Installer</title>
    <style>
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}
        body {{ font-family: monospace; background: #1a1a1a; color: #00ff00; padding: 20px; }}
        .container {{ max-width: 800px; margin: 0 auto; }}
        h1 {{ margin-bottom: 20px; border-bottom: 2px solid #00ff00; padding-bottom: 10px; }}
        .section {{ margin: 20px 0; padding: 15px; border: 1px solid #333; background: #222; }}
        .section h2 {{ margin-bottom: 10px; color: #00ff00; }}
        .form-group {{ margin: 15px 0; }}
        label {{ display: block; margin-bottom: 5px; font-weight: bold; }}
        input, select {{ width: 100%; padding: 10px; background: #333; color: #00ff00; border: 1px solid #555; font-family: monospace; font-size: 14px; }}
        button {{ padding: 15px 30px; background: #00ff00; color: #000; border: none; cursor: pointer; font-family: monospace; font-weight: bold; font-size: 16px; margin-top: 10px; }}
        button:hover {{ background: #00cc00; }}
        .error {{ background: #4d1a1a; color: #ff6666; padding: 15px; margin: 15px 0; border-left: 4px solid #ff0000; }}
        .success {{ background: #1a4d1a; color: #66ff66; padding: 15px; margin: 15px 0; border-left: 4px solid #00ff00; }}
        .info {{ background: #1a1a4d; color: #6666ff; padding: 10px; margin: 10px 0; border-left: 4px solid #0000ff; }}
        .status-bar {{ padding: 10px; margin-bottom: 20px; text-align: center; font-weight: bold; }}
    </style>
</head>
<body>
    <div class="container">
        <h1>🖥️ Homelab NixOS Installer</h1>

        <div class="status-bar {internet_class}">
            Internet Status: {internet_status}
        </div>

        {error_html}
        {success_html}

        <form method="POST" action="/install">
            <div class="section">
                <h2>1. Select Disk</h2>
                <div class="form-group">
                    <label>Disk Device:</label>
                    <select name="disk" required>
                        <option value="">-- Select a disk --</option>
                        {disk_options}
                    </select>
                </div>
                <div class="info">⚠️ All data on selected disk will be erased!</div>
            </div>

            <div class="section">
                <h2>2. System Configuration</h2>
                <div class="form-group">
                    <label>Hostname:</label>
                    <input type="text" name="hostname" value="homelab" required pattern="[a-z0-9-]+" title="Lowercase letters, numbers, and hyphens only">
                </div>
                <div class="form-group">
                    <label>Timezone:</label>
                    <input type="text" name="timezone" value="UTC" required placeholder="UTC, America/New_York, Europe/Paris, etc.">
                </div>
                <div class="form-group">
                    <label>Root SSH Key (REQUIRED):</label>
                    <input type="text" name="root_ssh_key" placeholder="ssh-ed25519 AAAA..." required>
                </div>
                <div class="form-group">
                    <label>Git Remote URL (REQUIRED):</label>
                    <input type="url" name="git_remote" placeholder="https://github.com/username/homelab.git" required>
                </div>
                <div class="info">💡 Configuration will be cloned from this git repository. Make sure it's accessible!</div>
            </div>

            <div class="section">
                <h2>3. Install</h2>
                <button type="submit">🚀 Install NixOS</button>
            </div>
        </form>
    </div>
</body>
</html>"""


def run_installation(disk, hostname, timezone, root_ssh_key, git_remote):
    install_dir = Path("/mnt/installer")
    install_dir.mkdir(parents=True, exist_ok=True)

    # Clone homelab configuration from git repository
    subprocess.run(
        ["git", "clone", git_remote, str(install_dir)],
        check=True,
        capture_output=True,
    )

    swap_size = detect_ram_size()
    wifi_config = get_wifi_config()

    config_toml = install_dir / "config.toml"
    config_toml.write_text(
        generate_config_toml(
            disk, hostname, timezone, root_ssh_key, swap_size, git_remote, wifi_config
        )
    )

    subprocess.run(
        ["nixos-generate-config", "--no-filesystems", "--dir", str(install_dir)],
        check=True,
        capture_output=True,
    )

    # Run disko-install to partition disk and install NixOS
    subprocess.run(
        [
            "nix",
            "--extra-experimental-features",
            "nix-command flakes",
            "run",
            "github:nix-community/disko#disko-install",
            "--",
            "--flake",
            f"{install_dir}#homelab",
            "--disk",
            "disk1",
            disk,
        ],
        check=True,
        capture_output=True,
    )

    # Copy git repository to /mnt/etc/nixos for future updates
    nixos_dir = Path("/mnt/etc/nixos")
    nixos_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        ["cp", "-rT", str(install_dir), str(nixos_dir)],
        check=True,
        capture_output=True,
    )


class InstallerHandler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/":
            internet = test_internet()
            if not internet:
                networks = scan_wifi_networks()
                html = generate_wifi_html(networks)
                self.send_response(200)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
            else:
                disks = detect_disks()
                html = generate_html(disks, internet_connected=True)
                self.send_response(200)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        if self.path == "/wifi/connect":
            content_length = int(self.headers["Content-Length"])
            post_data = self.rfile.read(content_length).decode("utf-8")
            params = urllib.parse.parse_qs(post_data)

            ssid = params.get("ssid", [""])[0]
            password = params.get("password", [""])[0]

            if not ssid:
                networks = scan_wifi_networks()
                html = generate_wifi_html(networks, error="Please select a network")
                self.send_response(400)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
                return

            success = connect_wifi(ssid, password)
            if success:
                networks = scan_wifi_networks()
                html = generate_wifi_html(
                    networks,
                    success=f"✅ Connected to {ssid}! Redirecting to installer...<br><meta http-equiv='refresh' content='2;url=/' />",
                )
                self.send_response(200)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
            else:
                networks = scan_wifi_networks()
                html = generate_wifi_html(
                    networks,
                    error=f"Failed to connect to {ssid}. Check password and try again.",
                )
                self.send_response(500)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())

        elif self.path == "/install":
            # Validate internet connection before install
            if not test_internet():
                disks = detect_disks()
                html = generate_html(
                    disks,
                    error="⚠️ Internet connection required! Please configure WiFi or connect ethernet.",
                    internet_connected=False,
                )
                self.send_response(400)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
                return

            content_length = int(self.headers["Content-Length"])
            post_data = self.rfile.read(content_length).decode("utf-8")
            params = urllib.parse.parse_qs(post_data)

            disk = params.get("disk", [""])[0]
            hostname = params.get("hostname", ["homelab"])[0]
            timezone = params.get("timezone", ["UTC"])[0]
            root_ssh_key = params.get("root_ssh_key", [""])[0]
            git_remote = params.get("git_remote", [""])[0]

            if not disk:
                disks = detect_disks()
                html = generate_html(
                    disks, error="Please select a disk", internet_connected=True
                )
                self.send_response(400)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
                return

            if not git_remote:
                disks = detect_disks()
                html = generate_html(
                    disks, error="Git remote URL is required!", internet_connected=True
                )
                self.send_response(400)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
                return

            try:
                run_installation(disk, hostname, timezone, root_ssh_key, git_remote)
                disks = detect_disks()
                html = generate_html(
                    disks,
                    success=f"✅ Installation complete!<br><br>Hostname: {hostname}<br>Disk: {disk}<br>Git Remote: {git_remote}<br><br>You can now reboot the system.",
                    internet_connected=True,
                )
                self.send_response(200)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
            except subprocess.CalledProcessError as e:
                disks = detect_disks()
                html = generate_html(
                    disks, error=f"Installation failed: {e}", internet_connected=True
                )
                self.send_response(500)
                self.send_header("Content-type", "text/html")
                self.end_headers()
                self.wfile.write(html.encode())
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        print(f"[{self.log_date_time_string()}] {format % args}")


if __name__ == "__main__":
    with socketserver.TCPServer(("", PORT), InstallerHandler) as httpd:
        print(f"Homelab Installer running on http://0.0.0.0:{PORT}")
        httpd.serve_forever()
