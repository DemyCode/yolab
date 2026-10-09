import os
import re
import unittest

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
SPEAKS_AES256K = (3, 17, 1)
RBAC = {"ServiceAccount", "ClusterRole", "ClusterRoleBinding", "Role", "RoleBinding"}


def docs():
    with open(os.path.join(HERE, "cephfs.yaml")) as f:
        return [d for d in yaml.safe_load_all(f) if d]


def find(kind, name):
    for d in docs():
        if d["kind"] == kind and d["metadata"]["name"] == name:
            return d
    raise AssertionError(f"{kind} {name} missing")


def ours(d):
    return (d["metadata"].get("labels") or {}).get("app.kubernetes.io/managed-by") == "yolab"


def containers(workload):
    return workload["spec"]["template"]["spec"]["containers"]


def cephcsi_version(image):
    m = re.search(r":v?(\d+)\.(\d+)\.(\d+)", image.split("@")[0])
    return tuple(int(x) for x in m.groups()) if m else None


class CephfsCsiManifest(unittest.TestCase):
    def test_existing_volumes_keep_every_name_they_were_bound_with(self):
        driver = find("CSIDriver", "rook-ceph.cephfs.csi.ceph.com")
        self.assertIs(driver["spec"]["attachRequired"], True)
        sc = find("StorageClass", "yolab-cephfs")
        self.assertEqual(sc["provisioner"], "rook-ceph.cephfs.csi.ceph.com")
        self.assertEqual(sc["parameters"]["clusterID"], "rook-ceph")
        self.assertEqual(
            sc["parameters"]["csi.storage.k8s.io/node-stage-secret-name"],
            "rook-csi-cephfs-node",
        )
        self.assertEqual(
            sc["parameters"]["csi.storage.k8s.io/provisioner-secret-name"],
            "rook-csi-cephfs-provisioner",
        )
        ds = find("DaemonSet", "csi-cephfsplugin")
        self.assertEqual(ds["metadata"]["namespace"], "rook-ceph")
        self.assertEqual(ds["spec"]["template"]["metadata"]["labels"]["app"], "csi-cephfsplugin")
        find("Deployment", "csi-cephfsplugin-provisioner")

    def test_the_node_plugin_mounts_from_the_host_network_so_mounts_outlive_its_pod(self):
        ds = find("DaemonSet", "csi-cephfsplugin")
        self.assertIs(ds["spec"]["template"]["spec"]["hostNetwork"], True)
        plugin = next(c for c in containers(ds) if c["name"] == "csi-cephfsplugin")
        self.assertIn("--forcecephkernelclient=true", plugin["args"])

    def test_every_cephcsi_we_ship_speaks_aes256k(self):
        workloads = [d for d in docs() if d["kind"] in ("DaemonSet", "Deployment")]
        self.assertEqual(len(workloads), 2)
        for w in workloads:
            plugins = [c for c in containers(w) if c["name"] == "csi-cephfsplugin"]
            self.assertTrue(plugins, w["metadata"]["name"])
            for c in plugins:
                self.assertGreaterEqual(cephcsi_version(c["image"]), SPEAKS_AES256K, c["image"])

    def test_every_image_is_pinned_by_digest(self):
        for w in docs():
            if w["kind"] not in ("DaemonSet", "Deployment"):
                continue
            for c in containers(w):
                self.assertIn("@sha256:", c["image"], c["image"])

    def test_everything_but_shared_names_is_labelled_ours(self):
        for d in docs():
            if d["kind"] in ("Namespace", "StorageClass"):
                continue
            self.assertTrue(ours(d), f'{d["kind"]} {d["metadata"]["name"]}')

    def test_our_rbac_never_shares_a_name_with_what_a_rook_uninstall_deletes(self):
        for d in docs():
            if d["kind"] in RBAC:
                self.assertTrue(d["metadata"]["name"].startswith("yolab-"), d["metadata"]["name"])


if __name__ == "__main__":
    unittest.main()
