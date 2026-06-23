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

BLUESTORE_MAGIC = b'bluestore block device\n'
CEPH_FSID_KEY   = b'\x09\x00\x00\x00ceph_fsid'  # 4-byte LE len(9) + key bytes


def get_devices():
    """
    Enumerate candidate block devices from host sysfs.
    Includes all physical disk types and attached loop devices.
    The OS disk is included — Rook's own inventory rejects it (has partitions).
    """
    devices = []
    try:
        for name in sorted(os.listdir('/host-sys/block')):
            is_physical = bool(
                re.fullmatch(r'sd[a-z]+', name)    or  # SATA / SCSI / USB
                re.fullmatch(r'nvme\d+n\d+', name) or  # NVMe namespace
                re.fullmatch(r'vd[a-z]+', name)        # VirtIO
            )
            is_loop = bool(re.fullmatch(r'loop\d+', name)) and \
                      os.path.exists(f'/host-sys/block/{name}/loop/backing_file')
            if is_physical or is_loop:
                devices.append(name)
    except Exception as e:
        log.warning(f'sysfs read error: {e}')
    return sorted(devices)


def get_cluster_fsid(custom_api):
    """Get this cluster's FSID from CephCluster status."""
    try:
        cr = custom_api.get_namespaced_custom_object(
            'ceph.rook.io', 'v1', NAMESPACE, 'cephclusters', CLUSTER)
        return cr.get('status', {}).get('ceph', {}).get('fsid')
    except Exception:
        return None


def read_bluestore_header(device):
    """Read the first 4096 bytes of a device. Returns None on error."""
    dev_path = f'/host-dev/{device}'
    try:
        with open(dev_path, 'rb') as f:
            return f.read(4096)
    except (IOError, OSError) as e:
        log.debug(f'Cannot read {dev_path}: {e}')
        return None


def get_bluestore_fsid(device):
    """
    Read the BlueStore device label and extract the cluster FSID.

    BlueStore label layout (from Ceph source):
      offset 0   : 'bluestore block device\\n'  (magic, 23 bytes)
      offset 23  : OSD UUID as ASCII + '\\n'    (37 bytes)
      offset 60~ : Ceph-encoded bluestore_bdev_label_t
                   The 'meta' map contains 'ceph_fsid' -> <UUID string>
                   Strings are encoded as: [4-byte LE length][bytes]

    Returns the cluster FSID string or None if not a BlueStore device.
    """
    data = read_bluestore_header(device)
    if data is None or not data.startswith(BLUESTORE_MAGIC):
        return None

    pos = data.find(CEPH_FSID_KEY)
    if pos < 0:
        return None

    val_start = pos + len(CEPH_FSID_KEY)
    if val_start + 4 + 36 > len(data):
        return None

    val_len = int.from_bytes(data[val_start:val_start + 4], 'little')
    if val_len != 36:
        return None

    try:
        fsid = data[val_start + 4:val_start + 40].decode('ascii')
    except ValueError:
        return None

    if re.fullmatch(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', fsid):
        return fsid
    return None


def get_bluestore_osd_uuid(device):
    """
    Extract the OSD UUID from the BlueStore label.

    The OSD UUID is stored as 36 ASCII bytes at offset 23 (immediately after
    the magic), followed by a newline at offset 59.

    Returns the OSD UUID string or None.
    """
    data = read_bluestore_header(device)
    if data is None or not data.startswith(BLUESTORE_MAGIC):
        return None

    try:
        osd_uuid = data[23:59].decode('ascii')
    except (ValueError, UnicodeDecodeError):
        return None

    if re.fullmatch(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', osd_uuid):
        return osd_uuid
    return None


def wipe_device(device):
    """
    Zero the first 10 MB of the device to erase any BlueStore signature.
    Uses pure Python I/O — no external binaries required.
    """
    dev_path = f'/host-dev/{device}'
    log.info(f'Wiping {device} ...')
    chunk = b'\x00' * (64 * 1024)
    with open(dev_path, 'r+b') as f:
        f.seek(0)
        for _ in range(160):  # 160 × 64 KB = 10 MB
            f.write(chunk)
    log.info(f'{device} wiped')


def classify_devices(all_devices, our_fsid):
    """
    Returns the list of devices to register in the CephCluster CR this cycle.

    - Loop devices: always ours (created by yolab-system-osd), pass through.
    - Physical disks with no BlueStore label: clean, include.
    - Physical disks whose BlueStore FSID matches ours: include —
      Rook recognises the existing OSD and re-integrates it without reformatting.
    - Physical disks with a foreign BlueStore FSID: wipe now, exclude this cycle.
      The next cycle they will be clean and re-added, triggering a new Rook
      OSD prepare job.
    """
    effective = []
    for device in all_devices:
        if re.fullmatch(r'loop\d+', device):
            effective.append(device)
            continue

        device_fsid = get_bluestore_fsid(device)

        if device_fsid is None:
            effective.append(device)

        elif device_fsid == our_fsid:
            log.info(f'{device}: our cluster OSD (fsid={device_fsid}), Rook will re-integrate')
            effective.append(device)

        else:
            log.warning(
                f'{device}: foreign BlueStore detected '
                f'(device={device_fsid}, ours={our_fsid}) — wiping'
            )
            try:
                wipe_device(device)
            except Exception as e:
                log.error(f'Wipe failed for {device}: {e}')
            # Excluded this cycle; re-added next cycle once clean

    return sorted(effective)


def is_prepare_job_complete(batch_api):
    """Return True if the Rook OSD prepare job for this node has completed."""
    try:
        job = batch_api.read_namespaced_job(
            f'rook-ceph-osd-prepare-{NODE_NAME}', NAMESPACE)
        return job.status.completion_time is not None
    except ApiException as e:
        if e.status == 404:
            return False
        raise


def migrate_osd_deployments(apps_api, batch_api, our_fsid):
    """
    For each disk on this node that belongs to our cluster, check whether the
    corresponding OSD Deployment targets a different node. If so, and the
    prepare job for this node has completed (meaning Rook has updated the CRUSH
    map), patch the Deployment to run here instead.
    """
    if not is_prepare_job_complete(batch_api):
        return

    all_devices = get_devices()
    our_osd_uuids = {}
    for device in all_devices:
        if re.fullmatch(r'loop\d+', device):
            continue
        if get_bluestore_fsid(device) != our_fsid:
            continue
        osd_uuid = get_bluestore_osd_uuid(device)
        if osd_uuid:
            our_osd_uuids[osd_uuid] = device

    if not our_osd_uuids:
        return

    try:
        deploys = apps_api.list_namespaced_deployment(
            NAMESPACE, label_selector='app=rook-ceph-osd')
    except ApiException as e:
        log.error(f'Failed to list OSD deployments: {e}')
        return

    for deploy in deploys.items:
        containers = deploy.spec.template.spec.containers
        if not containers:
            continue
        env = containers[0].env or []

        deploy_uuid = next(
            (e.value for e in env if e.name == 'ROOK_OSD_UUID'), None)
        if deploy_uuid not in our_osd_uuids:
            continue

        node_selector = deploy.spec.template.spec.node_selector or {}
        if node_selector.get('kubernetes.io/hostname') == NODE_NAME:
            continue  # Already targeting this node

        old_node = node_selector.get('kubernetes.io/hostname', '?')
        device   = our_osd_uuids[deploy_uuid]
        log.info(
            f'{deploy.metadata.name}: disk {device} (uuid={deploy_uuid}) '
            f'moved from {old_node} to {NODE_NAME} — patching Deployment'
        )
        _patch_osd_deployment(apps_api, deploy, NODE_NAME)


def _patch_osd_deployment(apps_api, deploy, new_node):
    """Redirect an OSD Deployment to new_node by updating nodeSelector, args, and env."""
    deploy_name = deploy.metadata.name
    containers  = deploy.spec.template.spec.containers

    # Fix --crush-location arg
    args = list(containers[0].args or [])
    for i, arg in enumerate(args):
        if arg.startswith('--crush-location='):
            args[i] = f'--crush-location=root=default host={new_node}'
            break

    # Fix node-specific env vars, preserve all others (including valueFrom entries)
    new_env = []
    for e in (containers[0].env or []):
        if e.name in ('ROOK_NODE_NAME', 'ROOK_CRUSHMAP_HOSTNAME'):
            new_env.append({'name': e.name, 'value': new_node})
        else:
            d = {'name': e.name}
            if e.value is not None:
                d['value'] = e.value
            if e.value_from is not None:
                d['valueFrom'] = e.value_from.to_dict()
            new_env.append(d)

    patch = {
        'spec': {
            'template': {
                'spec': {
                    'nodeSelector': {'kubernetes.io/hostname': new_node},
                    'containers': [{'name': 'osd', 'args': args, 'env': new_env}],
                }
            }
        }
    }

    for attempt in range(5):
        try:
            apps_api.patch_namespaced_deployment(deploy_name, NAMESPACE, patch)
            log.info(f'{deploy_name} patched → {new_node}')
            return
        except ApiException as e:
            if e.status == 409 and attempt < 4:
                time.sleep(random.uniform(1, 4))
            else:
                log.error(f'Failed to patch {deploy_name}: {e}')
                return


def reconcile(custom_api, apps_api, batch_api):
    all_devices = get_devices()
    our_fsid    = get_cluster_fsid(custom_api)
    effective   = classify_devices(all_devices, our_fsid)

    log.info(f'Devices this cycle: {effective} (all seen: {all_devices})')

    for attempt in range(5):
        try:
            cr      = custom_api.get_namespaced_custom_object(
                          'ceph.rook.io', 'v1', NAMESPACE, 'cephclusters', CLUSTER)
            storage = cr.get('spec', {}).get('storage', {})
            nodes   = list(storage.get('nodes') or [])

            desired_devs = [{'name': d} for d in effective]
            idx = next((i for i, n in enumerate(nodes) if n['name'] == NODE_NAME), None)
            changed = False

            if idx is None:
                nodes.append({'name': NODE_NAME, 'devices': desired_devs})
                changed = True
            else:
                current_devs = sorted(d['name'] for d in (nodes[idx].get('devices') or []))
                if current_devs != effective:
                    log.info(f'Device list change: {current_devs} -> {effective}')
                    nodes[idx]['devices'] = desired_devs
                    changed = True
                if 'deviceFilter' in nodes[idx]:
                    del nodes[idx]['deviceFilter']
                    changed = True

            if not changed:
                log.info('State is current, nothing to do')
                break

            patch = {'spec': {'storage': {'nodes': nodes}}}
            custom_api.patch_namespaced_custom_object(
                'ceph.rook.io', 'v1', NAMESPACE, 'cephclusters', CLUSTER, patch)
            log.info('CephCluster patched')
            break

        except ApiException as e:
            if e.status == 409 and attempt < 4:
                delay = random.uniform(1, 4)
                log.warning(f'Conflict, retrying in {delay:.1f}s ({attempt + 1}/5)')
                time.sleep(delay)
            else:
                raise

    if our_fsid:
        migrate_osd_deployments(apps_api, batch_api, our_fsid)


config.load_incluster_config()
custom_api = client.CustomObjectsApi()
apps_api   = client.AppsV1Api()
batch_api  = client.BatchV1Api()

time.sleep(random.uniform(0, 10))
log.info(f'osd-node-controller started on {NODE_NAME}')

while True:
    try:
        reconcile(custom_api, apps_api, batch_api)
    except Exception as e:
        log.error(f'Reconcile error: {e}')
    time.sleep(INTERVAL)
