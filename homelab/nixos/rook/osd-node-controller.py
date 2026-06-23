#!/usr/bin/env python3
import os, re, time, logging, random
from kubernetes import client, config
from kubernetes.client.rest import ApiException

logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(message)s')
log = logging.getLogger(__name__)

NODE_NAME = os.environ['MY_NODE_NAME']
NAMESPACE = 'rook-ceph'
CLUSTER   = 'rook-ceph'
INTERVAL  = 30

def get_devices():
    """
    Enumerate candidate block devices from host sysfs.

    Physical disks (sd*, nvme*n*, vd*) are all included — even the OS disk.
    Rook's own OSD prepare inventory already rejects disks that have partitions,
    existing filesystems, or are too small, so passing everything is safe.

    Loop devices are included only if they have a backing file attached.
    dm-*, md*, and partition devices are never listed in /sys/block directly
    so they don't appear here.
    """
    devices = []
    try:
        for name in sorted(os.listdir('/host-sys/block')):
            is_physical = bool(
                re.fullmatch(r'sd[a-z]+', name)     or  # SATA / SCSI / USB
                re.fullmatch(r'nvme\d+n\d+', name)  or  # NVMe namespace
                re.fullmatch(r'vd[a-z]+', name)         # VirtIO
            )
            is_loop = bool(re.fullmatch(r'loop\d+', name)) and \
                      os.path.exists(f'/host-sys/block/{name}/loop/backing_file')

            if is_physical or is_loop:
                devices.append(name)
    except Exception as e:
        log.warning(f'sysfs read error: {e}')
    return devices


def reconcile(custom_api):
    devices = get_devices()
    log.info(f'Candidate devices on {NODE_NAME}: {devices}')

    for attempt in range(5):
        try:
            cr      = custom_api.get_namespaced_custom_object(
                          'ceph.rook.io', 'v1', NAMESPACE, 'cephclusters', CLUSTER)
            storage = cr.get('spec', {}).get('storage', {})
            nodes   = list(storage.get('nodes') or [])

            desired_devs = [{'name': d} for d in devices]
            idx = next((i for i, n in enumerate(nodes) if n['name'] == NODE_NAME), None)
            changed = False

            if idx is None:
                nodes.append({'name': NODE_NAME, 'devices': desired_devs})
                changed = True
            else:
                current_devs = sorted(d['name'] for d in (nodes[idx].get('devices') or []))
                if current_devs != devices:
                    log.info(f'Device change: {current_devs} -> {devices}')
                    nodes[idx]['devices'] = desired_devs
                    changed = True
                if 'deviceFilter' in nodes[idx]:
                    del nodes[idx]['deviceFilter']
                    changed = True

            if not changed:
                log.info('State is current, nothing to do')
                return

            patch = {'spec': {'storage': {'nodes': nodes}}}
            custom_api.patch_namespaced_custom_object(
                'ceph.rook.io', 'v1', NAMESPACE, 'cephclusters', CLUSTER, patch)
            log.info('CephCluster patched')
            return

        except ApiException as e:
            if e.status == 409 and attempt < 4:
                delay = random.uniform(1, 4)
                log.warning(f'Conflict, retrying in {delay:.1f}s ({attempt + 1}/5)')
                time.sleep(delay)
            else:
                raise


config.load_incluster_config()
custom_api = client.CustomObjectsApi()

time.sleep(random.uniform(0, 10))
log.info(f'osd-node-controller started on {NODE_NAME}')

while True:
    try:
        reconcile(custom_api)
    except Exception as e:
        log.error(f'Reconcile error: {e}')
    time.sleep(INTERVAL)
