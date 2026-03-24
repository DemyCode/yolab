import asyncio
import os
import subprocess
from typing import AsyncIterator

VOLUMES_ROOT = "/mnt/yolab-volumes"


def _run(*args: str) -> None:
    subprocess.run(list(args), check=True)


def volume_path(service_name: str, volume_name: str) -> str:
    return f"{VOLUMES_ROOT}/{service_name}/{volume_name}"


def create_volume(service_name: str, volume_name: str, disk_paths: list[str]) -> str:
    mount = volume_path(service_name, volume_name)
    os.makedirs(mount, exist_ok=True)
    branches = ":".join(disk_paths)
    _run(
        "mergerfs", branches, mount,
        "-o", "allow_other,category.create=ff,minfreespace=1G,lazy-umount-on-error=true",
    )
    return mount


def destroy_volume(service_name: str, volume_name: str) -> None:
    mount = volume_path(service_name, volume_name)
    subprocess.run(["umount", mount], check=False)


async def reorganize_volume(
    service_name: str,
    volume_name: str,
    old_disk_paths: list[str],
    new_disk_paths: list[str],
) -> AsyncIterator[str]:
    from node_agent.k3s import scale_deployment
    mount = volume_path(service_name, volume_name)

    yield f"$ kubectl scale deployment {service_name} --replicas=0"
    scale_deployment(service_name, 0)

    yield f"$ umount {mount}"
    subprocess.run(["umount", mount], check=False)

    old_set = set(old_disk_paths)
    new_set = set(new_disk_paths)
    for src in old_set - new_set:
        if new_disk_paths:
            dst = new_disk_paths[0]
            yield f"$ rsync -avP {src}/ {dst}/"
            proc = await asyncio.create_subprocess_exec(
                "rsync", "-avP", "--progress", f"{src}/", f"{dst}/",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            assert proc.stdout is not None
            async for line in proc.stdout:
                text = line.decode().rstrip()
                if text:
                    yield text
            await proc.wait()
            if proc.returncode != 0:
                yield f"[ERROR] rsync exited with {proc.returncode}"
                return

    branches = ":".join(new_disk_paths)
    try:
        _run(
            "mergerfs", branches, mount,
            "-o", "allow_other,category.create=ff,minfreespace=1G,lazy-umount-on-error=true",
        )
    except subprocess.CalledProcessError as e:
        yield f"[ERROR] mergerfs remount failed: {e}"
        return

    yield f"$ kubectl scale deployment {service_name} --replicas=1"
    scale_deployment(service_name, 1)

    yield "[DONE]"


def reorganize_estimate(old_disk_paths: list[str], new_disk_paths: list[str]) -> dict[str, int]:
    old_set = set(old_disk_paths)
    new_set = set(new_disk_paths)
    total = 0
    for src in old_set - new_set:
        try:
            result = subprocess.run(
                ["du", "-sb", src], capture_output=True, text=True, check=True,
            )
            total += int(result.stdout.split()[0])
        except (subprocess.CalledProcessError, ValueError, IndexError):
            pass
    return {"bytes_to_move": total}
