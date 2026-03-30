import json
import logging
import os
import subprocess
from concurrent import futures

import grpc

from node_agent import config
from node_agent.csi import csi_pb2, csi_pb2_grpc
from node_agent.nfs import mount_remote, umount_remote

log = logging.getLogger("csi")

PLUGIN_NAME = "csi.yolab.dev"
PLUGIN_VERSION = "0.1.0"
NFS_MOUNT_ROOT = "/mnt/yolab-nfs"

_published_specs: dict[str, dict] = {}


def _parse_disk_spec(volume_context: dict) -> dict:
    raw = volume_context.get("diskSpec", "{}")
    return json.loads(raw)


def _ensure_mounted(spec: dict) -> str:
    disk_id = spec["disk_id"]
    node_wg_ipv6 = spec.get("node_wg_ipv6", "")
    mount_path = spec.get("mount_path", f"/yolab/data/{disk_id}")

    if not node_wg_ipv6 or node_wg_ipv6 == config.WG_IPV6:
        return mount_path

    local_path = f"{NFS_MOUNT_ROOT}/{disk_id}"
    if os.path.ismount(local_path):
        return local_path
    return mount_remote(disk_id, node_wg_ipv6, mount_path)


class IdentityServicer(csi_pb2_grpc.IdentityServicer):
    def GetPluginInfo(self, request, context):
        return csi_pb2.GetPluginInfoResponse(name=PLUGIN_NAME, vendor_version=PLUGIN_VERSION)

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
        return csi_pb2.CreateVolumeResponse(
            volume=csi_pb2.Volume(
                volume_id=request.name,
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
        spec = _parse_disk_spec(dict(request.volume_context))

        if not spec:
            context.set_code(grpc.StatusCode.INVALID_ARGUMENT)
            context.set_details("No disk spec in volume context")
            return csi_pb2.NodePublishVolumeResponse()

        os.makedirs(target_path, exist_ok=True)

        try:
            source_path = _ensure_mounted(spec)
        except Exception as e:
            log.error("failed to mount disk %s: %s", spec.get("disk_id"), e)
            context.set_code(grpc.StatusCode.INTERNAL)
            context.set_details(str(e))
            return csi_pb2.NodePublishVolumeResponse()

        try:
            subprocess.run(["mount", "--bind", source_path, target_path], check=True)
        except subprocess.CalledProcessError as e:
            context.set_code(grpc.StatusCode.INTERNAL)
            context.set_details(str(e))
            return csi_pb2.NodePublishVolumeResponse()

        _published_specs[volume_id] = spec
        return csi_pb2.NodePublishVolumeResponse()

    def NodeUnpublishVolume(self, request, context):
        target_path = request.target_path
        volume_id = request.volume_id
        spec = _published_specs.pop(volume_id, {})

        subprocess.run(["umount", target_path], check=False)

        node_wg_ipv6 = spec.get("node_wg_ipv6", "")
        if node_wg_ipv6 and node_wg_ipv6 != config.WG_IPV6:
            umount_remote(spec["disk_id"])

        return csi_pb2.NodeUnpublishVolumeResponse()

    def NodeGetCapabilities(self, request, context):
        return csi_pb2.NodeGetCapabilitiesResponse(capabilities=[])

    def NodeGetInfo(self, request, context):
        return csi_pb2.NodeGetInfoResponse(node_id=config.NODE_ID or "unknown")


def create_csi_server() -> grpc.Server:
    socket_path = config.CSI_SOCKET
    os.makedirs(os.path.dirname(socket_path), exist_ok=True)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    csi_pb2_grpc.add_IdentityServicer_to_server(IdentityServicer(), server)
    csi_pb2_grpc.add_ControllerServicer_to_server(ControllerServicer(), server)
    csi_pb2_grpc.add_NodeServicer_to_server(NodeServicer(), server)
    server.add_insecure_port(f"unix://{socket_path}")
    return server
