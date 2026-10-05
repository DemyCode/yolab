import unittest
from pathlib import Path

import check_charts
import yaml

EXPLORER_ON = {"properties": {"file_explorer_enabled": {"const": True}}}


def schema(config=None, outputs=None, extra=None):
    props = {"config": {"type": "object", "properties": config or {}}}
    if outputs is not None:
        props["outputs"] = {"type": "object", "readOnly": True, "properties": outputs}
    props.update(extra or {})
    return {"type": "object", "properties": props}


def failures(s, chart_yaml="apiVersion: v2\nname: demo\n"):
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
        chart = "annotations:\n  yolab.io/outputs: |\n    []\n  yolab.io/uischema: |\n    {}\n"
        found = failures(GOOD, chart)
        self.assertEqual(len(found), 2)
        self.assertTrue(all("values.schema.json" in f for f in found))

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
    r"YOLAB_OUTPUT file_explorer_url (\S+)", format="uri", when=EXPLORER_ON
)
EXPLORER_PASSWORD = logs(
    r"YOLAB_OUTPUT file_explorer_password (\S+)", format="secret", when=EXPLORER_ON
)


class FileExplorerOutputs(unittest.TestCase):
    def explorer_failures(self, s):
        found = []
        check_charts.check(
            "demo", rendered_explorer(), lambda app, msg: found.append(msg), "", s
        )
        return [f for f in found if "file explorer" in f or "file_explorer" in f]

    def test_both_explorer_outputs_with_their_condition_pass(self):
        s = schema(
            outputs={
                "file_explorer_url": EXPLORER_URL,
                "file_explorer_password": EXPLORER_PASSWORD,
            }
        )
        self.assertEqual(self.explorer_failures(s), [])

    def test_a_missing_explorer_output_is_reported(self):
        s = schema(outputs={"file_explorer_password": EXPLORER_PASSWORD})
        found = self.explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("do not declare file_explorer_url", found[0])

    def test_an_explorer_output_without_its_condition_is_reported(self):
        unconditioned = logs(r"YOLAB_OUTPUT file_explorer_url (\S+)", format="uri")
        s = schema(
            outputs={
                "file_explorer_url": unconditioned,
                "file_explorer_password": EXPLORER_PASSWORD,
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
            sorted(engines), ["ollama-amd", "ollama-intel", "ollama-nvidia"]
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
            ["ollama-amd", "ollama-intel", "ollama-nvidia"],
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
        "jellyfin",
        "immich",
        "photoprism",
        "frigate",
    }
)


class PrivateAccessChoice(unittest.TestCase):
    def offers(self):
        import json

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

    def test_every_app_offering_one_way_in_offers_both_even_slow_video_over_tor(self):
        offers = self.offers()
        partial = {
            a for a, o in offers.items() if o != {"tor_enabled", "tailscale_enabled"}
        }
        self.assertEqual(partial, set())

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
        spec = self.deployments(self.docs(PRIVATE_ON))["gateway"]
        explorer = next(c for c in spec["containers"] if c["name"] == "file-explorer")
        read_only = {
            m.get("subPath")
            for m in explorer["volumeMounts"]
            if m.get("readOnly") is True
        }
        self.assertIn("release/tor", read_only)
        self.assertIn("release/tailscale", read_only)

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
        import json

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


if __name__ == "__main__":
    unittest.main()
