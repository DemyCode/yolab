import os
import subprocess

EXPORTS_FILE = "/etc/exports.d/yolab.exports"
NFS_MOUNT_ROOT = "/mnt/yolab-nfs"


def _run(*args: str) -> None:
    subprocess.run(list(args), check=True)


def export_disk(disk_id: str, mount_path: str) -> None:
    os.makedirs(os.path.dirname(EXPORTS_FILE), exist_ok=True)
    entry = (
        f"{mount_path} *(rw,sync,no_subtree_check,no_root_squash)  # yolab:{disk_id}\n"
    )
    with open(EXPORTS_FILE, "a") as f:
        f.write(entry)
    _run("exportfs", "-ra")


def unexport_disk(disk_id: str) -> None:
    if not os.path.exists(EXPORTS_FILE):
        return
    with open(EXPORTS_FILE) as f:
        lines = f.readlines()
    lines = [l for l in lines if f"# yolab:{disk_id}" not in l]
    with open(EXPORTS_FILE, "w") as f:
        f.writelines(lines)
    _run("exportfs", "-ra")


def mount_remote(disk_id: str, remote_ipv6: str, remote_path: str) -> str:
    local_path = f"{NFS_MOUNT_ROOT}/{disk_id}"
    os.makedirs(local_path, exist_ok=True)
    _run(
        "mount",
        "-t",
        "nfs4",
        "-o",
        "soft,timeo=5,retrans=2",
        f"[{remote_ipv6}]:{remote_path}",
        local_path,
    )
    return local_path


def umount_remote(disk_id: str) -> None:
    local_path = f"{NFS_MOUNT_ROOT}/{disk_id}"
    subprocess.run(["umount", local_path], check=False)
