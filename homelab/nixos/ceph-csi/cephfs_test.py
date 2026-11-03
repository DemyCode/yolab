import os
import re
import unittest

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
SPEAKS_AES256K = (3, 17, 1)
NAMESPACE = "ceph-csi"
DRIVER = "cephfs.csi.ceph.com"


def docs():
    with open(os.path.join(HERE, "cephfs.yaml")) as f:
        return [d for d in yaml.safe_load_all(f) if d]


def find(kind, name):
    for d in docs():
        if d["kind"] == kind and d["metadata"]["name"] == name:
            return d
    raise AssertionError(f"{kind} {name} missing")


def containers(workload):
    return workload["spec"]["template"]["spec"]["containers"]


def cephcsi_version(image):
    m = re.search(r":v?(\d+)\.(\d+)\.(\d+)", image.split("@")[0])
    return tuple(int(x) for x in m.groups()) if m else None


class CephfsCsiManifest(unittest.TestCase):
    def test_the_namespace_is_created_before_anything_placed_in_it(self):
        all_docs = docs()
        self.assertEqual(all_docs[0]["kind"], "Namespace")
        self.assertEqual(all_docs[0]["metadata"]["name"], NAMESPACE)
        for d in all_docs[1:]:
            if "namespace" in d["metadata"]:
                self.assertEqual(d["metadata"]["namespace"], NAMESPACE)

    def test_the_storage_class_names_what_local_api_publishes(self):
        find("CSIDriver", DRIVER)
        sc = find("StorageClass", "yolab-cephfs")
        self.assertEqual(sc["provisioner"], DRIVER)
        p = sc["parameters"]
        self.assertEqual(p["clusterID"], "yolab")
        self.assertEqual(p["csi.storage.k8s.io/node-stage-secret-name"], "csi-cephfs-node")
        self.assertEqual(
            p["csi.storage.k8s.io/provisioner-secret-name"], "csi-cephfs-provisioner"
        )
        for key, value in p.items():
            if key.endswith("-secret-namespace"):
                self.assertEqual(value, NAMESPACE, key)

    def test_both_workloads_read_the_config_map_local_api_publishes(self):
        for kind, name in [
            ("DaemonSet", "csi-cephfsplugin"),
            ("Deployment", "csi-cephfsplugin-provisioner"),
        ]:
            volumes = find(kind, name)["spec"]["template"]["spec"]["volumes"]
            maps = [v["configMap"]["name"] for v in volumes if "configMap" in v]
            self.assertEqual(maps, ["ceph-csi-config"], name)

    def test_every_driver_name_and_socket_path_is_the_same_driver(self):
        text = open(os.path.join(HERE, "cephfs.yaml")).read()
        for found in re.findall(r"[\w.-]*cephfs\.csi\.ceph\.com", text):
            self.assertEqual(found, DRIVER)

    def test_the_node_plugin_mounts_from_the_host_network_so_mounts_outlive_its_pod(
        self,
    ):
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
                self.assertGreaterEqual(
                    cephcsi_version(c["image"]), SPEAKS_AES256K, c["image"]
                )

    def test_every_image_is_pinned_by_digest(self):
        for w in docs():
            if w["kind"] not in ("DaemonSet", "Deployment"):
                continue
            for c in containers(w):
                self.assertIn("@sha256:", c["image"], c["image"])


if __name__ == "__main__":
    unittest.main()
