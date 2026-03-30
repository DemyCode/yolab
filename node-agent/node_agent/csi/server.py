import json
import logging
import os
import subprocess
from concurrent import futures

import grpc

from node_agent import config
from node_agent.csi import csi_pb2, csi_pb2_grpc
from node_agent.nfs import mount_remote, umount_remote
from node_agent.mergerfs import create_volume, destroy_volume, volume_path

log = logging.getLogger("csi")

PLUGIN_NAME = "csi.yolab.dev"
PLUGIN_VERSION = "0.1.0"
NFS_MOUNT_ROOT = "/mnt/yolab-nfs"
VOLUMES_MOUNT_ROOT = "/mnt/yolab-volumes"

_published_disk_specs: dict[str, list[dict]] = {}


def _parse_disk_specs(volume_context: dict) -> list[dict]:
    raw = volume_context.get("diskSpecs", "[]")
    return json.loads(raw)


def _ensure_disk_mounted(spec: dict) -> str:
    disk_id = spec["disk_id"]
    node_ipv6 = spec.get("node_ipv6", "")
    remote_path = spec.get("mount_path", f"/yolab/data/{disk_id}")

    if not node_ipv6 or node_ipv6 == config.WG_IPV6:
        return remote_path

    local_path = f"{NFS_MOUNT_ROOT}/{disk_id}"
    if os.path.ismount(local_path):
        return local_path
    return mount_remote(disk_id, node_ipv6, remote_path)


def _unmount_remote_disks(disk_specs: list[dict]) -> None:
    for spec in disk_specs:
        node_ipv6 = spec.get("node_ipv6", "")
        if node_ipv6 and node_ipv6 != config.WG_IPV6:
            umount_remote(spec["disk_id"])


class IdentityServicer(csi_pb2_grpc.IdentityServicer):
    def GetPluginInfo(self, request, context):
        return csi_pb2.GetPluginInfoResponse(
            name=PLUGIN_NAME,
            vendor_version=PLUGIN_VERSION,
        )

    def GetPluginCapabilities(self, request, context):
        cap = csi_pb2.PluginCapability(
            service=csi_pb2.PluginCapability.Service(
                type=csi_pb2.PluginCapability.Service.CONTROLLER_SERVICE,
            )
        )
        return csi_pb2.GetPluginCapabilitiesResponse(capabilities=[cap])

    def Probe(self, request, context):
        from google.protobuf import wrappers_pb2
        return csi_pb2.ProbeResponse(ready=wrappers_pb2.BoolValue(value=True))


class ControllerServicer(csi_pb2_grpc.ControllerServicer):
    def CreateVolume(self, request, context):
        volume_id = request.name
        return csi_pb2.CreateVolumeResponse(
            volume=csi_pb2.Volume(
                volume_id=volume_id,
                volume_context=dict(request.parameters),
            )
        )

    def DeleteVolume(self, request, context):
        return csi_pb2.DeleteVolumeResponse()

    def ControllerGetCapabilities(self, request, context):
        cap = csi_pb2.ControllerServiceCapability(
            rpc=csi_pb2.ControllerServiceCapability.RPC(
                type=csi_pb2.ControllerServiceCapability.RPC.CREATE_DELETE_VOLUME,
            )
        )
        return csi_pb2.ControllerGetCapabilitiesResponse(capabilities=[cap])


class NodeServicer(csi_pb2_grpc.NodeServicer):
    def NodePublishVolume(self, request, context):
        volume_id = request.volume_id
        target_path = request.target_path
        disk_specs = _parse_disk_specs(dict(request.volume_context))

        os.makedirs(target_path, exist_ok=True)

        local_paths = []
        for spec in disk_specs:
            try:
                path = _ensure_disk_mounted(spec)
                local_paths.append(path)
            except Exception as e:
                log.error("failed to mount disk %s: %s", spec.get("disk_id"), e)
                context.set_code(grpc.StatusCode.INTERNAL)
                context.set_details(str(e))
                return csi_pb2.NodePublishVolumeResponse()

        service_name, volume_name = _split_volume_id(volume_id)
        mount_point = volume_path(service_name, volume_name)
        if not os.path.ismount(mount_point):
            mount_point = create_volume(service_name, volume_name, local_paths)

        try:
            subprocess.run(
                ["mount", "--bind", mount_point, target_path], check=True
            )
        except subprocess.CalledProcessError as e:
            context.set_code(grpc.StatusCode.INTERNAL)
            context.set_details(str(e))
            return csi_pb2.NodePublishVolumeResponse()

        _published_disk_specs[volume_id] = disk_specs
        return csi_pb2.NodePublishVolumeResponse()

    def NodeUnpublishVolume(self, request, context):
        target_path = request.target_path
        volume_id = request.volume_id
        disk_specs = _published_disk_specs.pop(volume_id, [])

        subprocess.run(["umount", target_path], check=False)

        service_name, volume_name = _split_volume_id(volume_id)
        destroy_volume(service_name, volume_name)
        _unmount_remote_disks(disk_specs)

        return csi_pb2.NodeUnpublishVolumeResponse()

    def NodeGetCapabilities(self, request, context):
        return csi_pb2.NodeGetCapabilitiesResponse(capabilities=[])

    def NodeGetInfo(self, request, context):
        return csi_pb2.NodeGetInfoResponse(node_id=config.NODE_ID or "unknown")


def _split_volume_id(volume_id: str) -> tuple[str, str]:
    parts = volume_id.split("/", 1)
    if len(parts) == 2:
        return parts[0], parts[1]
    return volume_id, "data"


def create_csi_server() -> grpc.Server:
    socket_path = config.CSI_SOCKET
    os.makedirs(os.path.dirname(socket_path), exist_ok=True)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    csi_pb2_grpc.add_IdentityServicer_to_server(IdentityServicer(), server)
    csi_pb2_grpc.add_ControllerServicer_to_server(ControllerServicer(), server)
    csi_pb2_grpc.add_NodeServicer_to_server(NodeServicer(), server)
    server.add_insecure_port(f"unix://{socket_path}")
    return server
