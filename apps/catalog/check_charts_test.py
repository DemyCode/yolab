import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import ClassVar

import check_charts
import yaml

EXPLORER_ON = {"properties": {"file_explorer_enabled": {"const": True}}}


def schema(config=None, outputs=None, extra=None):
    props = {"config": {"type": "object", "properties": config or {}}}
    if outputs is not None:
        props["outputs"] = {"type": "object", "readOnly": True, "properties": outputs}
    props.update(extra or {})
    return {"type": "object", "properties": props}


DEMO_CHART = (
    'apiVersion: v2\nname: demo\nannotations:\n  yolab.io/tagline: "A demo app"\n'
)


def failures(s, chart_yaml=DEMO_CHART):
    found = []
    check_charts.check_schema("demo", s, chart_yaml, lambda app, msg: found.append(msg))
    return found


def logs(pattern, **more):
    return {"type": "string", "title": "Shown", "source": {"logs": pattern}, **more}


GOOD = schema(
    config={
        "subdomain": {"type": "string", "format": "tunnel"},
        "password": {"type": "string", "writeOnly": True, "generate": True},
        "pin": {"type": "string", "writeOnly": True},
        "file_explorer_enabled": {"type": "boolean", "default": True},
    },
    outputs={
        "url": logs(r"YOLAB_OUTPUT url (\S+)", format="uri"),
        "password": {
            "type": "string",
            "title": "Admin password",
            "format": "secret",
            "source": {"config": "password"},
        },
        "file_explorer_password": logs(
            r"YOLAB_OUTPUT file_explorer_password (\S+)",
            format="secret",
            when=EXPLORER_ON,
        ),
    },
)


class CheckSchema(unittest.TestCase):
    def test_a_well_formed_app_passes(self):
        self.assertEqual(failures(GOOD), [])

    def test_an_app_with_no_outputs_passes(self):
        self.assertEqual(failures(schema(config={"subdomain": {"type": "string"}})), [])

    def test_the_old_annotations_are_refused(self):
        chart = (
            "annotations:\n  yolab.io/tagline: demo\n"
            "  yolab.io/outputs: |\n    []\n  yolab.io/uischema: |\n    {}\n"
        )
        found = failures(GOOD, chart)
        self.assertEqual(len(found), 2)
        self.assertTrue(all("values.schema.json" in f for f in found))

    def test_an_app_without_a_tagline_is_refused(self):
        found = failures(GOOD, "apiVersion: v2\nname: demo\n")
        self.assertEqual(len(found), 1)
        self.assertIn("yolab.io/tagline", found[0])

    def test_a_tagline_too_long_for_a_store_card_is_refused(self):
        chart = "annotations:\n  yolab.io/tagline: " + "x" * 61 + "\n"
        found = failures(GOOD, chart)
        self.assertEqual(len(found), 1)
        self.assertIn("over 60", found[0])

    def test_a_github_path_must_be_owner_slash_repo(self):
        for good in ("immich-app/immich", "dgtlmoon/changedetection.io"):
            chart = DEMO_CHART + f"  yolab.io/github: {good}\n"
            self.assertEqual(failures(GOOD, chart), [], good)
        for bad in ("https://github.com/immich-app/immich", "immich", "a/b/c"):
            chart = DEMO_CHART + f'  yolab.io/github: "{bad}"\n'
            self.assertEqual(len(failures(GOOD, chart)), 1, bad)

    def test_only_known_collections_may_be_named(self):
        chart = DEMO_CHART + '  yolab.io/collections: "start-here,family"\n'
        self.assertEqual(failures(GOOD, chart), [])
        chart = DEMO_CHART + '  yolab.io/collections: "start-here,best-ever"\n'
        found = failures(GOOD, chart)
        self.assertEqual(len(found), 1)
        self.assertIn("best-ever", found[0])

    def test_the_platform_section_is_refused(self):
        found = failures(schema(extra={"yolab": {"type": "object"}}))
        self.assertTrue(any("`yolab`" in f for f in found))

    def test_two_address_fields_are_refused(self):
        tunnel = {"type": "string", "format": "tunnel"}
        found = failures(schema(config={"a": tunnel, "b": tunnel}))
        self.assertTrue(any("more than one address field" in f for f in found))

    def test_a_generated_value_must_be_a_credential(self):
        found = failures(schema(config={"pin": {"type": "string", "generate": True}}))
        self.assertTrue(any("not marked writeOnly" in f for f in found))

    def test_an_output_needs_a_title(self):
        found = failures(schema(outputs={"x": logs(r"x (\S+)", title="  ")}))
        self.assertTrue(any("no title" in f for f in found))

    def test_an_unknown_format_is_refused(self):
        found = failures(schema(outputs={"x": logs(r"x (\S+)", format="hologram")}))
        self.assertTrue(any("hologram" in f for f in found))

    def test_an_output_needs_exactly_one_source(self):
        for source in (None, {}, {"logs": r"x (\S+)", "config": "a"}):
            out = {"type": "string", "title": "X"}
            if source is not None:
                out["source"] = source
            found = failures(schema(outputs={"x": out}))
            self.assertTrue(any("exactly one source" in f for f in found), source)

    def test_an_unknown_source_is_refused(self):
        out = {"type": "string", "title": "X", "source": {"env": "X"}}
        self.assertTrue(
            any("unknown source" in f for f in failures(schema(outputs={"x": out})))
        )

    def test_a_logs_pattern_must_compile(self):
        found = failures(schema(outputs={"x": logs("(")}))
        self.assertTrue(any("does not compile" in f for f in found))

    def test_a_logs_pattern_must_capture_the_value(self):
        found = failures(schema(outputs={"x": logs(r"YOLAB_OUTPUT x \S+")}))
        self.assertTrue(any("capture group" in f for f in found))

    def test_a_config_output_must_name_a_real_setting(self):
        out = {"type": "string", "title": "X", "source": {"config": "nope"}}
        found = failures(schema(outputs={"x": out}))
        self.assertTrue(any("does not exist" in f for f in found))

    def test_a_config_output_must_be_a_generated_value(self):
        config = {"pin": {"type": "string", "writeOnly": True}}
        out = {"type": "string", "title": "PIN", "source": {"config": "pin"}}
        found = failures(schema(config=config, outputs={"pin": out}))
        self.assertTrue(any("typed themselves" in f for f in found))

    def test_when_must_be_a_schema(self):
        found = failures(schema(outputs={"x": logs(r"x (\S+)", when="yes")}))
        self.assertTrue(any("must be a schema" in f for f in found))


def rendered_explorer():
    def container(name):
        return {
            "name": name,
            "image": f"example/{name}@sha256:0",
            "imagePullPolicy": "IfNotPresent",
        }

    return [
        {
            "kind": "Deployment",
            "metadata": {"name": "gateway"},
            "spec": {
                "template": {
                    "metadata": {"labels": {"app": "gateway"}},
                    "spec": {
                        "initContainers": [
                            container("wg-register"),
                            container("file-explorer-init"),
                        ],
                        "containers": [],
                    },
                }
            },
        }
    ]


EXPLORER_URL = logs(
    r"YOLAB_OUTPUT file_explorer_url (\S+)",
    format="uri",
    when=check_charts.EXPLORER_PUBLIC,
)
EXPLORER_PASSWORD = logs(
    r"YOLAB_OUTPUT file_explorer_password (\S+)", format="secret", when=EXPLORER_ON
)
EXPLORER_PRIVATE = {
    "file_explorer_tor_url": logs(
        r"YOLAB_OUTPUT file_explorer_tor_url (\S+)",
        format="uri",
        when=check_charts.explorer_way_on("file_explorer_tor_enabled"),
    ),
    "file_explorer_tailscale_url": logs(
        r"YOLAB_OUTPUT file_explorer_tailscale_url (\S+)",
        format="uri",
        when=check_charts.explorer_way_on("file_explorer_tailscale_enabled"),
    ),
}


class FileExplorerOutputs(unittest.TestCase):
    def explorer_failures(self, s):
        found = []
        check_charts.check(
            "demo", rendered_explorer(), lambda app, msg: found.append(msg), "", s
        )
        return [f for f in found if "file explorer" in f or "file_explorer" in f]

    def test_every_explorer_output_with_its_condition_passes(self):
        s = schema(
            outputs={
                "file_explorer_url": EXPLORER_URL,
                "file_explorer_password": EXPLORER_PASSWORD,
                **EXPLORER_PRIVATE,
            }
        )
        self.assertEqual(self.explorer_failures(s), [])

    def test_an_explorer_address_shown_while_its_yolab_address_is_off_is_reported(self):
        s = schema(
            outputs={
                "file_explorer_url": {**EXPLORER_URL, "when": EXPLORER_ON},
                "file_explorer_password": EXPLORER_PASSWORD,
                **EXPLORER_PRIVATE,
            }
        )
        found = self.explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("file_explorer_url", found[0])

    def test_a_missing_explorer_tor_address_is_reported(self):
        s = schema(
            outputs={
                "file_explorer_url": EXPLORER_URL,
                "file_explorer_password": EXPLORER_PASSWORD,
                "file_explorer_tailscale_url": EXPLORER_PRIVATE[
                    "file_explorer_tailscale_url"
                ],
            }
        )
        found = self.explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("do not declare file_explorer_tor_url", found[0])

    def test_a_missing_explorer_output_is_reported(self):
        s = schema(
            outputs={"file_explorer_password": EXPLORER_PASSWORD, **EXPLORER_PRIVATE}
        )
        found = self.explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("do not declare file_explorer_url", found[0])

    def test_an_explorer_output_without_its_condition_is_reported(self):
        unconditioned = logs(r"YOLAB_OUTPUT file_explorer_url (\S+)", format="uri")
        s = schema(
            outputs={
                "file_explorer_url": unconditioned,
                "file_explorer_password": EXPLORER_PASSWORD,
                **EXPLORER_PRIVATE,
            }
        )
        found = self.explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("file_explorer_url", found[0])
        self.assertIn("waits for it forever", found[0])


def explorer_chart(
    listen="127.0.0.1",
    header_up=True,
    protected=("release/postgres",),
    read_only=False,
):
    writable = not read_only
    config = {
        "http": {"listen": listen, "port": 18790},
        "server": {
            "sources": [
                {
                    "path": "/srv/data",
                    "name": "demo",
                    "config": {
                        "readOnly": read_only,
                        "defaultPermissions": {
                            "view": True,
                            "download": True,
                            "modify": writable,
                            "create": writable,
                            "delete": writable,
                        },
                    },
                }
            ]
        },
        "auth": {
            "methods": {
                "password": {"enabled": False},
                "proxy": {"enabled": True, "header": "X-Yolab-User"},
            }
        },
    }
    caddyfile = "files.example {\n  reverse_proxy localhost:18790"
    if header_up:
        caddyfile += " {\n    header_up X-Yolab-User {http.auth.user.id}\n  }"
    caddyfile += "\n}\n"
    data_volume = {
        "name": "data",
        "persistentVolumeClaim": {"claimName": "release-data"},
    }
    explorer_mounts = [
        {
            "name": "data",
            "mountPath": "/srv/data",
            "subPath": "release",
            "readOnly": read_only,
        }
    ] + [
        {"name": "data", "mountPath": f"/srv/data/{p}", "subPath": p, "readOnly": True}
        for p in protected
    ]
    return [
        {
            "kind": "ConfigMap",
            "metadata": {"name": "release-caddy"},
            "data": {"Caddyfile": caddyfile},
        },
        {
            "kind": "ConfigMap",
            "metadata": {"name": "release-file-explorer"},
            "data": {"config.yaml": yaml.safe_dump(config)},
        },
        {
            "kind": "Deployment",
            "metadata": {"name": "gateway"},
            "spec": {
                "template": {
                    "spec": {
                        "initContainers": [{"name": "file-explorer-init"}],
                        "containers": [
                            {
                                "name": "file-explorer",
                                "image": "gtstef/filebrowser:2@sha256:0",
                                "volumeMounts": explorer_mounts,
                            }
                        ],
                        "volumes": [data_volume],
                    }
                }
            },
        },
        {
            "kind": "Deployment",
            "metadata": {"name": "postgres"},
            "spec": {
                "template": {
                    "spec": {
                        "containers": [
                            {
                                "name": "postgres",
                                "image": "postgres:16-alpine@sha256:0",
                                "volumeMounts": [
                                    {
                                        "name": "data",
                                        "mountPath": "/var/lib/postgresql/data",
                                        "subPath": "release/postgres",
                                    }
                                ],
                            }
                        ],
                        "volumes": [data_volume],
                    }
                }
            },
        },
    ]


def explorer_failures(docs, read_only=False):
    found = []
    check = (
        check_charts.check_file_explorer_read_only
        if read_only
        else check_charts.check_file_explorer
    )
    check("demo", docs, lambda app, msg: found.append(msg))
    return found


class FileExplorerContainer(unittest.TestCase):
    def test_a_loopback_explorer_behind_caddy_with_its_database_guarded_passes(self):
        self.assertEqual(explorer_failures(explorer_chart()), [])

    def test_an_explorer_other_pods_can_reach_is_reported(self):
        found = explorer_failures(explorer_chart(listen="0.0.0.0"))
        self.assertEqual(len(found), 1)
        self.assertIn("without the Caddy login", found[0])

    def test_a_user_header_caddy_does_not_overwrite_is_reported(self):
        found = explorer_failures(explorer_chart(header_up=False))
        self.assertEqual(len(found), 1)
        self.assertIn("could send that header itself", found[0])

    def test_a_database_folder_the_explorer_can_write_is_reported(self):
        found = explorer_failures(explorer_chart(protected=()))
        self.assertEqual(len(found), 1)
        self.assertIn("release/postgres", found[0])
        self.assertIn("yolab.fileExplorer.protect", found[0])

    def test_a_read_only_parent_folder_guards_the_database_inside_it(self):
        self.assertEqual(
            explorer_failures(explorer_chart(protected=(), read_only=True)), []
        )

    def test_an_init_without_the_explorer_container_is_reported(self):
        docs = explorer_chart()
        docs[2]["spec"]["template"]["spec"]["containers"] = []
        found = explorer_failures(docs)
        self.assertEqual(len(found), 1)
        self.assertIn("no file-explorer container", found[0])

    def test_the_database_image_is_recognised_whatever_its_registry(self):
        for image in (
            "postgres:16-alpine@sha256:0",
            "ghcr.io/immich-app/postgres:14-vectorchord0.4.3@sha256:0",
            "docker.io/valkey/valkey:8-bookworm@sha256:0",
            "postgis/postgis:17-3.5-alpine@sha256:0",
        ):
            self.assertTrue(
                check_charts.DATABASE_IMAGES.match(check_charts.image_name(image)),
                image,
            )
        self.assertFalse(
            check_charts.DATABASE_IMAGES.match(
                check_charts.image_name(
                    "ghcr.io/umami-software/umami:postgresql-latest"
                )
            )
        )


class FileExplorerReadOnly(unittest.TestCase):
    def test_a_fully_read_only_explorer_passes(self):
        self.assertEqual(
            explorer_failures(explorer_chart(read_only=True), read_only=True), []
        )

    def test_a_writable_mount_under_read_only_is_reported(self):
        found = explorer_failures(explorer_chart(), read_only=True)
        self.assertTrue(
            any("mounts release-data:'release' writable" in f for f in found)
        )

    def test_write_permissions_under_read_only_are_reported(self):
        found = explorer_failures(explorer_chart(), read_only=True)
        self.assertTrue(any("leaves source demo writable" in f for f in found))
        self.assertTrue(any("still grants modify, create, delete" in f for f in found))


def pod(*containers):
    return {
        "kind": "Deployment",
        "spec": {"template": {"spec": {"containers": list(containers)}}},
    }


def shell(name, script):
    return {"name": name, "command": ["/bin/sh", "-c", script]}


GEN = shell("gen", "printf 'DB_PASSWORD=%s\\n' x > /state/secrets.env")


def sourcing_failures(*containers):
    found = []
    check_charts.check_sourced_secrets_reach_the_program(
        "demo", [pod(GEN, *containers)], lambda app, msg: found.append(msg)
    )
    return found


class SourcedSecrets(unittest.TestCase):
    def test_a_sourced_secret_the_program_never_sees_is_reported(self):
        found = sourcing_failures(
            shell("server", ". /state/secrets.env\nexec start.sh\n")
        )
        self.assertEqual(len(found), 1)
        self.assertIn("DB_PASSWORD", found[0])

    def test_set_a_before_sourcing_exports_it(self):
        script = "set -a\n. /state/secrets.env\nset +a\nexec start.sh\n"
        self.assertEqual(sourcing_failures(shell("server", script)), [])

    def test_a_script_that_hands_the_value_on_itself_passes(self):
        script = (
            '. /state/secrets.env\nexport POSTGRES_PASSWORD="$DB_PASSWORD"\n'
            "exec docker-entrypoint.sh postgres\n"
        )
        self.assertEqual(sourcing_failures(shell("postgres", script)), [])

    def test_the_tunnel_env_exports_its_own_values(self):
        script = ". /yolab/env\nexec caddy run\n"
        self.assertEqual(sourcing_failures(shell("caddy", script)), [])

    def test_a_script_given_as_args_is_read_too(self):
        web = {
            "name": "web",
            "command": ["/bin/sh", "-c"],
            "args": [". /yolab/secrets.env\nexec rails server\n"],
        }
        self.assertEqual(len(sourcing_failures(web)), 1)


class Workloads(unittest.TestCase):
    def failures(self, image, node_selector=None, arches=None):
        daemonset = {
            "kind": "DaemonSet",
            "metadata": {"name": "engine"},
            "spec": {
                "template": {
                    "metadata": {"labels": {"app": "engine"}},
                    "spec": {
                        "nodeSelector": node_selector or {},
                        "containers": [
                            {
                                "name": "engine",
                                "image": image,
                                "imagePullPolicy": "IfNotPresent",
                            }
                        ],
                    },
                }
            },
        }
        service = {
            "kind": "Service",
            "metadata": {"name": "engine"},
            "spec": {"selector": {"app": "engine"}, "ports": [{"port": 1}]},
        }
        found = []
        check_charts.check(
            "demo",
            rendered_explorer() + [daemonset, service],
            lambda app, msg: found.append(msg),
            arches=arches
            if arches is not None
            else {
                "example/engine@sha256:0": ["amd64", "arm64"],
                "example/engine:latest": ["amd64", "arm64"],
            },
        )
        return [f for f in found if "engine" in f]

    def test_a_service_may_front_a_daemonset(self):
        self.assertEqual(self.failures("example/engine@sha256:0"), [])

    def test_a_daemonset_image_must_be_pinned_like_any_other(self):
        found = self.failures("example/engine:latest")
        self.assertEqual(len(found), 1)
        self.assertIn("not digest-pinned", found[0])


X86_ONLY = {"example/engine@sha256:0": ["amd64"]}


class Architectures(Workloads):
    def test_an_x86_only_image_must_be_pinned_to_x86_machines(self):
        found = self.failures("example/engine@sha256:0", arches=X86_ONLY)
        self.assertEqual(len(found), 1)
        self.assertIn("no arm64 build", found[0])

    def test_an_x86_only_image_pinned_to_x86_machines_passes(self):
        pinned = {"kubernetes.io/arch": "amd64"}
        self.assertEqual(
            self.failures("example/engine@sha256:0", pinned, arches=X86_ONLY), []
        )

    def test_an_image_nobody_recorded_is_refused(self):
        found = self.failures("example/engine@sha256:0", arches={})
        self.assertEqual(len(found), 1)
        self.assertIn("image_arches.py", found[0])

    def test_yolab_s_own_images_are_built_for_both_and_need_no_record(self):
        own = "ghcr.io/demycode/engine@sha256:0"
        self.assertEqual(self.failures(own, arches={}), [])

    def test_an_official_image_without_a_namespace_is_found(self):
        import image_arches

        digest = "@sha256:" + "a" * 64
        found = image_arches.PINNED.findall(
            f"image: caddy:2{digest}\n  image: ghcr.io/x/y:1{digest}"
        )
        self.assertEqual(found, [f"caddy:2{digest}", f"ghcr.io/x/y:1{digest}"])

    def test_every_pinned_catalog_image_is_recorded(self):
        import image_arches

        missing = [
            i for i in image_arches.pinned_images() if i not in check_charts.ARCHES
        ]
        self.assertEqual(missing, [])


class RenderedChart(unittest.TestCase):
    CHART = None

    @classmethod
    def setUpClass(cls):
        import glob
        import os
        import subprocess
        import tempfile

        cls.tmp = tempfile.TemporaryDirectory()
        version = check_charts.chart_field(
            Path(check_charts.LIBRARY, "Chart.yaml").read_text(), "version"
        )
        subprocess.run(
            [
                "helm",
                "package",
                check_charts.LIBRARY,
                "--version",
                version,
                "--destination",
                cls.tmp.name,
            ],
            check=True,
            capture_output=True,
        )
        cls.library = glob.glob(os.path.join(cls.tmp.name, "yolab-common-*.tgz"))[0]
        cls.chart = os.path.join(check_charts.HERE, cls.CHART)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def docs(self, extra=None):
        text, err = check_charts.render(self.chart, self.library, self.tmp.name, extra)
        self.assertIsNone(err)
        return [d for d in yaml.safe_load_all(text) if d]

    def deployments(self, docs):
        return {
            d["metadata"]["name"]: d["spec"]["template"]["spec"]
            for d in docs
            if d["kind"] == "Deployment"
        }

    def env(self, spec, container):
        c = next(c for c in spec["containers"] if c["name"] == container)
        return {e["name"]: e.get("value") for e in c.get("env") or []}


class OpenWebUiEngines(RenderedChart):
    CHART = "open-webui"

    def engines(self, docs):
        return {
            d["metadata"]["name"]: d["spec"]["template"]["spec"]
            for d in docs
            if d["kind"] == "DaemonSet"
        }

    def gpu_cluster(self):
        return self.docs(check_charts.VARIANTS["open-webui"][0])

    def test_without_a_gpu_ollama_runs_beside_the_ui(self):
        docs = self.docs()
        pods = self.deployments(docs)
        self.assertEqual(list(pods), ["gateway"])
        self.assertEqual(self.engines(docs), {})
        names = [c["name"] for c in pods["gateway"]["containers"]]
        self.assertIn("ollama", names)
        self.assertEqual(
            self.env(pods["gateway"], "open-webui")["OLLAMA_BASE_URL"],
            "http://localhost:11434",
        )

    def test_each_vendor_s_engine_lands_only_where_that_vendor_is_the_best_card(self):
        engines = self.engines(self.gpu_cluster())
        self.assertEqual(
            sorted(engines),
            ["ollama-amd", "ollama-intel", "ollama-nvidia", "ollama-vulkan"],
        )
        nvidia = engines["ollama-nvidia"]
        self.assertEqual(nvidia["nodeSelector"], {"yolab.io/accelerator": "nvidia"})
        self.assertEqual(
            nvidia["containers"][0]["resources"]["limits"], {"nvidia.com/gpu-all": "1"}
        )
        amd = engines["ollama-amd"]
        self.assertEqual(
            amd["nodeSelector"],
            {"yolab.io/accelerator": "amd", "kubernetes.io/arch": "amd64"},
        )
        self.assertIn("-rocm@sha256:", amd["containers"][0]["image"])
        self.assertEqual(
            amd["containers"][0]["resources"]["limits"], {"yolab.io/kfd": "1"}
        )

    def test_the_nvidia_engine_also_runs_on_arm_machines_like_a_dgx_spark(self):
        nvidia = self.engines(self.gpu_cluster())["ollama-nvidia"]
        self.assertNotIn("kubernetes.io/arch", nvidia["nodeSelector"])
        self.assertIn(
            "arm64",
            check_charts.ARCHES[nvidia["containers"][0]["image"]],
        )

    def test_a_gpu_machine_that_joins_later_gets_an_engine_without_a_re_render(self):
        nvidia_only = {
            "machines[0].name": "gpu-box",
            "machines[0].accelerator": "nvidia",
        }
        self.assertEqual(
            sorted(self.engines(self.docs(nvidia_only))),
            ["ollama-amd", "ollama-intel", "ollama-nvidia", "ollama-vulkan"],
        )

    def test_the_ui_talks_to_every_engine_through_one_service(self):
        docs = self.gpu_cluster()
        pods = self.deployments(docs)
        self.assertNotIn("ollama", [c["name"] for c in pods["gateway"]["containers"]])
        self.assertEqual(
            self.env(pods["gateway"], "open-webui")["OLLAMA_BASE_URL"],
            "http://ollama:11434",
        )
        service = next(
            d
            for d in docs
            if d["kind"] == "Service" and d["metadata"]["name"] == "ollama"
        )
        self.assertEqual(service["spec"]["selector"], {"app": "ollama"})

    def test_every_engine_shares_one_model_store_and_never_prunes_another_s_download(
        self,
    ):
        for name, spec in self.engines(self.gpu_cluster()).items():
            c = spec["containers"][0]
            self.assertEqual(self.env(spec, "ollama")["OLLAMA_NOPRUNE"], "1", name)
            self.assertEqual(
                [(m["mountPath"], m["subPath"]) for m in c["volumeMounts"]],
                [("/root/.ollama", "release/ollama")],
            )

    def test_how_long_a_model_holds_the_card_is_the_user_s_choice_on_every_engine(
        self,
    ):
        extra = {**check_charts.VARIANTS["open-webui"][0], "config.unload_after": "1m"}
        for spec in self.engines(self.docs(extra)).values():
            self.assertEqual(self.env(spec, "ollama")["OLLAMA_KEEP_ALIVE"], "1m")
        cpu = self.deployments(self.docs({"config.unload_after": "30m"}))["gateway"]
        self.assertEqual(self.env(cpu, "ollama")["OLLAMA_KEEP_ALIVE"], "30m")

    def test_an_intel_machine_runs_the_vulkan_engine_on_x86_only(self):
        intel = {"machines[0].name": "nuc", "machines[0].accelerator": "intel"}
        engine = self.engines(self.docs(intel))["ollama-intel"]
        c = engine["containers"][0]
        self.assertTrue(c["image"].startswith("ghcr.io/demycode/ollama-vulkan:"))
        self.assertIn("@sha256:", c["image"])
        self.assertEqual(c["resources"]["limits"], {"yolab.io/dri": "1"})
        self.assertEqual(
            engine["nodeSelector"],
            {"yolab.io/accelerator": "intel", "kubernetes.io/arch": "amd64"},
        )

    def test_without_a_vulkan_image_an_intel_machine_gets_no_engine(self):
        intel = {
            "machines[0].name": "nuc",
            "machines[0].accelerator": "intel",
            "vulkanImage": "",
        }
        self.assertEqual(self.engines(self.docs(intel)), {})


class SteamHeadless(RenderedChart):
    CHART = "steam-headless"

    def game(self, extra=None):
        return self.deployments(self.docs(extra))["steam-headless"]

    def test_an_nvidia_machine_is_used_through_its_cdi_device(self):
        spec = self.game(check_charts.VARIANTS["steam-headless"][0])
        self.assertEqual(
            spec["nodeSelector"],
            {
                "yolab.io/game-input": "true",
                "kubernetes.io/arch": "amd64",
                "kubernetes.io/hostname": "gpu-box",
            },
        )
        self.assertEqual(
            spec["containers"][0]["resources"]["limits"],
            {"yolab.io/uinput": "1", "nvidia.com/gpu-all": "1"},
        )

    def test_an_intel_or_amd_machine_is_used_through_dev_dri(self):
        spec = self.game(check_charts.VARIANTS["steam-headless"][1])
        self.assertEqual(
            spec["containers"][0]["resources"]["limits"],
            {"yolab.io/uinput": "1", "yolab.io/dri": "1"},
        )

    def test_an_older_nvidia_card_is_pinned_and_used_through_its_cdi_device(self):
        spec = self.game(check_charts.VARIANTS["steam-headless"][2])
        self.assertEqual(spec["nodeSelector"]["kubernetes.io/hostname"], "gt710")
        self.assertEqual(
            spec["containers"][0]["resources"]["limits"],
            {"yolab.io/uinput": "1", "nvidia.com/gpu-all": "1"},
        )

    def test_without_a_known_gpu_it_still_lands_where_game_input_exists(self):
        spec = self.game()
        self.assertEqual(
            spec["nodeSelector"],
            {"yolab.io/game-input": "true", "kubernetes.io/arch": "amd64"},
        )
        self.assertEqual(
            spec["containers"][0]["resources"]["limits"], {"yolab.io/uinput": "1"}
        )

    def test_the_desktop_is_reachable_only_through_the_login(self):
        spec = self.game(check_charts.VARIANTS["steam-headless"][0])
        self.assertFalse(spec.get("hostNetwork", False))
        published = {
            p["containerPort"]
            for p in spec["containers"][0]["ports"]
            if "hostPort" in p
        }
        self.assertNotIn(8083, published)
        self.assertEqual(published, {47984, 47989, 47990, 48010, 47998, 47999, 48000})
        caddy = next(
            d["data"]["Caddyfile"]
            for d in self.docs()
            if d["kind"] == "ConfigMap" and "Caddyfile" in (d.get("data") or {})
        )
        self.assertIn("forward_auth", caddy)


class JellyfinTranscoding(RenderedChart):
    CHART = "jellyfin"

    def jellyfin(self, extra=None):
        spec = self.deployments(self.docs(extra))["gateway"]
        return spec, next(c for c in spec["containers"] if c["name"] == "jellyfin")

    def test_an_intel_machine_lends_jellyfin_its_dev_dri(self):
        spec, c = self.jellyfin(check_charts.VARIANTS["jellyfin"][0])
        self.assertEqual(spec["nodeSelector"], {"kubernetes.io/hostname": "nuc"})
        self.assertEqual(c["resources"]["limits"], {"yolab.io/dri": "1"})

    def test_an_nvidia_machine_lends_jellyfin_its_cdi_device_and_driver(self):
        spec, c = self.jellyfin(check_charts.VARIANTS["jellyfin"][1])
        self.assertEqual(c["resources"]["limits"], {"nvidia.com/gpu-all": "1"})
        self.assertEqual(
            self.env(spec, "jellyfin")["NVIDIA_DRIVER_CAPABILITIES"], "all"
        )

    def test_turning_it_off_leaves_jellyfin_free_to_run_anywhere(self):
        off = {
            **check_charts.VARIANTS["jellyfin"][0],
            "config.hardware_transcoding": "false",
        }
        spec, c = self.jellyfin(off)
        self.assertNotIn("nodeSelector", spec)
        self.assertNotIn("resources", c)

    def test_with_no_gpu_in_the_cluster_nothing_changes(self):
        spec, c = self.jellyfin()
        self.assertNotIn("nodeSelector", spec)
        self.assertNotIn("resources", c)


class ImmichMachineLearning(RenderedChart):
    CHART = "immich"

    def ml(self, extra=None):
        spec = self.deployments(self.docs(extra))["immich-ml"]
        return spec, spec["containers"][0]

    def test_an_nvidia_machine_runs_the_cuda_build_on_its_device(self):
        spec, c = self.ml(check_charts.VARIANTS["immich"][0])
        self.assertIn(":release-cuda@sha256:", c["image"])
        self.assertEqual(
            spec["nodeSelector"],
            {"kubernetes.io/hostname": "gpu-box", "kubernetes.io/arch": "amd64"},
        )
        self.assertEqual(c["resources"]["limits"], {"nvidia.com/gpu-all": "1"})

    def test_an_intel_machine_runs_the_openvino_build_on_dev_dri(self):
        _, c = self.ml(check_charts.VARIANTS["immich"][1])
        self.assertIn(":release-openvino@sha256:", c["image"])
        self.assertEqual(c["resources"]["limits"], {"yolab.io/dri": "1"})

    def test_an_intel_machine_is_chosen_by_its_compute_label_not_its_video_one(self):
        gpu = Path(check_charts.HERE, "immich", "templates", "_gpu.tpl").read_text()
        self.assertIn('"intel" "yolab.io/gpu-intel-compute"', gpu)
        self.assertNotIn('printf "yolab.io/gpu-%s"', gpu)

    def test_without_a_gpu_it_keeps_the_cpu_build_and_runs_anywhere(self):
        spec, c = self.ml()
        self.assertIn(":release@sha256:", c["image"])
        self.assertNotIn("nodeSelector", spec)
        self.assertNotIn("resources", c)


class FrigateAcceleration(RenderedChart):
    CHART = "frigate"

    def seeded(self, extra=None):
        spec = self.deployments(self.docs(extra))["gateway"]
        seed = next(c for c in spec["initContainers"] if c["name"] == "seed-config")
        script = seed["command"][2]
        body = script.split("<<'EOF'\n", 1)[1].split("\nEOF", 1)[0]
        frigate = next(c for c in spec["containers"] if c["name"] == "frigate")
        return spec, frigate, yaml.safe_load(body)

    def test_an_intel_machine_decodes_with_vaapi_and_detects_with_openvino(self):
        spec, frigate, config = self.seeded(check_charts.VARIANTS["frigate"][0])
        self.assertEqual(spec["nodeSelector"], {"kubernetes.io/hostname": "nuc"})
        self.assertEqual(frigate["resources"]["limits"], {"yolab.io/dri": "1"})
        self.assertEqual(config["ffmpeg"]["hwaccel_args"], "preset-vaapi")
        self.assertEqual(
            config["detectors"]["ov"], {"type": "openvino", "device": "GPU"}
        )

    def test_an_nvidia_machine_decodes_with_nvdec(self):
        _, frigate, config = self.seeded(check_charts.VARIANTS["frigate"][1])
        self.assertEqual(frigate["resources"]["limits"], {"nvidia.com/gpu-all": "1"})
        self.assertEqual(config["ffmpeg"]["hwaccel_args"], "preset-nvidia")
        self.assertNotIn("detectors", config)

    def test_without_a_gpu_the_first_config_is_unchanged(self):
        spec, frigate, config = self.seeded()
        self.assertNotIn("nodeSelector", spec)
        self.assertNotIn("resources", frigate)
        self.assertEqual(
            config,
            {"mqtt": {"enabled": False}, "tls": {"enabled": False}, "cameras": {}},
        )


class MinioStorage:
    def minio(self):
        spec = self.deployments(self.docs())["minio"]
        return spec, spec["containers"][0]

    def test_minio_comes_from_an_image_that_still_exists_for_both_processors(self):
        spec, c = self.minio()
        self.assertTrue(c["image"].startswith("cgr.dev/chainguard/minio:"))
        self.assertEqual(check_charts.ARCHES[c["image"]], ["amd64", "arm64"])
        self.assertNotIn("kubernetes.io/arch", spec.get("nodeSelector") or {})

    def test_minio_keeps_reading_the_data_root_wrote_before(self):
        _, c = self.minio()
        self.assertEqual(c["securityContext"], {"runAsUser": 0, "runAsGroup": 0})

    def test_minio_is_started_by_name_because_the_image_s_entrypoint_is_minio_itself(
        self,
    ):
        _, c = self.minio()
        self.assertEqual(c["command"][:3], ["minio", "server", "/data"])


class AppflowyMinio(MinioStorage, RenderedChart):
    CHART = "appflowy"

    def caddyfile(self):
        return next(
            d["data"]["Caddyfile"]
            for d in self.docs()
            if d["kind"] == "ConfigMap" and "Caddyfile" in (d.get("data") or {})
        )

    def test_the_public_minio_route_forwards_only_presigned_requests(self):
        caddy = self.caddyfile()
        matcher = next(
            line for line in caddy.splitlines() if line.strip().startswith("@presigned")
        )
        self.assertIn('path("/minio-api/*")', matcher)
        self.assertIn('{query.X-Amz-Signature} != ""', matcher)
        self.assertIn('{header.Authorization} == ""', matcher)
        presigned = caddy.split("handle @presigned {", 1)[1].split("}", 1)[0]
        self.assertIn("reverse_proxy minio:9000", presigned)
        self.assertEqual(caddy.count("reverse_proxy minio:9000"), 1)

    def test_anything_else_on_the_minio_route_is_refused(self):
        caddy = self.caddyfile()
        refused = caddy.split("handle /minio-api/* {", 1)[1].split("}", 1)[0]
        self.assertIn("respond 403", refused)
        self.assertLess(
            caddy.index("handle @presigned"), caddy.index("handle /minio-api/*")
        )


class ReactiveResumeMinio(MinioStorage, RenderedChart):
    CHART = "reactive-resume"


PA_VALUES = "yolab:\n  gateway:\n    upstream: localhost:8080\n"
TOR_SWITCH = {"type": "boolean", "default": False}
TS_SWITCH = {"type": "boolean", "default": False}


def pa_output(key):
    return logs(
        rf"YOLAB_OUTPUT {key}_url (\S+)",
        format="uri",
        when={"properties": {f"{key}_enabled": {"const": True}}},
    )


def offer_failures(s, values=PA_VALUES):
    found = []
    check_charts.check_private_access_offer(
        "demo", s, values, lambda app, msg: found.append(msg)
    )
    return found


class PrivateAccessOffer(unittest.TestCase):
    def offered(self, config=None, outputs=None):
        return schema(
            config={
                "tor_enabled": TOR_SWITCH,
                "tailscale_enabled": TS_SWITCH,
                **(config or {}),
            },
            outputs=outputs
            if outputs is not None
            else {"tor_url": pa_output("tor"), "tailscale_url": pa_output("tailscale")},
        )

    def test_an_app_offering_both_with_their_addresses_passes(self):
        self.assertEqual(offer_failures(self.offered()), [])

    def test_an_app_offering_nothing_is_not_judged(self):
        self.assertEqual(offer_failures(schema(), values=""), [])

    def test_offering_it_next_to_an_authelia_login_is_refused(self):
        found = offer_failures(
            self.offered(config={"auth_enabled": {"type": "boolean"}})
        )
        self.assertTrue(any("skip the login" in f for f in found), found)

    def test_offering_it_with_a_custom_caddyfile_is_refused(self):
        found = offer_failures(
            self.offered(), values="yolab:\n  gateway:\n    caddyfile: ':80 {}'\n"
        )
        self.assertTrue(any("own Caddyfile" in f for f in found), found)

    def test_an_address_never_shown_is_reported(self):
        found = offer_failures(self.offered(outputs={"tor_url": pa_output("tor")}))
        self.assertEqual(len(found), 1)
        self.assertIn("tailscale_url", found[0])

    def test_an_address_shown_while_switched_off_is_reported(self):
        always = logs(r"YOLAB_OUTPUT tor_url (\S+)", format="uri")
        found = offer_failures(
            self.offered(
                outputs={"tor_url": always, "tailscale_url": pa_output("tailscale")}
            )
        )
        self.assertEqual(len(found), 1)
        self.assertIn("only show when tor_enabled is on", found[0])


PRIVATE_BOTH = frozenset(
    {
        "actual",
        "audiobookshelf",
        "calibre-web",
        "changedetection",
        "cinny",
        "code-server",
        "copyparty",
        "freshrss",
        "grafana",
        "grocy",
        "home-assistant",
        "it-tools",
        "jellyseerr",
        "kavita",
        "mealie",
        "memos",
        "n8n",
        "navidrome",
        "open-webui",
        "romm",
        "searxng",
        "stirling-pdf",
        "uptime-kuma",
        "vaultwarden",
        "vikunja",
        "wallos",
        "filebrowser",
        "jellyfin",
        "immich",
        "photoprism",
        "frigate",
    }
)

PRIVATE_TAILSCALE_ONLY = frozenset({"ollama"})


class PrivateAccessChoice(unittest.TestCase):
    def offers(self):

        found = {}
        for path in Path(check_charts.HERE).glob("*/values.schema.json"):
            offered = check_charts.offered_private_access(json.loads(path.read_text()))
            if offered:
                found[path.parent.name] = set(offered)
        return found

    def test_tor_and_tailscale_are_offered_on_apps_that_work_on_any_address(self):
        offers = self.offers()
        both = {
            a for a, o in offers.items() if o == {"tor_enabled", "tailscale_enabled"}
        }
        self.assertEqual(both, PRIVATE_BOTH)

    def test_apps_offering_one_way_in_offer_both_except_apis_that_offer_only_tailscale(
        self,
    ):
        offers = self.offers()
        partial = {
            a for a, o in offers.items() if o != {"tor_enabled", "tailscale_enabled"}
        }
        self.assertEqual(partial, PRIVATE_TAILSCALE_ONLY)
        for app in PRIVATE_TAILSCALE_ONLY:
            self.assertEqual(offers[app], {"tailscale_enabled"})

    def test_apps_that_pin_their_own_address_offer_neither(self):
        offers = self.offers()
        for pinned in ("nextcloud", "gitea", "paperless-ngx", "bookstack", "mastodon"):
            self.assertNotIn(pinned, offers)


PRIVATE_ON = {
    "config.tor_enabled": "true",
    "config.tailscale_enabled": "true",
    "config.tailscale_auth_key": "tskey-auth-placeholder",
}


class PrivateAccessRendered(RenderedChart):
    CHART = "vaultwarden"

    def names(self, docs):
        spec = self.deployments(docs)["gateway"]
        return {c["name"] for c in spec["containers"]}, {
            c["name"] for c in spec.get("initContainers") or []
        }

    def test_switched_off_nothing_extra_runs(self):
        docs = self.docs()
        containers, inits = self.names(docs)
        self.assertFalse({"tor", "tailscale"} & containers)
        self.assertNotIn("tor-state", inits)
        caddyfile = check_charts.configmap_data(docs, "-caddy", "Caddyfile")
        self.assertNotIn("bind 127.0.0.1", caddyfile)

    def test_switched_on_both_sit_next_to_caddy_and_pass_the_checks(self):
        docs = self.docs(PRIVATE_ON)
        containers, inits = self.names(docs)
        self.assertTrue({"caddy", "tor", "tailscale"} <= containers)
        self.assertIn("tor-state", inits)
        found = []
        check_charts.check_private_access(
            "vaultwarden",
            docs,
            ["tor_enabled", "tailscale_enabled"],
            lambda app, msg: found.append(msg),
        )
        self.assertEqual(found, [])

    def test_tailscale_cannot_be_switched_on_without_a_key(self):
        text, err = check_charts.render(
            self.chart,
            self.library,
            self.tmp.name,
            {"config.tailscale_enabled": "true"},
        )
        self.assertIsNone(text)
        self.assertIn("tailscale_auth_key", err)

    def test_the_file_explorer_cannot_change_the_onion_key_or_tailscale_identity(self):
        spec = self.deployments(
            self.docs({**PRIVATE_ON, **check_charts.EXPLORER_ENABLED})
        )["gateway"]
        explorer = next(c for c in spec["containers"] if c["name"] == "file-explorer")
        read_only = {
            m.get("subPath")
            for m in explorer["volumeMounts"]
            if m.get("readOnly") is True
        }
        self.assertIn("release/tor", read_only)
        self.assertIn("release/tailscale", read_only)

    def init(self, docs, name):
        spec = self.deployments(docs)["gateway"]
        return next(c for c in spec["initContainers"] if c["name"] == name)

    def test_an_onion_key_nobody_claims_for_this_namespace_is_reported(self):
        docs = self.docs(PRIVATE_ON)
        init = self.init(docs, "tor-state")
        init["command"][-1] = init["command"][-1].replace(
            "claim_state /var/lib/tor\n", ""
        )
        found = []
        check_charts.check_private_access(
            "vaultwarden", docs, ["tor_enabled"], lambda app, msg: found.append(msg)
        )
        self.assertEqual(len(found), 1, found)
        self.assertIn("a copy would run with the original's identity", found[0])

    def test_a_copied_identity_is_wiped_and_this_namespace_s_own_is_kept(self):
        script = self.init(self.docs(PRIVATE_ON), "tor-state")["command"][-1]
        start = script.index("claim_state() {")
        claim = script[start : script.index("\n}\n", start) + 3]
        with tempfile.TemporaryDirectory() as root:

            def state(name, owner):
                d = os.path.join(root, name)
                os.makedirs(os.path.join(d, "service"))
                Path(d, "service", "hs_ed25519_secret_key").write_text("key")
                if owner:
                    Path(d, ".yolab-owner").write_text(owner + "\n")
                return d

            copied = state("copied", "yolab-vaultwarden-ab12")
            own = state("own", "yolab-vaultwarden-cd34")
            legacy = state("legacy", None)
            calls = "".join(f'claim_state "{d}"\n' for d in (copied, own, legacy))
            subprocess.run(
                ["sh", "-c", f"set -eu\n{claim}{calls}"],
                env={**os.environ, "POD_NAMESPACE": "yolab-vaultwarden-cd34"},
                check=True,
                capture_output=True,
            )
            for d in (copied, own, legacy):
                self.assertEqual(
                    Path(d, ".yolab-owner").read_text(), "yolab-vaultwarden-cd34\n", d
                )
            self.assertFalse(
                Path(copied, "service").exists(), "the copy gets a new onion"
            )
            self.assertTrue(Path(own, "service", "hs_ed25519_secret_key").exists())
            self.assertTrue(
                Path(legacy, "service", "hs_ed25519_secret_key").exists(),
                "state from before ownership is this app's own",
            )

    def test_a_sidecar_pointed_elsewhere_is_reported(self):
        docs = self.docs(PRIVATE_ON)
        for d in docs:
            if d.get("kind") == "ConfigMap" and d["metadata"]["name"].endswith("-tor"):
                d["data"]["torrc"] = d["data"]["torrc"].replace("18792", "8080")
        found = []
        check_charts.check_private_access(
            "vaultwarden", docs, ["tor_enabled"], lambda app, msg: found.append(msg)
        )
        self.assertEqual(len(found), 1)
        self.assertIn("onion service does not point at Caddy", found[0])


YOLAB_TUNNEL = {"type": "string", "title": "Subdomain", "format": "tunnel"}
YOLAB_TOKEN = {"type": "string", "format": "yolab-token", "writeOnly": True}


def yolab_switch_schema(
    top_tunnel=False, off_required=True, token=YOLAB_TOKEN, url_when=True
):
    off = {"properties": {"yolab_enabled": {"const": False}}}
    if off_required:
        off["required"] = ["yolab_enabled"]
    on_props = {"yolab_enabled": {"const": True}, "subdomain": YOLAB_TUNNEL}
    if token is not None:
        on_props["yolab_token"] = token
    config = {"yolab_enabled": {"type": "boolean", "default": True}}
    if top_tunnel:
        config["subdomain"] = YOLAB_TUNNEL
    s = schema(
        config=config,
        outputs={
            "url": logs(
                r"YOLAB_OUTPUT url (\S+)",
                format="uri",
                **({"when": check_charts.YOLAB_ON} if url_when else {}),
            )
        },
    )
    s["properties"]["config"]["dependencies"] = {
        "yolab_enabled": {
            "oneOf": [off, {"properties": on_props, "required": ["subdomain"]}]
        }
    }
    return s


class YolabSwitch(unittest.TestCase):
    def test_a_switch_hiding_the_subdomain_and_token_passes(self):
        self.assertEqual(failures(yolab_switch_schema()), [])

    def test_a_subdomain_shown_while_off_is_refused(self):
        found = failures(yolab_switch_schema(top_tunnel=True))
        self.assertTrue(
            any("shown even with the YoLab address off" in f for f in found), found
        )

    def test_an_off_branch_that_old_installs_also_match_is_refused(self):
        found = failures(yolab_switch_schema(off_required=False))
        self.assertTrue(any("upgrade is refused" in f for f in found), found)

    def test_the_token_field_the_box_fills_in_is_required(self):
        found = failures(yolab_switch_schema(token=None))
        self.assertTrue(any("yolab_token" in f for f in found), found)

    def test_the_address_is_not_shown_while_switched_off(self):
        found = failures(yolab_switch_schema(url_when=False))
        self.assertTrue(
            any("only show when yolab_enabled is on" in f for f in found), found
        )

    def test_every_app_with_a_subdomain_has_the_switch(self):

        for path in Path(check_charts.HERE).glob("*/values.schema.json"):
            config = json.loads(path.read_text())["properties"]["config"]
            deps = config.get("dependencies") or {}
            has_address = "yolab_enabled" in deps or any(
                p.get("format") == "tunnel"
                for p in (config.get("properties") or {}).values()
            )
            if has_address:
                self.assertIn("yolab_enabled", config["properties"], path.parent.name)


YOLAB_OFF = {**PRIVATE_ON, "config.yolab_enabled": "false"}


class YolabOffRendered(RenderedChart):
    CHART = "vaultwarden"

    def test_on_by_default_the_tunnel_is_registered_as_before(self):
        spec = self.deployments(self.docs())["gateway"]
        self.assertIn("wg-register", {c["name"] for c in spec["initContainers"]})
        self.assertIn("wireguard", {c["name"] for c in spec["containers"]})

    def test_off_no_tunnel_wireguard_explorer_or_public_site(self):
        docs = self.docs(YOLAB_OFF)
        found = []
        check_charts.check_yolab_off(
            "vaultwarden", docs, lambda app, msg: found.append(msg)
        )
        self.assertEqual(found, [])
        spec = self.deployments(docs)["gateway"]
        self.assertEqual([c["name"] for c in spec["initContainers"]][:1], ["yolab-env"])

    def test_switching_off_removes_the_old_tunnel_and_keeps_scripts_working(self):
        spec = self.deployments(self.docs(YOLAB_OFF))["gateway"]
        init = next(c for c in spec["initContainers"] if c["name"] == "yolab-env")
        script = init["command"][2]
        self.assertIn("-X DELETE", script)
        self.assertIn("rm -f /state/wg-state.json", script)
        self.assertIn("> /yolab/env", script)

    def test_a_tunnel_left_running_while_off_is_reported(self):
        docs = self.docs(YOLAB_OFF)
        spec = self.deployments(docs)["gateway"]
        spec["containers"].append({"name": "wireguard", "image": "x"})
        found = []
        check_charts.check_yolab_off(
            "vaultwarden", docs, lambda app, msg: found.append(msg)
        )
        self.assertEqual(len(found), 1)


EXPLORER_ENABLED = check_charts.EXPLORER_ENABLED
EXPLORER_WAYS_ON = check_charts.EXPLORER_WAYS_ON


class FileExplorerWaysRendered(RenderedChart):
    CHART = "vaultwarden"

    def gateway(self, docs):
        return self.deployments(docs)["gateway"]

    def names(self, spec, key="containers"):
        return {c["name"] for c in spec.get(key) or []}

    def ways_failures(self, docs):
        found = []
        check_charts.check_file_explorer_ways(
            "vaultwarden", docs, lambda app, msg: found.append(msg)
        )
        return found

    def test_by_default_the_explorer_is_off(self):
        docs = self.docs()
        self.assertFalse(
            {"file-explorer", "file-explorer-tor", "file-explorer-tailscale"}
            & self.names(self.gateway(docs))
        )
        self.assertNotIn(
            "{$FILE_EXPLORER_FQDN}",
            check_charts.configmap_data(docs, "-caddy", "Caddyfile"),
        )

    def test_switched_on_the_explorer_has_only_its_yolab_address(self):
        docs = self.docs(EXPLORER_ENABLED)
        containers = self.names(self.gateway(docs))
        self.assertIn("file-explorer", containers)
        self.assertFalse({"file-explorer-tor", "file-explorer-tailscale"} & containers)
        caddyfile = check_charts.configmap_data(docs, "-caddy", "Caddyfile")
        self.assertIn("{$FILE_EXPLORER_FQDN}", caddyfile)
        self.assertNotIn("18793", caddyfile)
        self.assertNotIn("18794", caddyfile)

    def test_switched_on_the_explorer_s_tor_and_tailscale_pass_the_checks(self):
        docs = self.docs(EXPLORER_WAYS_ON)
        self.assertTrue(
            {"file-explorer", "file-explorer-tor", "file-explorer-tailscale"}
            <= self.names(self.gateway(docs))
        )
        self.assertIn(
            "file-explorer-tor-state",
            self.names(self.gateway(docs), "initContainers"),
        )
        self.assertEqual(self.ways_failures(docs), [])

    def test_an_explorer_tailscale_identity_nobody_claims_is_reported(self):
        docs = self.docs(EXPLORER_WAYS_ON)
        sidecar = next(
            c
            for c in self.gateway(docs)["containers"]
            if c["name"] == "file-explorer-tailscale"
        )
        sidecar["command"][-1] = sidecar["command"][-1].replace(
            "claim_state /var/lib/tailscale\n", ""
        )
        found = self.ways_failures(docs)
        self.assertEqual(len(found), 1, found)
        self.assertIn("file-explorer-tailscale", found[0])
        self.assertIn("a copy would run with the original's identity", found[0])

    def test_the_explorer_s_tailscale_cannot_be_switched_on_without_its_own_key(self):
        text, err = check_charts.render(
            self.chart,
            self.library,
            self.tmp.name,
            {**EXPLORER_ENABLED, "config.file_explorer_tailscale_enabled": "true"},
        )
        self.assertIsNone(text)
        self.assertIn("file_explorer_tailscale_auth_key", err)

    def test_the_explorer_keeps_identities_apart_from_the_app_s(self):
        spec = self.gateway(self.docs({**PRIVATE_ON, **EXPLORER_WAYS_ON}))
        mounts = {
            c["name"]: {m.get("subPath") for m in c.get("volumeMounts") or []}
            for c in spec["containers"]
        }
        self.assertIn("release/tor", mounts["tor"])
        self.assertIn("release/file-explorer-tor", mounts["file-explorer-tor"])
        self.assertIn("release/tailscale", mounts["tailscale"])
        self.assertIn(
            "release/file-explorer-tailscale", mounts["file-explorer-tailscale"]
        )
        self.assertNotEqual(
            self.env(spec, "tailscale")["TS_HOSTNAME"],
            self.env(spec, "file-explorer-tailscale")["TS_HOSTNAME"],
        )

    def test_the_explorer_cannot_change_its_own_onion_key_or_tailscale_identity(self):
        spec = self.gateway(self.docs(EXPLORER_WAYS_ON))
        explorer = next(c for c in spec["containers"] if c["name"] == "file-explorer")
        read_only = {
            m.get("subPath")
            for m in explorer["volumeMounts"]
            if m.get("readOnly") is True
        }
        self.assertIn("release/file-explorer-tor", read_only)
        self.assertIn("release/file-explorer-tailscale", read_only)

    def test_two_tailscale_devices_in_one_pod_do_not_fight_over_a_port(self):
        spec = self.gateway(self.docs({**PRIVATE_ON, **EXPLORER_WAYS_ON}))
        self.assertEqual(
            self.env(spec, "file-explorer-tailscale")["TS_TAILSCALED_EXTRA_ARGS"],
            "--port=0",
        )

    def test_an_explorer_entry_point_without_the_login_is_reported(self):
        docs = self.docs(EXPLORER_WAYS_ON)
        for d in docs:
            if d.get("kind") == "ConfigMap" and "Caddyfile" in (d.get("data") or {}):
                d["data"]["Caddyfile"] = d["data"]["Caddyfile"].replace(
                    "http://:18794 {\n  bind 127.0.0.1\n  basic_auth",
                    "http://:18794 {\n  bind 127.0.0.1\n  log",
                )
        found = self.ways_failures(docs)
        self.assertEqual(len(found), 1)
        self.assertIn("18794", found[0])

    def test_app_address_off_and_explorer_s_on_registers_only_the_explorer(self):
        docs = self.docs(
            {
                **EXPLORER_ENABLED,
                "config.yolab_enabled": "false",
                "config.tor_enabled": "true",
                "config.file_explorer_yolab_enabled": "true",
            }
        )
        spec = self.gateway(docs)
        register = next(c for c in spec["initContainers"] if c["name"] == "wg-register")
        env = {e["name"]: e.get("value") for e in register["env"]}
        self.assertEqual(env["SERVICE_NAME"], "")
        self.assertTrue(env["ALIASES"].startswith("FILE_EXPLORER_FQDN="))
        self.assertIn("wireguard", self.names(spec))
        caddyfile = check_charts.configmap_data(docs, "-caddy", "Caddyfile")
        self.assertIn("{$FILE_EXPLORER_FQDN}", caddyfile)
        self.assertNotIn("YOLAB_FQDN", caddyfile)

    def test_an_explorer_reached_only_over_tor_needs_no_tunnel(self):
        docs = self.docs(
            {
                **EXPLORER_ENABLED,
                "config.yolab_enabled": "false",
                "config.tor_enabled": "true",
                "config.file_explorer_yolab_enabled": "false",
                "config.file_explorer_tor_enabled": "true",
            }
        )
        spec = self.gateway(docs)
        inits = self.names(spec, "initContainers")
        self.assertNotIn("wg-register", inits)
        self.assertIn("yolab-env", inits)
        self.assertFalse({"wireguard"} & self.names(spec))
        self.assertIn("file-explorer-tor", self.names(spec))
        explorer_init = next(
            c for c in spec["initContainers"] if c["name"] == "file-explorer-init"
        )
        self.assertNotIn("FILE_EXPLORER_FQDN", explorer_init["command"][2])
        caddyfile = check_charts.configmap_data(docs, "-caddy", "Caddyfile")
        self.assertNotIn("FILE_EXPLORER_FQDN", caddyfile)

    def test_from_before_the_switches_the_explorer_follows_the_app_s_address(self):
        spec = self.gateway(self.docs({**YOLAB_OFF, **EXPLORER_ENABLED}))
        self.assertNotIn("file-explorer", self.names(spec))
        self.assertNotIn("wg-register", self.names(spec, "initContainers"))


class ImportedEnvironment(unittest.TestCase):
    def env(self, svc, env_file_vars=None):
        import import_umbrel

        c = import_umbrel.build_container(
            "app", svc, "tdex", set(), [], env_file_vars=env_file_vars
        )
        return {e["name"]: e.get("value") for e in c.get("env") or []}

    def test_a_compose_mapping_keeps_each_name_and_value(self):
        env = self.env(
            {
                "image": "x",
                "environment": {"OCEAN_LOG_LEVEL": 5, "OCEAN_NO_TLS": "true"},
            }
        )
        self.assertEqual(env, {"OCEAN_LOG_LEVEL": "5", "OCEAN_NO_TLS": "true"})

    def test_a_compose_list_keeps_each_name_and_value(self):
        env = self.env({"image": "x", "environment": ["A=1", "B=two=2"]})
        self.assertEqual(env, {"A": "1", "B": "two=2"})

    def test_env_file_pairs_come_first_so_compose_overrides_them(self):
        import import_umbrel

        c = import_umbrel.build_container(
            "app",
            {"image": "x", "environment": {"A": "compose"}},
            "tdex",
            set(),
            [],
            env_file_vars=[("A", "file"), ("B", "file")],
        )
        self.assertEqual(
            [(e["name"], e["value"]) for e in c["env"]],
            [("A", "file"), ("B", "file"), ("A", "compose")],
        )

    def test_a_compose_hostname_in_a_command_becomes_the_service_name(self):
        import import_umbrel

        c = import_umbrel.build_container(
            "init",
            {
                "image": "x",
                "command": 'sh -c "mc alias set s http://lobe-chat_rustfs_1:9000"',
            },
            "lobe-chat",
            set(),
            [],
        )
        self.assertIn("http://rustfs:9000", " ".join(c["args"]))
        self.assertNotIn("_1:", " ".join(c["args"]))

    def test_a_cross_service_hostname_becomes_the_service_name(self):
        env = self.env({"image": "x", "environment": {"WALLET": "tdex_oceand_1:18000"}})
        self.assertEqual(env, {"WALLET": "oceand:18000"})


class Regenerated(unittest.TestCase):
    def test_a_regenerated_chart_keeps_its_curated_annotations_and_moves_up(self):
        import import_umbrel

        chart = import_umbrel.carry_over(
            {
                "version": "0.1.9",
                "annotations": {
                    "yolab.io/tagline": "Curated",
                    "yolab.io/github": "a/b",
                },
            },
            {
                "version": "0.1.0",
                "annotations": {"yolab.io/tagline": "Generated", "yolab.io/icon": "x"},
            },
        )
        self.assertEqual(chart["version"], "0.1.10")
        self.assertEqual(
            chart["annotations"],
            {
                "yolab.io/tagline": "Curated",
                "yolab.io/icon": "x",
                "yolab.io/github": "a/b",
            },
        )

    def test_tdex_is_never_imported(self):
        import import_umbrel

        self.assertIn("Liquid", import_umbrel.skip_reason("tdex", {}, set()))


class Containers(unittest.TestCase):
    def failures(self, containers, init=None):
        docs = [
            {
                "kind": "Deployment",
                "metadata": {"name": "gateway"},
                "spec": {
                    "template": {
                        "spec": {"containers": containers, "initContainers": init or []}
                    }
                },
            }
        ]
        found = []
        check_charts.check_containers("app", docs, lambda app, msg: found.append(msg))
        return found

    def test_a_python_pair_rendered_as_an_env_name_is_reported(self):
        found = self.failures(
            [{"name": "app", "env": [{"name": "('PORT', 3000)", "value": ""}]}]
        )
        self.assertEqual(len(found), 1, found)
        self.assertIn("not a variable name", found[0])

    def test_ordinary_env_names_pass(self):
        self.assertEqual(
            self.failures(
                [
                    {
                        "name": "app",
                        "env": [{"name": "APP_SEED"}, {"name": "spring.port"}],
                    }
                ]
            ),
            [],
        )

    def test_two_containers_with_one_name_in_a_pod_are_reported(self):
        found = self.failures([{"name": "caddy"}, {"name": "caddy"}])
        self.assertEqual(found, ["pod gateway has two containers named caddy"])

    def test_an_init_container_may_not_reuse_a_container_s_name(self):
        found = self.failures([{"name": "seed"}], init=[{"name": "seed"}])
        self.assertEqual(len(found), 1, found)

    def test_a_port_name_longer_than_fifteen_characters_is_reported(self):
        found = self.failures(
            [
                {
                    "name": "steam",
                    "ports": [{"name": "sunshine-control", "containerPort": 1}],
                }
            ]
        )
        self.assertEqual(len(found), 1, found)
        self.assertIn("sunshine-control", found[0])

    def test_port_names_the_api_server_refuses_are_reported(self):
        for bad in ["Web", "web_ui", "-web", "web-", "we--b", "8080"]:
            found = self.failures([{"name": "app", "ports": [{"name": bad}]}])
            self.assertEqual(len(found), 1, bad)

    def test_ordinary_port_names_and_unnamed_ports_pass(self):
        ports = [{"name": "sunshine-ctrl"}, {"name": "http"}, {"name": "h2c"}, {}]
        self.assertEqual(self.failures([{"name": "app", "ports": ports}]), [])


class Disabled(unittest.TestCase):
    def failures(self, annotations):
        found = []
        check_charts.check_store_annotations(
            "app",
            {"yolab.io/tagline": "A line", **annotations},
            lambda app, msg: found.append(msg),
        )
        return found

    def test_a_disabled_app_says_why(self):
        self.assertEqual(self.failures({"yolab.io/disabled": "upstream archived"}), [])

    def test_a_disabled_app_without_a_reason_is_reported(self):
        found = self.failures({"yolab.io/disabled": ""})
        self.assertEqual(len(found), 1, found)
        self.assertIn("does not say why", found[0])


class OpenWebUiOllama(RenderedChart):
    CHART = "open-webui"

    def gateway(self, docs):
        return self.deployments(docs)["gateway"]

    def test_by_default_it_runs_its_own_ollama(self):
        spec = self.gateway(self.docs())
        self.assertIn("ollama", {c["name"] for c in spec["containers"]})

    def test_an_old_amd_machine_gets_the_vulkan_engine_through_dri(self):
        docs = self.docs(
            {"machines[0].name": "old-radeon", "machines[0].accelerator": "vulkan"}
        )
        engines = {
            d["metadata"]["name"]: d["spec"]["template"]["spec"]
            for d in docs
            if d.get("kind") == "DaemonSet"
        }
        self.assertIn("ollama-vulkan", engines)
        spec = engines["ollama-vulkan"]
        self.assertEqual(spec["nodeSelector"]["yolab.io/accelerator"], "vulkan")
        limits = spec["containers"][0]["resources"]["limits"]
        self.assertEqual(limits, {"yolab.io/dri": "1"})

    def test_it_listens_on_ipv6_where_the_readiness_probe_knocks(self):
        spec = self.gateway(self.docs())
        webui = next(c for c in spec["containers"] if c["name"] == "open-webui")
        env = {e["name"]: e.get("value") for e in webui["env"]}
        self.assertEqual(env["HOST"], "::")

    def test_a_linked_ollama_replaces_its_own_everywhere(self):
        url = "http://ollama.yolab-ai.svc.cluster.local:11434"
        docs = self.docs(
            {
                "config.ollama.from": "yolab-ai",
                "config.ollama.api": url,
                "machines[0].name": "gpu-box",
                "machines[0].accelerator": "nvidia",
            }
        )
        spec = self.gateway(docs)
        self.assertNotIn("ollama", {c["name"] for c in spec["containers"]})
        self.assertFalse([d for d in docs if d.get("kind") == "DaemonSet"])
        self.assertFalse(
            [
                d
                for d in docs
                if d.get("kind") == "Service" and d["metadata"]["name"] == "ollama"
            ]
        )
        webui = next(c for c in spec["containers"] if c["name"] == "open-webui")
        env = {e["name"]: e.get("value") for e in webui["env"]}
        self.assertEqual(env["OLLAMA_BASE_URL"], url)

    def test_an_install_from_before_connections_keeps_its_ollama(self):
        url = "http://ollama.yolab-ai.svc.cluster.local:11434"
        spec = self.gateway(self.docs({"config.ollama_url": url}))
        webui = next(c for c in spec["containers"] if c["name"] == "open-webui")
        env = {e["name"]: e.get("value") for e in webui["env"]}
        self.assertEqual(env["OLLAMA_BASE_URL"], url)


class OllamaEngine(RenderedChart):
    CHART = "ollama"

    def engine(self, extra=None):
        return self.deployments(self.docs(extra))["ollama"]

    def test_without_a_gpu_anywhere_it_runs_on_the_cpu_wherever_it_fits(self):
        spec = self.engine()
        self.assertNotIn("nodeSelector", spec)
        self.assertNotIn("resources", spec["containers"][0])
        self.assertIn("ollama/ollama:0.35.1@", spec["containers"][0]["image"])

    def test_it_runs_on_the_best_gpu_machine_with_that_cards_engine(self):
        cases = {
            "nvidia": ("ollama/ollama:0.35.1@", "nvidia.com/gpu-all"),
            "amd": ("-rocm@", "yolab.io/kfd"),
            "vulkan": ("ghcr.io/demycode/ollama-vulkan", "yolab.io/dri"),
            "intel": ("ghcr.io/demycode/ollama-vulkan", "yolab.io/dri"),
        }
        for accelerator, (image, device) in cases.items():
            spec = self.engine(
                {"machine.name": "box", "machine.accelerator": accelerator}
            )
            self.assertEqual(
                spec["nodeSelector"]["kubernetes.io/hostname"], "box", accelerator
            )
            container = spec["containers"][0]
            self.assertIn(image, container["image"], accelerator)
            self.assertEqual(container["resources"]["limits"], {device: "1"})

    def test_the_gateway_reaches_ollama_through_its_service(self):
        docs = self.docs()
        gateway = self.deployments(docs)["gateway"]
        self.assertNotIn("ollama", [c["name"] for c in gateway["containers"]])
        services = [d for d in docs if d.get("kind") == "Service"]
        ollama = next(s for s in services if s["metadata"]["name"] == "ollama")
        self.assertEqual(ollama["spec"]["selector"], {"app": "ollama"})


class ServiceLinks(unittest.TestCase):
    def collect(self, fn, *args):
        found = []
        fn(*args, lambda app, msg: found.append(msg))
        return found

    def test_a_provided_service_must_be_rendered_on_its_port(self):
        schema = {"x-yolab-provides": {"ollama": {"service": "ollama", "port": 11434}}}
        service = {
            "kind": "Service",
            "metadata": {"name": "ollama"},
            "spec": {"ports": [{"port": 11434}]},
        }
        self.assertEqual(
            self.collect(check_charts.check_provides, "ollama", schema, [service]), []
        )
        found = self.collect(check_charts.check_provides, "ollama", schema, [])
        self.assertEqual(len(found), 1, found)
        self.assertIn("does not render", found[0])

    def test_a_provided_service_needs_a_name_and_a_port(self):
        found = self.collect(
            check_charts.check_provides,
            "x",
            {"x-yolab-provides": {"api": {"service": "x"}}},
            [],
        )
        self.assertIn("needs a service name and a numeric port", found[0])

    def test_link_fields_are_found_behind_switches(self):
        schema = {
            "properties": {
                "config": {
                    "dependencies": {
                        "ai": {
                            "oneOf": [
                                {
                                    "properties": {
                                        "ollama_url": {
                                            "type": "string",
                                            "format": "service-url",
                                            "x-yolab-service": "ollama",
                                        }
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }
        self.assertEqual(check_charts.wanted_kinds(schema), {"ollama"})

    def torrent_client(self, provides):
        return {
            "properties": {
                "outputs": {
                    "properties": {
                        "api": {
                            "title": "Web UI",
                            "source": {"service": {"name": "qbit", "port": 8080}},
                        },
                        "password": {
                            "title": "Password",
                            "source": {"config": "password"},
                        },
                        "onion": {"title": "Onion", "source": {"logs": "ONION (\\S+)"}},
                    }
                }
            },
            "x-yolab-provides": provides,
        }

    def service(self, name, port):
        return {
            "kind": "Service",
            "metadata": {"name": name},
            "spec": {"ports": [{"port": port}]},
        }

    def test_a_provider_hands_over_outputs_it_declares(self):
        schema = self.torrent_client({"torrent-client": ["api", "password"]})
        self.assertEqual(
            self.collect(
                check_charts.check_provides, "q", schema, [self.service("qbit", 8080)]
            ),
            [],
        )

    def test_an_address_output_must_be_rendered(self):
        schema = self.torrent_client({"torrent-client": ["api"]})
        found = self.collect(check_charts.check_provides, "q", schema, [])
        self.assertTrue(any("does not render" in f for f in found), found)

    def test_a_value_only_known_once_the_app_runs_cannot_be_handed_over(self):
        schema = self.torrent_client({"torrent-client": ["onion"]})
        found = self.collect(
            check_charts.check_provides, "q", schema, [self.service("qbit", 8080)]
        )
        self.assertTrue(any("only known once" in f for f in found), found)

    def test_an_unknown_output_cannot_be_handed_over(self):
        schema = self.torrent_client({"torrent-client": ["nope"]})
        found = self.collect(
            check_charts.check_provides, "q", schema, [self.service("qbit", 8080)]
        )
        self.assertTrue(any("which is no output" in f for f in found), found)

    def test_a_connection_field_wants_its_keys_from_its_interface(self):
        schema = {
            "properties": {
                "config": {
                    "properties": {
                        "download_client": {
                            "type": "object",
                            "format": "connection",
                            "x-yolab-requires": "torrent-client",
                            "properties": {
                                "from": {"type": "string"},
                                "api": {"type": "string"},
                                "password": {"type": "string"},
                            },
                        }
                    }
                }
            }
        }
        self.assertEqual(check_charts.wanted_kinds(schema), {"torrent-client"})
        self.assertEqual(
            check_charts.wanted_keys(schema), {"torrent-client": {"api", "password"}}
        )

    def test_a_consumer_wanting_a_key_a_provider_never_hands_over_is_reported(self):
        found = self.collect(
            check_charts.check_link_keys,
            {"sonarr": {"torrent-client": {"api", "password"}}},
            {
                "qbittorrent": {"torrent-client": {"api", "password"}},
                "transmission": {"torrent-client": {"api"}},
            },
        )
        self.assertEqual(len(found), 1, found)
        self.assertIn("transmission", found[0])
        self.assertIn("password", found[0])

    def test_the_old_address_form_hands_over_one_url(self):
        schema = {"x-yolab-provides": {"ollama": {"service": "ollama", "port": 1}}}
        self.assertEqual(check_charts.provided_keys(schema), {"ollama": {"url"}})

    def test_a_link_to_a_service_nobody_provides_is_reported(self):
        found = self.collect(
            check_charts.check_links,
            {"open-webui": {"ollama"}, "lnd-ui": {"lnd-grpc"}},
            {"ollama": {"ollama"}},
        )
        self.assertEqual(
            found, ["a link field wants lnd-grpc, which no chart provides"]
        )


class Folders(unittest.TestCase):
    def collect(self, fn, *args):
        found = []
        fn(*args, lambda app, msg: found.append(msg))
        return found

    def field(self, **overrides):
        prop = {
            "type": "string",
            "format": "folder",
            "title": "Media folder",
            "default": "",
            "pattern": check_charts.FOLDER_PATTERN,
        }
        prop.update(overrides)
        return {"properties": {"config": {"properties": {"media_folder": prop}}}}

    def claim(self, **spec_overrides):
        spec = {
            "accessModes": ["ReadWriteMany"],
            "storageClassName": "",
            "volumeName": f"default.folder-{check_charts.FOLDER_PROBE}",
        }
        spec.update(spec_overrides)
        return {
            "kind": "PersistentVolumeClaim",
            "metadata": {
                "name": f"folder-{check_charts.FOLDER_PROBE}",
                "namespace": "default",
                "labels": {check_charts.FOLDER_LABEL: check_charts.FOLDER_PROBE},
            },
            "spec": spec,
        }

    def deployment(self, **mount_overrides):
        mount = {"name": "media", "mountPath": f"/data/{check_charts.FOLDER_PROBE}"}
        mount.update(mount_overrides)
        return {
            "kind": "Deployment",
            "metadata": {"name": "gateway"},
            "spec": {
                "template": {
                    "spec": {
                        "containers": [
                            {"name": "app", "image": "x", "volumeMounts": [mount]}
                        ],
                        "volumes": [
                            {
                                "name": "media",
                                "persistentVolumeClaim": {
                                    "claimName": f"folder-{check_charts.FOLDER_PROBE}"
                                },
                            }
                        ],
                    }
                }
            },
        }

    def test_a_well_formed_folder_field_passes(self):
        self.assertEqual(
            self.collect(check_charts.check_folder_fields, "x", self.field()), []
        )

    def test_a_folder_chosen_by_default_is_refused(self):
        found = self.collect(
            check_charts.check_folder_fields, "x", self.field(default="movies")
        )
        self.assertIn("must default to empty", found[0])

    def test_a_folder_field_without_the_name_pattern_is_refused(self):
        found = self.collect(
            check_charts.check_folder_fields, "x", self.field(pattern=".*")
        )
        self.assertIn("pattern", found[0])

    def test_folder_fields_are_found_behind_switches(self):
        schema = {
            "properties": {
                "config": {
                    "dependencies": {
                        "on": {
                            "oneOf": [
                                {
                                    "properties": {
                                        "downloads": {
                                            "type": "string",
                                            "format": "folder",
                                        }
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }
        self.assertEqual(set(check_charts.folder_fields(schema)), {"downloads"})

    def test_a_chart_that_mounts_the_whole_folder_at_data_passes(self):
        docs = [self.claim(), self.deployment()]
        self.assertEqual(
            self.collect(check_charts.check_folders, "x", "media_folder", docs), []
        )

    def test_a_folder_claim_that_would_get_a_new_empty_volume_is_refused(self):
        docs = [self.claim(storageClassName="yolab-cephfs"), self.deployment()]
        found = self.collect(check_charts.check_folders, "x", "media_folder", docs)
        self.assertTrue(any("storageClassName" in f for f in found), found)

    def test_a_folder_claim_bound_to_another_apps_mount_is_refused(self):
        docs = [self.claim(volumeName="yolab-other.folder-x"), self.deployment()]
        found = self.collect(check_charts.check_folders, "x", "media_folder", docs)
        self.assertTrue(any("volumeName" in f for f in found), found)

    def test_a_folder_mounted_somewhere_else_than_data_is_refused(self):
        docs = [self.claim(), self.deployment(mountPath="/media")]
        found = self.collect(check_charts.check_folders, "x", "media_folder", docs)
        self.assertTrue(any("/data/<folder>" in f for f in found), found)

    def test_a_folder_mounted_in_part_is_refused(self):
        docs = [self.claim(), self.deployment(subPath="movies")]
        found = self.collect(check_charts.check_folders, "x", "media_folder", docs)
        self.assertTrue(any("hardlinks" in f for f in found), found)

    def test_a_chosen_folder_nobody_mounts_is_refused(self):
        found = self.collect(
            check_charts.check_folders, "x", "media_folder", [self.claim()]
        )
        self.assertTrue(any("no container mounts" in f for f in found), found)

    def test_a_folder_claim_does_not_count_as_the_apps_own_volume(self):
        own = {
            "kind": "PersistentVolumeClaim",
            "metadata": {"name": "release-data"},
            "spec": {},
        }
        self.assertTrue(check_charts.is_folder_claim(self.claim()))
        self.assertFalse(check_charts.is_folder_claim(own))


class Groups(unittest.TestCase):
    CHARTS: ClassVar[dict] = {
        "jellyfin": (
            "0.2.12",
            {
                "properties": {
                    "config": {
                        "properties": {
                            "media_folder": {"type": "string", "format": "folder"},
                        },
                        "dependencies": {
                            "gpu": {
                                "oneOf": [{"properties": {"hardware_transcoding": {}}}]
                            }
                        },
                    }
                }
            },
        ),
        "prowlarr": ("0.1.4", {}),
    }

    def collect(self, fn, *args):
        found = []
        fn(*args, lambda where, msg: found.append(msg))
        return found

    def resolve(self, schema):
        found = []
        resolved = check_charts.resolve_refs(
            schema, self.CHARTS, lambda w, m: found.append(m), "g"
        )
        return resolved, found

    def test_an_imported_field_arrives_whole_with_the_groups_own_words(self):
        resolved, found = self.resolve(
            {
                "$ref": "chart:jellyfin#/properties/config/properties/media_folder",
                "title": "Films",
            }
        )
        self.assertEqual(found, [])
        self.assertEqual(
            resolved, {"type": "string", "format": "folder", "title": "Films"}
        )

    def test_a_whole_app_form_comes_in_without_what_the_group_decides(self):
        resolved, found = self.resolve(
            {
                "$ref": "chart:jellyfin#/properties/config",
                "title": "Jellyfin",
                "x-yolab-omit": ["media_folder"],
            }
        )
        self.assertEqual(found, [])
        self.assertNotIn("media_folder", resolved["properties"])
        self.assertNotIn("x-yolab-omit", resolved)
        self.assertEqual(resolved["title"], "Jellyfin")

    def test_a_reference_the_catalog_cannot_follow_is_reported(self):
        for ref, words in [
            ("chart:plex#/x", "no chart named plex"),
            ("chart:jellyfin@0.1.0#/x", "not 0.1.0"),
            ("chart:jellyfin#/nope", "nothing at /nope"),
            ("chart:Jelly#/x", "is not chart:"),
        ]:
            _, found = self.resolve({"$ref": ref})
            self.assertTrue(any(words in f for f in found), (ref, found))

    def test_a_pinned_version_the_catalog_has_resolves(self):
        _, found = self.resolve(
            {"$ref": "chart:jellyfin@0.2.12#/properties/config/properties/media_folder"}
        )
        self.assertEqual(found, [])

    def app(self, **fields):
        return {"apiVersion": "yolab.io/v1", "kind": "App", **fields}

    def test_apps_that_install_or_reuse_pass(self):
        docs = [
            self.app(name="jellyfin", chart="jellyfin", values={"media_folder": "m"}),
            self.app(name="client", use="yolab-qbittorrent-ab12"),
            self.app(name="prowlarr", chart="prowlarr@0.1.4"),
        ]
        found = self.collect(check_charts.check_group_apps, "g", docs, self.CHARTS)
        self.assertEqual(found, [])

    def test_a_setting_behind_a_switch_of_the_chart_counts_as_its_setting(self):
        docs = [
            self.app(
                name="jellyfin",
                chart="jellyfin",
                values={"hardware_transcoding": True},
            )
        ]
        found = self.collect(check_charts.check_group_apps, "g", docs, self.CHARTS)
        self.assertEqual(found, [])

    def test_what_a_group_may_not_render_is_reported(self):
        cases = [
            ({"apiVersion": "v1", "kind": "ConfigMap"}, "may only render"),
            (self.app(name="Bad Name", chart="jellyfin"), "not a plain name"),
            (self.app(name="a", chart="plex"), "not a chart in this catalog"),
            (self.app(name="a", chart="jellyfin@0.1.0"), "pins jellyfin 0.1.0"),
            (self.app(name="a", chart="jellyfin", use="yolab-x"), "exactly one"),
            (self.app(name="a", use="kube-system"), "not an app"),
            (
                self.app(name="a", chart="jellyfin", values={"turbo": True}),
                "has no setting turbo",
            ),
        ]
        for doc, words in cases:
            found = self.collect(check_charts.check_group_apps, "g", [doc], self.CHARTS)
            self.assertTrue(any(words in f for f in found), (doc, found))

    def test_two_apps_with_one_name_are_reported(self):
        docs = [
            self.app(name="a", chart="jellyfin"),
            self.app(name="a", chart="prowlarr"),
        ]
        found = self.collect(check_charts.check_group_apps, "g", docs, self.CHARTS)
        self.assertTrue(any("two Apps" in f for f in found), found)

    def test_a_group_is_told_apart_from_an_app_by_its_annotation(self):
        self.assertTrue(
            check_charts.is_group_chart(
                "name: x\nannotations:\n  yolab.io/kind: group\n"
            )
        )
        self.assertFalse(check_charts.is_group_chart("name: x\n"))

    def test_the_catalog_groups_resolve_and_render_every_choice(self):
        charts = check_charts.catalog_charts()
        groups = [
            str(p.parent)
            for p in Path(check_charts.HERE).glob("*/Chart.yaml")
            if check_charts.is_group_chart(p.read_text())
        ]
        self.assertTrue(groups)
        found = []
        with tempfile.TemporaryDirectory() as tmp:
            for group in groups:
                check_charts.check_group(
                    group, charts, tmp, lambda w, m: found.append(f"{w}: {m}")
                )
        self.assertEqual(found, [])


class ImageArches(unittest.TestCase):
    def test_a_known_digest_is_never_asked_again(self):
        import image_arches

        asked = []

        def lookup(image):
            asked.append(image)
            return ["amd64"]

        result, unreadable = image_arches.merged(
            ["a@sha256:1", "b@sha256:2"], {"a@sha256:1": ["amd64", "arm64"]}, lookup
        )
        self.assertEqual(asked, ["b@sha256:2"])
        self.assertEqual(result["a@sha256:1"], ["amd64", "arm64"])
        self.assertEqual(unreadable, [])

    def test_a_failed_lookup_is_left_out_not_recorded_as_running_nowhere(self):
        import image_arches

        result, unreadable = image_arches.merged(["a@sha256:1"], {}, lambda image: None)
        self.assertEqual(result, {})
        self.assertEqual(unreadable, ["a@sha256:1"])

    def test_an_empty_entry_from_an_old_failure_is_asked_again(self):
        import image_arches

        result, _ = image_arches.merged(
            ["a@sha256:1"], {"a@sha256:1": []}, lambda image: ["arm64"]
        )
        self.assertEqual(result, {"a@sha256:1": ["arm64"]})

    def test_docker_hub_images_are_read_from_googles_mirror_first(self):
        import image_arches

        digest = "sha256:" + "a" * 64
        self.assertEqual(
            image_arches.sources(f"postgres:17@{digest}"),
            [f"mirror.gcr.io/library/postgres@{digest}", f"postgres@{digest}"],
        )
        self.assertEqual(
            image_arches.sources(f"docker.io/akaunting/akaunting:3@{digest}"),
            [
                f"mirror.gcr.io/akaunting/akaunting@{digest}",
                f"docker.io/akaunting/akaunting@{digest}",
            ],
        )

    def test_other_registries_are_read_directly(self):
        import image_arches

        digest = "sha256:" + "a" * 64
        for image in (
            f"ghcr.io/immich-app/server:v2@{digest}",
            f"lscr.io/linuxserver/jellyfin:latest@{digest}",
            f"reg:5000/team/app:1@{digest}",
        ):
            self.assertEqual(len(image_arches.sources(image)), 1, image)

    def test_a_disabled_chart_needs_no_platform_record(self):
        import image_arches

        self.assertTrue(
            image_arches.is_disabled(
                'annotations:\n  yolab.io/disabled: "image deleted upstream"\n'
            )
        )
        self.assertFalse(
            image_arches.is_disabled("annotations:\n  yolab.io/tagline: x\n")
        )

    def test_an_image_no_longer_pinned_is_dropped(self):
        import image_arches

        result, _ = image_arches.merged(
            [], {"gone@sha256:1": ["amd64"]}, lambda image: ["amd64"]
        )
        self.assertEqual(result, {})


class OffersFileExplorer(unittest.TestCase):
    def test_a_chart_with_the_explorer_switch_offers_it_even_when_off_by_default(self):
        schema = {
            "properties": {
                "config": {
                    "properties": {
                        "file_explorer_enabled": {"type": "boolean", "default": False}
                    }
                }
            }
        }
        self.assertTrue(check_charts.offers_file_explorer(schema))

    def test_a_chart_without_the_switch_does_not_offer_it(self):
        self.assertFalse(
            check_charts.offers_file_explorer({"properties": {"config": {}}})
        )
        self.assertFalse(check_charts.offers_file_explorer({}))


if __name__ == "__main__":
    unittest.main()


def api_key_failures(s):
    found = []
    check_charts.check_api_key_offer("demo", s, lambda app, msg: found.append(msg))
    return found


API_KEY_OUTPUT = logs(
    r"YOLAB_OUTPUT api_key (\S+)",
    format="secret",
    when={"properties": {"api_key_enabled": {"const": True}}},
)


class ApiKeyOffer(unittest.TestCase):
    def test_an_offered_key_must_be_readable_as_a_secret_only_while_on(self):
        on = {"api_key_enabled": {"type": "boolean", "default": True}}
        self.assertEqual(
            api_key_failures(schema(config=on, outputs={"api_key": API_KEY_OUTPUT})), []
        )
        self.assertTrue(api_key_failures(schema(config=on, outputs={})))
        shown = dict(API_KEY_OUTPUT, format="text")
        self.assertTrue(api_key_failures(schema(config=on, outputs={"api_key": shown})))

    def test_a_key_next_to_an_authelia_login_is_refused(self):
        config = {
            "api_key_enabled": {"type": "boolean", "default": True},
            "auth_enabled": {"type": "boolean", "default": False},
        }
        found = api_key_failures(
            schema(config=config, outputs={"api_key": API_KEY_OUTPUT})
        )
        self.assertTrue(any("Authelia" in f for f in found))


class OllamaApiKey(RenderedChart):
    CHART = "ollama"
    UPSTREAM = "ollama:11434"

    def caddyfile(self, docs):
        return check_charts.configmap_data(docs, "-caddy", "Caddyfile") or ""

    def failures(self, docs):
        found = []
        check_charts.check_api_key(
            "ollama", docs, self.UPSTREAM, lambda a, m: found.append(m)
        )
        return found

    def test_the_key_is_required_by_default_on_the_yolab_address(self):
        docs = self.docs()
        self.assertIn(check_charts.API_KEY_GUARD, self.caddyfile(docs))
        self.assertEqual(self.failures(docs), [])

    def test_every_way_in_requires_the_key(self):
        docs = self.docs(check_charts.PRIVATE_ACCESS_ON["tailscale_enabled"])
        caddy = self.caddyfile(docs)
        self.assertEqual(caddy.count(check_charts.API_KEY_GUARD), 2)
        self.assertEqual(self.failures(docs), [])

    def test_switching_the_key_off_removes_its_guard_and_its_init(self):
        docs = self.docs({"config.api_key_enabled": "false"})
        self.assertNotIn(check_charts.API_KEY_GUARD, self.caddyfile(docs))
        inits = [
            c["name"]
            for spec in self.deployments(docs).values()
            for c in spec.get("initContainers") or []
        ]
        self.assertNotIn("api-key-init", inits)

    def test_the_key_is_kept_on_the_apps_volume_so_restarts_keep_it(self):
        gateway = self.deployments(self.docs())["gateway"]
        init = next(c for c in gateway["initContainers"] if c["name"] == "api-key-init")
        mount = next(m for m in init["volumeMounts"] if m["mountPath"] == "/api-key")
        self.assertEqual(mount["name"], "data")
        self.assertTrue(mount["subPath"].endswith("/api-key"))
