import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import ClassVar

import check_charts
import yaml


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


def output(**more):
    return {"type": "string", "title": "Shown", **more}


GOOD = schema(
    config={
        "subdomain": {"type": "string", "format": "tunnel"},
        "password": {"type": "string", "writeOnly": True, "generate": True},
        "pin": {"type": "string", "writeOnly": True},
    },
    outputs={
        "url": output(format="uri"),
        "password": output(title="Admin password", format="secret"),
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
        found = failures(schema(outputs={"x": output(title="  ")}))
        self.assertTrue(any("no title" in f for f in found))

    def test_an_unknown_format_is_refused(self):
        found = failures(schema(outputs={"x": output(format="hologram")}))
        self.assertTrue(any("hologram" in f for f in found))

    def test_an_output_is_only_a_type_title_and_format(self):
        for extra in (
            {"source": {"logs": r"YOLAB_OUTPUT x (\S+)"}},
            {"source": {"config": "password"}},
            {"when": {"properties": {"tor_enabled": {"const": True}}}},
        ):
            found = failures(schema(outputs={"x": output(**extra)}))
            self.assertEqual(len(found), 1, extra)
            self.assertIn("YOLAB_OUTPUT x <value>", found[0])

    def test_every_catalog_output_is_only_a_type_title_and_format(self):
        for path in sorted(Path(check_charts.HERE).glob("*/values.schema.json")):
            s = json.loads(path.read_text())
            outputs = (s.get("properties") or {}).get("outputs") or {}
            for key, out in (outputs.get("properties") or {}).items():
                self.assertLessEqual(
                    set(out), check_charts.OUTPUT_FIELDS, f"{path.parent.name} {key}"
                )


def rendered_gateway():
    return [
        {
            "kind": "Deployment",
            "metadata": {"name": "gateway"},
            "spec": {
                "template": {
                    "metadata": {"labels": {"app": "gateway"}},
                    "spec": {
                        "initContainers": [
                            {
                                "name": "wg-register",
                                "image": "example/wg-register@sha256:0",
                                "imagePullPolicy": "IfNotPresent",
                            }
                        ],
                        "containers": [],
                    },
                }
            },
        }
    ]


def pod(*containers):
    return {
        "kind": "Deployment",
        "metadata": {"name": "demo"},
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
            rendered_gateway() + [daemonset, service],
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
                [("/root/.ollama", "ollama")],
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
        self.assertNotIn(8483, published)
        self.assertEqual(published, {47984, 47989, 47990, 48010, 47998, 47999, 48000})
        self.assertIn("forward_auth", self.caddyfile())

    def caddyfile(self, extra=None):
        return next(
            d["data"]["Caddyfile"]
            for d in self.docs(extra)
            if d["kind"] == "ConfigMap" and "Caddyfile" in (d.get("data") or {})
        )

    def mounts(self):
        return {m["mountPath"]: m for m in self.game()["containers"][0]["volumeMounts"]}

    def volumes(self):
        return {v["name"]: v for v in self.game()["volumes"]}

    def test_its_page_is_reached_over_the_https_it_serves(self):
        caddy = self.caddyfile()
        self.assertIn("reverse_proxy steam-headless:8483 {", caddy)
        self.assertIn("tls_insecure_skip_verify", caddy)
        probe = self.game()["containers"][0]["readinessProbe"]["tcpSocket"]
        self.assertEqual(probe["port"], 8483)

    def test_systemd_keeps_what_the_image_prepares_in_run(self):
        self.assertEqual(self.mounts()["/run"]["name"], "run")
        self.assertEqual(self.volumes()["run"]["emptyDir"]["medium"], "Memory")

    def test_its_home_and_state_live_on_the_apps_volume(self):
        mounts = self.mounts()
        self.assertEqual(mounts["/home/steamheadless"]["subPath"], "home")
        self.assertEqual(mounts["/var/lib/steam-headless"]["subPath"], "state")
        self.assertEqual(mounts["/mnt/games"]["subPath"], "games")

    def test_the_desktop_password_is_a_root_only_file_the_image_reads(self):
        mount = self.mounts()["/steam-headless-provisioning-password"]
        self.assertEqual(mount["subPath"], "password")
        secret = self.volumes()[mount["name"]]["secret"]
        self.assertEqual(
            secret["items"], [{"key": "desktop", "path": "password", "mode": 0o400}]
        )


class GatewayUpstream(RenderedChart):
    CHART = "vaultwarden"

    def test_a_plain_http_app_is_proxied_without_tls(self):
        caddy = next(
            d["data"]["Caddyfile"]
            for d in self.docs()
            if d["kind"] == "ConfigMap" and "Caddyfile" in (d.get("data") or {})
        )
        self.assertNotIn("transport http", caddy)
        self.assertRegex(caddy, r"reverse_proxy \S+\n")


class VaultwardenAdmin(RenderedChart):
    CHART = "vaultwarden"

    def vaultwarden(self):
        return self.deployments(self.docs())["vaultwarden"]

    def admin_secret_ref(self, container):
        c = next(c for c in self.vaultwarden()["containers"] if c["name"] == container)
        ref = next(e for e in c["env"] if e["name"] == "ADMIN_TOKEN")
        return ref["valueFrom"]["secretKeyRef"]

    def test_nobody_can_sign_up_or_be_invited_by_another_user(self):
        env = self.env(self.vaultwarden(), "vaultwarden")
        self.assertEqual(env["SIGNUPS_ALLOWED"], "false")
        self.assertEqual(env["INVITATIONS_ALLOWED"], "false")

    def test_the_admin_page_opens_with_the_password_chosen_at_setup(self):
        ref = self.admin_secret_ref("vaultwarden")
        secret = next(
            d
            for d in self.docs()
            if d["kind"] == "Secret" and d["metadata"]["name"] == ref["name"]
        )
        self.assertEqual(
            secret["stringData"][ref["key"]],
            check_charts.LINT_VALUES["config.admin_password"],
        )
        self.assertEqual(self.admin_secret_ref("invite-admin"), ref)

    def test_the_email_given_at_setup_is_the_one_invited(self):
        env = self.env(self.vaultwarden(), "invite-admin")
        self.assertEqual(
            env["ADMIN_EMAIL"], check_charts.LINT_VALUES["config.admin_email"]
        )

    def test_without_an_email_the_chart_refuses_to_render(self):
        text, err = check_charts.render(
            self.chart, self.library, self.tmp.name, {"config.admin_email": ""}
        )
        self.assertIsNone(text)
        self.assertIn("config.admin_email is required", err)

    def run_links(self, env_file):
        gateway = self.deployments(self.docs())["gateway"]
        init = next(c for c in gateway["initContainers"] if c["name"] == "vault-links")
        with tempfile.TemporaryDirectory() as root:
            Path(root, "env").write_text(env_file)
            script = init["command"][-1].replace("/yolab", root)
            out = subprocess.run(
                ["sh", "-c", script], capture_output=True, text=True, check=True
            )
        return out.stdout.splitlines()

    def test_the_links_to_start_with_point_at_the_app_address(self):
        self.assertEqual(
            self.run_links("export YOLAB_FQDN=vault.example.test\n"),
            [
                "YOLAB_OUTPUT signup_url https://vault.example.test/#/signup",
                "YOLAB_OUTPUT admin_url https://vault.example.test/admin",
            ],
        )

    def test_without_a_yolab_address_the_links_say_none(self):
        self.assertEqual(
            self.run_links(""),
            ["YOLAB_OUTPUT signup_url NONE", "YOLAB_OUTPUT admin_url NONE"],
        )


def env_part_failures(*containers):
    found = []
    check_charts.check_env_parts(
        "demo", [pod(*containers)], lambda app, msg: found.append(msg)
    )
    return found


def caddy_dns_failures(
    caddyfile, caddy_env=("ACCOUNT_TOKEN", "PLATFORM_API_URL"), tunneled=True
):
    gateway = pod(
        {"name": "caddy", "env": [{"name": n, "value": "x"} for n in caddy_env]}
    )
    if tunneled:
        gateway["spec"]["template"]["spec"]["initContainers"] = [
            {"name": "wg-register"}
        ]
    config = {
        "kind": "ConfigMap",
        "metadata": {"name": "demo-caddy"},
        "data": {"Caddyfile": caddyfile},
    }
    found = []
    check_charts.check_caddy_dns(
        "demo", [gateway, config], lambda app, msg: found.append(msg)
    )
    return found


DNS_CADDYFILE = (
    "\n{\n  cert_issuer acme {\n    dns acmedns {\n      subdomain apps\n    }\n  }\n"
    "  cert_issuer acme\n}\n{$YOLAB_SITES} {\n  reverse_proxy app:80\n}\n"
)


class CaddyDns(unittest.TestCase):
    def test_a_gateway_proving_its_names_through_dns_passes(self):
        self.assertEqual(caddy_dns_failures(DNS_CADDYFILE), [])

    def test_a_gateway_waiting_for_let_s_encrypt_to_reach_it_is_reported(self):
        found = caddy_dns_failures("{$YOLAB_SITES} {\n  reverse_proxy app:80\n}\n")
        self.assertEqual(len(found), 1)
        self.assertIn("demo-caddy", found[0])

    def test_caddy_without_the_account_token_is_reported(self):
        found = caddy_dns_failures(DNS_CADDYFILE, caddy_env=("PLATFORM_API_URL",))
        self.assertEqual(len(found), 1)
        self.assertIn("ACCOUNT_TOKEN", found[0])

    def test_a_gateway_without_a_tunnel_has_nothing_to_prove(self):
        self.assertEqual(
            caddy_dns_failures("http://:8080 {\n}\n", caddy_env=(), tunneled=False), []
        )


class GatewayProvesNamesThroughDns(RenderedChart):
    CHART = "vaultwarden"

    def test_the_account_token_reaches_caddy_from_the_tunnel_secret(self):
        docs = self.docs()
        gateway = self.deployments(docs)["gateway"]
        caddy = next(c for c in gateway["containers"] if c["name"] == "caddy")
        token = next(e for e in caddy["env"] if e["name"] == "ACCOUNT_TOKEN")
        self.assertEqual(token["valueFrom"]["secretKeyRef"]["key"], "account-token")
        self.assertNotIn("value", token)

    def test_every_yolab_name_is_proven_through_the_apps_bucket(self):
        caddyfile = next(
            d["data"]["Caddyfile"]
            for d in self.docs()
            if d["kind"] == "ConfigMap" and d["metadata"]["name"].endswith("-caddy")
        )
        self.assertTrue(caddyfile.lstrip().startswith(check_charts.CADDY_DNS_GLOBAL))
        self.assertIn("subdomain apps", caddyfile)
        self.assertIn("password {env.ACCOUNT_TOKEN}", caddyfile)


class EnvParts(unittest.TestCase):
    def test_appending_to_the_shared_env_file_is_reported(self):
        found = env_part_failures(
            shell("host", "printf 'export YOLAB_SITES=x\\n' >> /yolab/env\n")
        )
        self.assertEqual(len(found), 1)

    def test_overwriting_the_shared_env_file_is_reported(self):
        found = env_part_failures(
            shell("tunnel", "printf 'export YOLAB_FQDN=x\\n' > /yolab/env\n")
        )
        self.assertEqual(len(found), 1)

    def test_writing_the_loader_is_allowed(self):
        script = "cat > /yolab/env <<'YOLAB_ENV'\n. /yolab/env.d/env\nYOLAB_ENV\n"
        self.assertEqual(env_part_failures(shell("tunnel", script)), [])

    def test_a_part_the_loader_never_sources_is_reported(self):
        found = env_part_failures(
            shell("extra", "printf 'export X=1\\n' > /yolab/env.d/extra\n")
        )
        self.assertEqual(len(found), 1)
        self.assertIn("extra", found[0])

    def test_appending_to_a_part_is_reported(self):
        found = env_part_failures(
            shell("host", "printf 'export X=1\\n' >> /yolab/env.d/host\n")
        )
        self.assertEqual(len(found), 1)

    def test_rewriting_an_own_part_passes(self):
        script = "printf 'export X=1\\n' > /yolab/env.d/host\n"
        self.assertEqual(env_part_failures(shell("host", script)), [])


class EnvSurvivesALoneInitRerun(RenderedChart):
    CHART = "vaultwarden"

    def run_init(self, root, script, **env):
        fake_setup = (
            "printf 'export YOLAB_FQDN=app.example.test\\n"
            'export YOLAB_URL=https://app.example.test\\n\' > "$YOLAB_DIR/env"'
        )
        script = script.replace("exec /bin/sh /setup.sh", fake_setup)
        script = script.replace("/yolab", root)
        subprocess.run(
            ["sh", "-c", script],
            env={"PATH": os.environ["PATH"], "YOLAB_DIR": f"{root}/env.d", **env},
            check=True,
            capture_output=True,
        )

    def sourced(self, root, var):
        out = subprocess.run(
            ["sh", "-c", f'. "{root}/env"; printf %s "${var}"'],
            check=True,
            capture_output=True,
            text=True,
        )
        return out.stdout

    def test_rerunning_only_wg_register_keeps_the_host_lines(self):
        spec = self.deployments(self.docs())["gateway"]
        inits = {c["name"]: c["command"][2] for c in spec["initContainers"]}
        with tempfile.TemporaryDirectory() as root:
            self.run_init(root, inits["wg-register"])
            self.run_init(root, inits["yolab-host"], CUSTOM_DOMAIN="example.org")
            self.run_init(root, inits["wg-register"])
            self.assertEqual(
                self.sourced(root, "YOLAB_SITES"), "app.example.test, example.org"
            )
            self.assertEqual(self.sourced(root, "YOLAB_URL"), "https://example.org")

    def test_the_loader_sources_every_part_the_check_allows(self):
        spec = self.deployments(self.docs())["gateway"]
        script = next(
            c["command"][2]
            for c in spec["initContainers"]
            if c["name"] == "wg-register"
        )
        self.assertIn(
            f"for yolab_env_part in {' '.join(check_charts.ENV_PARTS)}; do", script
        )


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


class ImmichFiles(RenderedChart):
    CHART = "immich"

    def test_photos_uploaded_to_immich_stay_in_its_own_data(self):
        server = self.deployments(self.docs())["immich-server"]
        library = next(
            m
            for m in server["containers"][0]["volumeMounts"]
            if m["mountPath"] == "/usr/src/app/upload"
        )
        self.assertEqual(library["name"], "data")
        self.assertEqual(library["subPath"], "uploads")
        self.assertNotIn("UPLOAD_LOCATION", self.env(server, "immich-server"))

    def test_it_gets_no_second_folder_it_would_never_look_in(self):
        docs = self.docs()
        self.assertEqual([d for d in docs if check_charts.is_files_doc(d)], [])
        for _, spec in check_charts.pod_specs(docs):
            self.assertNotIn("files", [v["name"] for v in spec.get("volumes") or []])


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


def pa_output():
    return output(format="uri")


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
            else {"tor_url": pa_output(), "tailscale_url": pa_output()},
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
        found = offer_failures(self.offered(outputs={"tor_url": pa_output()}))
        self.assertEqual(len(found), 1)
        self.assertIn("tailscale_url", found[0])


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


def yolab_switch_schema(top_tunnel=False, off_required=True, token=YOLAB_TOKEN):
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
        outputs={"url": output(format="uri")},
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

    def test_every_app_with_a_subdomain_has_the_switch(self):

        for path in Path(check_charts.HERE).glob("*/values.schema.json"):
            if check_charts.pack_members(path.with_name("Chart.yaml").read_text()):
                continue
            config = json.loads(path.read_text())["properties"]["config"]
            deps = config.get("dependencies") or {}
            has_address = "yolab_enabled" in deps or any(
                p.get("format") == "tunnel"
                for p in (config.get("properties") or {}).values()
            )
            if has_address and path.parent.name not in check_charts.ADDRESS_ONLY:
                self.assertIn("yolab_enabled", config["properties"], path.parent.name)

    def test_an_app_that_only_works_on_its_address_offers_no_switch(self):
        for app in check_charts.ADDRESS_ONLY:
            schema = Path(check_charts.HERE, app, "values.schema.json").read_text()
            config = json.loads(schema)["properties"]["config"]
            self.assertNotIn("yolab_enabled", config["properties"], app)


YOLAB_OFF = {**PRIVATE_ON, "config.yolab_enabled": "false"}


class YolabOffRendered(RenderedChart):
    CHART = "vaultwarden"

    def test_on_by_default_the_tunnel_is_registered_as_before(self):
        spec = self.deployments(self.docs())["gateway"]
        self.assertIn("wg-register", {c["name"] for c in spec["initContainers"]})
        self.assertIn("wireguard", {c["name"] for c in spec["containers"]})

    def test_off_no_tunnel_wireguard_or_public_site(self):
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
        self.assertIn("> /yolab/env.d/env", script)

    def test_a_tunnel_left_running_while_off_is_reported(self):
        docs = self.docs(YOLAB_OFF)
        spec = self.deployments(docs)["gateway"]
        spec["containers"].append({"name": "wireguard", "image": "x"})
        found = []
        check_charts.check_yolab_off(
            "vaultwarden", docs, lambda app, msg: found.append(msg)
        )
        self.assertEqual(len(found), 1)


class OutputsRendered(RenderedChart):
    CHART = "grafana"

    def inits(self, extra=None):
        spec = self.deployments(self.docs(extra))["gateway"]
        return {c["name"]: c for c in spec["initContainers"]}

    def off(self, extra=None):
        init = self.inits(extra).get("outputs-off")
        return init["command"][2] if init else ""

    def test_nothing_switched_off_needs_no_outputs_off_step(self):
        self.assertNotIn("outputs-off", self.inits({**PRIVATE_ON}))

    def test_a_way_in_switched_off_prints_none_so_its_old_address_goes(self):
        script = self.off()
        self.assertIn("YOLAB_OUTPUT tor_url NONE", script)
        self.assertIn("YOLAB_OUTPUT tailscale_url NONE", script)
        self.assertNotIn("YOLAB_OUTPUT url NONE", script)
        self.assertNotIn("YOLAB_OUTPUT ipv6 NONE", script)

    def test_the_yolab_address_switched_off_prints_none(self):
        script = self.off(YOLAB_OFF)
        self.assertIn("YOLAB_OUTPUT url NONE", script)
        self.assertIn("YOLAB_OUTPUT ipv6 NONE", script)

    def test_a_way_in_switched_on_is_never_printed_none(self):
        script = self.off(PRIVATE_ON)
        self.assertNotIn("YOLAB_OUTPUT tor_url NONE", script)
        self.assertNotIn("YOLAB_OUTPUT tailscale_url NONE", script)

    def test_an_api_key_the_chart_does_not_offer_is_never_printed_none(self):
        self.assertNotIn("api_key", self.off())

    def test_a_generated_password_is_printed_from_its_secret(self):
        outputs = self.inits()["outputs"]
        env = {e["name"]: e for e in outputs["env"]}
        ref = env["OUTPUT_ADMIN_PASSWORD"]["valueFrom"]["secretKeyRef"]
        self.assertEqual(ref, {"name": "release-secrets", "key": "admin_password"})
        self.assertIn(
            "YOLAB_OUTPUT admin_password ${OUTPUT_ADMIN_PASSWORD:-NONE}",
            outputs["command"][2],
        )


class ValheimPasswordOutput(RenderedChart):
    CHART = "valheim"

    def test_a_server_without_a_password_prints_none_for_it(self):
        docs = self.docs({"config.server_pass": ""})
        outputs = next(
            c
            for spec in self.deployments(docs).values()
            for c in spec.get("initContainers") or []
            if c["name"] == "outputs"
        )
        env = {e["name"]: e.get("value") for e in outputs["env"]}
        self.assertEqual(env["OUTPUT_SERVER_PASS"], "")
        self.assertIn("${OUTPUT_SERVER_PASS:-NONE}", outputs["command"][2])


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


def tree_pair(claim, path):
    pv = f"{check_charts.NAMESPACE}.{claim}"
    return [
        {
            "kind": "PersistentVolume",
            "metadata": {
                "name": pv,
                "labels": {check_charts.TREE_LABEL: "true"},
                "annotations": {check_charts.TREE_PATH: path},
            },
            "spec": {
                "accessModes": ["ReadWriteMany"],
                "capacity": {"storage": check_charts.VOLUME_SIZE},
                "persistentVolumeReclaimPolicy": "Retain",
                "storageClassName": "",
                "volumeMode": "Filesystem",
                "claimRef": {"namespace": check_charts.NAMESPACE, "name": claim},
                "csi": {
                    "driver": "cephfs.csi.ceph.com",
                    "volumeHandle": pv,
                    "nodeStageSecretRef": {
                        "name": "yolab-cephfs-static",
                        "namespace": "ceph-csi",
                    },
                    "volumeAttributes": {
                        "clusterID": "yolab",
                        "fsName": "yolab-fs",
                        "staticVolume": "true",
                        "rootPath": f"/tree/{path}",
                    },
                },
            },
        },
        {
            "kind": "PersistentVolumeClaim",
            "metadata": {
                "name": claim,
                "namespace": check_charts.NAMESPACE,
                "labels": {check_charts.TREE_LABEL: "true"},
                "annotations": {check_charts.TREE_PATH: path},
            },
            "spec": {
                "accessModes": ["ReadWriteMany"],
                "storageClassName": "",
                "volumeName": pv,
                "resources": {"requests": {"storage": check_charts.VOLUME_SIZE}},
            },
        },
    ]


class TreeVolumes(unittest.TestCase):
    def tree(self, *docs):
        return collected(check_charts.check_tree_volumes, "x", list(docs))

    def test_a_folder_of_the_tree_rendered_by_yolab_common_passes(self):
        self.assertEqual(self.tree(*tree_pair("release-data", "release")), [])

    def test_a_volume_the_cluster_makes_up_on_its_own_is_refused(self):
        _, claim = tree_pair("release-data", "release")
        claim["spec"]["storageClassName"] = "yolab-cephfs"
        del claim["spec"]["volumeName"]
        found = self.tree(claim)
        self.assertTrue(any("storageClassName" in f for f in found), found)
        self.assertTrue(any("volumeName" in f for f in found), found)

    def test_a_claim_without_its_folder_is_refused(self):
        _, claim = tree_pair("release-data", "release")
        found = self.tree(claim)
        self.assertTrue(any("does not render" in f for f in found), found)

    def test_a_claim_that_does_not_say_its_folder_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        del claim["metadata"]["annotations"]
        found = self.tree(pv, claim)
        self.assertTrue(any("which folder" in f for f in found), found)

    def test_a_volume_outside_the_tree_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        pv["spec"]["csi"]["volumeAttributes"]["rootPath"] = "/volumes/csi/x"
        found = self.tree(pv, claim)
        self.assertTrue(any("its attributes" in f for f in found), found)

    def test_a_volume_whose_data_goes_when_it_is_deleted_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        pv["spec"]["persistentVolumeReclaimPolicy"] = "Delete"
        found = self.tree(pv, claim)
        self.assertTrue(any("reclaim policy" in f for f in found), found)

    def test_a_volume_bound_to_another_claim_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        pv["spec"]["claimRef"]["name"] = "other"
        found = self.tree(pv, claim)
        self.assertTrue(any("claimRef" in f for f in found), found)

    def test_a_volume_mounted_with_other_credentials_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        pv["spec"]["csi"]["nodeStageSecretRef"]["name"] = "csi-cephfs-node"
        found = self.tree(pv, claim)
        self.assertTrue(any("its secret" in f for f in found), found)

    def test_a_volume_naming_another_folder_than_its_claim_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        pv["metadata"]["annotations"][check_charts.TREE_PATH] = "other"
        found = self.tree(pv, claim)
        self.assertTrue(any("its folder" in f for f in found), found)

    def test_a_volume_no_claim_binds_is_refused(self):
        pv, _ = tree_pair("release-data", "release")
        found = self.tree(pv)
        self.assertTrue(any("no claim of the chart binds" in f for f in found), found)

    def test_a_volume_with_a_size_of_its_own_is_refused(self):
        pv, claim = tree_pair("release-data", "release")
        claim["spec"]["resources"]["requests"]["storage"] = "10Gi"
        found = self.tree(pv, claim)
        self.assertTrue(any("its size" in f for f in found), found)

    def test_a_storage_size_question_is_refused(self):
        s = schema(config={"storage_size": {"type": "string", "title": "Storage size"}})
        found = collected(check_charts.check_schema, "x", s, "")
        self.assertTrue(any("storage_size is gone" in f for f in found), found)


class FilesRules(unittest.TestCase):
    FOLDER = check_charts.FILES_FOLDER

    def deployment(self, claim=None, **mount_overrides):
        mount = {"name": "files", "mountPath": check_charts.FILES_MOUNT}
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
                                "name": "files",
                                "persistentVolumeClaim": {
                                    "claimName": claim
                                    or check_charts.files_claim(self.FOLDER)
                                },
                            }
                        ],
                    }
                }
            },
        }

    def own_files(self, folder=None):
        folder = folder or self.FOLDER
        return tree_pair(check_charts.files_claim(folder), folder)

    def files(self, *docs, wanted=True, folder=None):
        chosen = folder or self.FOLDER
        return collected(
            lambda app, d, w, fail: check_charts.check_files(app, d, w, fail, chosen),
            "x",
            list(docs),
            wanted,
        )

    def test_a_chart_that_mounts_its_files_whole_passes(self):
        self.assertEqual(self.files(*self.own_files(), self.deployment()), [])

    def test_files_left_in_another_folder_than_the_chosen_one_are_refused(self):
        found = self.files(*self.own_files("elsewhere"), self.deployment())
        self.assertTrue(any("'elsewhere'" in f for f in found), found)

    def test_the_files_claim_is_named_after_its_folder(self):
        pv, claim = tree_pair("content-00000000", self.FOLDER)
        found = self.files(pv, claim, self.deployment(claim="content-00000000"))
        self.assertTrue(any("the name follows the folder" in f for f in found), found)

    def test_another_folder_is_another_claim(self):
        self.assertNotEqual(
            check_charts.files_claim("radarr.files"),
            check_charts.files_claim("media"),
        )
        self.assertRegex(check_charts.files_claim("media"), r"^content-[0-9a-f]{8}$")

    def test_files_mounted_somewhere_else_are_refused(self):
        found = self.files(*self.own_files(), self.deployment(mountPath="/media"))
        self.assertTrue(any("must see the same paths" in f for f in found), found)

    def test_files_mounted_in_part_are_refused(self):
        found = self.files(*self.own_files(), self.deployment(subPath="movies"))
        self.assertTrue(any("hardlinks" in f for f in found), found)

    def test_files_nobody_mounts_are_refused(self):
        found = self.files(*self.own_files())
        self.assertTrue(any("no container mounts" in f for f in found), found)

    def test_a_chart_that_asks_for_files_must_render_its_claim(self):
        found = self.files(self.deployment())
        self.assertTrue(any("must render one content claim" in f for f in found), found)

    def test_files_used_without_asking_for_them_are_refused(self):
        found = self.files(*self.own_files(), self.deployment(), wanted=False)
        self.assertTrue(any("yolab.files" in f for f in found), found)

    def test_a_chart_without_files_needs_nothing(self):
        self.assertEqual(self.files(wanted=False), [])

    def test_the_files_volume_and_claim_do_not_count_as_the_services_own(self):
        pv, claim = self.own_files()
        own_pv, own_claim = tree_pair("release-data", "release")
        self.assertTrue(check_charts.is_files_doc(pv))
        self.assertTrue(check_charts.is_files_doc(claim))
        self.assertFalse(check_charts.is_files_doc(own_pv))
        self.assertFalse(check_charts.is_files_doc(own_claim))

    def test_a_chart_with_files_must_let_people_choose_their_folder(self):
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            values = Path(d, "values.yaml")
            values.write_text("yolab:\n  files: true\n")
            self.assertEqual(
                collected(
                    check_charts.check_files_folder_offer, "x", folder_offer(), values
                ),
                [],
            )
            found = collected(
                check_charts.check_files_folder_offer, "x", schema(), values
            )
            self.assertTrue(any("files_folder_existing" in f for f in found), found)
            values.write_text("yolab:\n  files: false\n")
            self.assertEqual(
                collected(check_charts.check_files_folder_offer, "x", schema(), values),
                [],
            )


def folder_offer(switch=None, chosen=None, required=("files_folder",)):
    return {
        "properties": {
            "config": {
                "properties": {
                    "files_folder_existing": switch
                    or {"type": "boolean", "default": False}
                },
                "dependencies": {
                    "files_folder_existing": {
                        "oneOf": [
                            {"properties": {"files_folder_existing": {"const": False}}},
                            {
                                "properties": {
                                    "files_folder_existing": {"const": True},
                                    "files_folder": chosen
                                    or {"type": "string", "format": "tree-folder"},
                                },
                                "required": list(required),
                            },
                        ]
                    }
                },
            }
        }
    }


class FilesFolderOffer(unittest.TestCase):
    def test_the_switch_with_a_tree_folder_behind_it_passes(self):
        self.assertIsNone(check_charts.files_folder_offer_problem(folder_offer()))

    def test_a_folder_asked_outright_is_refused(self):
        s = folder_offer()
        s["properties"]["config"]["properties"]["files_folder"] = {"type": "string"}
        self.assertIn("outright", check_charts.files_folder_offer_problem(s))

    def test_a_switch_on_by_default_is_refused(self):
        problem = check_charts.files_folder_offer_problem(
            folder_offer(switch={"type": "boolean", "default": True})
        )
        self.assertIn("off by default", problem)

    def test_a_free_text_folder_is_refused(self):
        problem = check_charts.files_folder_offer_problem(
            folder_offer(chosen={"type": "string"})
        )
        self.assertIn("tree-folder", problem)

    def test_an_optional_folder_behind_the_switch_is_refused(self):
        problem = check_charts.files_folder_offer_problem(folder_offer(required=()))
        self.assertIn("must require files_folder", problem)

    def test_every_catalog_chart_with_files_offers_its_folder_the_same_way(self):
        for chart in sorted(Path(check_charts.HERE).glob("*/values.yaml")):
            if not check_charts.uses_files(yaml.safe_load(chart.read_text()) or {}):
                continue
            s = json.loads((chart.parent / "values.schema.json").read_text())
            self.assertIsNone(
                check_charts.files_folder_offer_problem(s), chart.parent.name
            )


class StateSubPaths(unittest.TestCase):
    def test_a_state_folder_is_found_with_or_without_a_parent(self):
        self.assertTrue(check_charts.ends_in("tor", "tor"))
        self.assertTrue(check_charts.ends_in('"tor"', "tor"))
        self.assertTrue(check_charts.ends_in("state/tor/", "tor"))
        self.assertFalse(check_charts.ends_in("tor-data", "tor"))
        self.assertFalse(check_charts.ends_in("mytor", "tor"))


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


def no_explorer_failures(schema, values=None):
    found = []
    check_charts.check_no_file_explorer(
        "demo", schema, values or {}, lambda app, msg: found.append(msg)
    )
    return found


class NoFileExplorerOfItsOwn(unittest.TestCase):
    def test_a_service_without_an_explorer_of_its_own_passes(self):
        self.assertEqual(no_explorer_failures(GOOD, {"yolab": {"files": True}}), [])

    def test_an_explorer_switch_left_in_the_schema_is_reported(self):
        s = schema(
            config={"file_explorer_enabled": {"type": "boolean", "default": False}}
        )
        found = no_explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("file_explorer_enabled", found[0])
        self.assertIn("File Browser", found[0])

    def test_an_explorer_setting_inside_a_switch_or_an_output_is_reported(self):
        s = schema(
            config={"yolab_enabled": {"type": "boolean"}},
            outputs={"file_explorer_url": output(format="uri")},
        )
        s["properties"]["config"]["dependencies"] = {
            "yolab_enabled": {
                "oneOf": [
                    {"properties": {"file_explorer_subdomain": {"type": "string"}}}
                ]
            }
        }
        found = no_explorer_failures(s)
        self.assertEqual(len(found), 1)
        self.assertIn("file_explorer_subdomain, file_explorer_url", found[0])

    def test_explorer_values_left_behind_are_reported(self):
        found = no_explorer_failures(
            GOOD, {"yolab": {"fileExplorer": {"protect": ["postgres"]}}}
        )
        self.assertEqual(len(found), 1)
        self.assertIn("yolab.fileExplorer", found[0])

    def test_no_catalog_service_carries_its_own_explorer(self):
        for chart in sorted(Path(check_charts.HERE).glob("*/Chart.yaml")):
            schema_path = chart.parent / "values.schema.json"
            values_path = chart.parent / "values.yaml"
            self.assertEqual(
                no_explorer_failures(
                    json.loads(schema_path.read_text()) if schema_path.exists() else {},
                    yaml.safe_load(values_path.read_text())
                    if values_path.exists()
                    else {},
                ),
                [],
                chart.parent.name,
            )


def api_key_failures(s):
    found = []
    check_charts.check_api_key_offer("demo", s, lambda app, msg: found.append(msg))
    return found


API_KEY_OUTPUT = output(format="secret")


class ApiKeyOffer(unittest.TestCase):
    def test_an_offered_key_must_be_readable_as_a_secret(self):
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
        off = next(
            c["command"][2]
            for spec in self.deployments(docs).values()
            for c in spec.get("initContainers") or []
            if c["name"] == "outputs-off"
        )
        self.assertIn("YOLAB_OUTPUT api_key NONE", off)
        self.assertIn("YOLAB_OUTPUT api_url NONE", off)

    def test_the_key_is_kept_on_the_apps_volume_so_restarts_keep_it(self):
        gateway = self.deployments(self.docs())["gateway"]
        init = next(c for c in gateway["initContainers"] if c["name"] == "api-key-init")
        mount = next(m for m in init["volumeMounts"] if m["mountPath"] == "/api-key")
        self.assertEqual(mount["name"], "data")
        self.assertEqual(mount["subPath"], "api-key")


def collected(fn, *args):
    found = []
    fn(*args, lambda app, msg: found.append(msg))
    return found


class NoLinks(unittest.TestCase):
    def test_an_address_typed_in_plain_boxes_passes(self):
        s = schema(
            config={
                "client": {
                    "type": "object",
                    "properties": {
                        "internal_url": {"type": "string"},
                        "password": {"type": "string"},
                    },
                }
            }
        )
        self.assertEqual(collected(check_charts.check_no_links, "x", s), [])

    def test_every_way_of_linking_to_another_app_is_refused(self):
        for fmt in ("yolab-output", "connection", "service-url", "app"):
            s = schema(config={"client": {"type": "object", "format": fmt}})
            found = collected(check_charts.check_no_links, "x", s)
            self.assertTrue(any("links are gone" in f for f in found), (fmt, found))
        for key in ("x-yolab-from", "x-yolab-outputs"):
            s = schema(config={"client": {"type": "object", key: ["qbittorrent"]}})
            found = collected(check_charts.check_no_links, "x", s)
            self.assertTrue(any("links are gone" in f for f in found), (key, found))
        provides = {**schema(), "x-yolab-provides": {"ollama": ["api"]}}
        found = collected(check_charts.check_no_links, "x", provides)
        self.assertTrue(any("x-yolab-provides" in f for f in found), found)

    def test_a_link_behind_a_switch_is_refused_too(self):
        s = schema(
            extra={
                "config": {
                    "type": "object",
                    "properties": {"mode": {"type": "string"}},
                    "dependencies": {
                        "mode": {
                            "oneOf": [
                                {"properties": {"client": {"format": "yolab-output"}}}
                            ]
                        }
                    },
                }
            }
        )
        found = collected(check_charts.check_no_links, "x", s)
        self.assertTrue(any("config.client" in f for f in found), found)

    def test_no_chart_in_the_catalog_links_to_another_app(self):
        for app, (_, s) in check_charts.catalog_charts().items():
            self.assertEqual(collected(check_charts.check_no_links, app, s), [], app)


PACK_CHART = """apiVersion: v2
name: watch
dependencies:
  - name: yolab-stack
    version: "0.1.0"
  - name: jellyfin
    version: "{jellyfin}"
"""


class PackDeclaration(unittest.TestCase):
    CHARTS: ClassVar[dict] = {"jellyfin": ("0.3.0", {})}

    def failures(self, **fill):
        text = PACK_CHART.format(**{"jellyfin": "0.3.0", **fill})
        return collected(
            check_charts.check_pack_declaration, "watch", text, self.CHARTS
        )

    def test_a_pack_naming_its_apps_at_their_versions_passes(self):
        self.assertEqual(self.failures(), [])

    def test_a_pack_pinning_an_old_version_of_an_app_is_reported(self):
        found = self.failures(jellyfin="0.2.12")
        self.assertTrue(any("bump the pack" in f for f in found), found)

    def test_the_retired_members_annotation_is_refused(self):
        text = PACK_CHART.format(jellyfin="0.3.0") + (
            'annotations:\n  yolab.io/members: "jellyfin"\n'
        )
        found = collected(
            check_charts.check_pack_declaration, "watch", text, self.CHARTS
        )
        self.assertTrue(any("yolab.io/members is gone" in f for f in found), found)

    def test_an_app_named_files_cannot_be_in_a_pack(self):
        text = PACK_CHART.format(jellyfin="0.3.0") + (
            '  - name: files\n    version: "0.1.0"\n'
        )
        found = collected(
            check_charts.check_pack_declaration, "watch", text, self.CHARTS
        )
        self.assertTrue(any("pack's own files folder" in f for f in found), found)

    def test_a_pack_without_yolab_stack_is_reported(self):
        text = PACK_CHART.format(jellyfin="0.3.0").replace(
            "yolab-stack", "something-else"
        )
        found = collected(
            check_charts.check_pack_declaration, "watch", text, self.CHARTS
        )
        self.assertTrue(any("yolab-stack" in f for f in found), found)

    def test_a_pack_s_apps_are_its_dependencies_minus_the_libraries(self):
        self.assertEqual(
            check_charts.pack_members(PACK_CHART.format(jellyfin="1")), ["jellyfin"]
        )
        app = (
            "name: notes\n"
            "dependencies:\n"
            "  - name: yolab-common\n"
            '    version: "0.1.15"\n'
        )
        self.assertEqual(check_charts.pack_members(app), [])
        self.assertEqual(check_charts.pack_members("name: plain\n"), [])


class PackMembers(unittest.TestCase):
    MEMBERS: ClassVar[list] = ["sonarr", "radarr", "qbittorrent"]

    def test_each_resource_belongs_to_the_app_whose_name_it_carries(self):
        for name, owner in [
            ("sonarr", "sonarr"),
            ("sonarr-gateway", "sonarr"),
            ("release-sonarr-data", "sonarr"),
            ("release-qbittorrent-uninstall", "qbittorrent"),
            ("release-keys", None),
            ("content", None),
        ]:
            self.assertEqual(check_charts.member_of(name, self.MEMBERS), owner, name)

    def test_the_longest_matching_app_name_wins(self):
        self.assertEqual(
            check_charts.member_of("qbit-vpn-gateway", ["qbit", "qbit-vpn"]), "qbit-vpn"
        )

    def test_two_apps_rendering_one_name_is_reported(self):
        docs = [
            {"kind": "Service", "metadata": {"name": "sonarr"}, "spec": {}},
            {"kind": "Service", "metadata": {"name": "sonarr"}, "spec": {}},
        ]
        found = collected(check_charts.check_pack_render, "p", docs, [])
        self.assertTrue(any("twice" in f for f in found), found)

    def test_something_no_app_owns_may_only_be_a_shared_secret_or_the_files_folder(
        self,
    ):
        files = check_charts.files_claim(check_charts.FILES_FOLDER)
        docs = [
            {"kind": "Secret", "metadata": {"name": "release-keys"}},
            *tree_pair(files, check_charts.FILES_FOLDER),
            {"kind": "PersistentVolume", "metadata": {"name": "stray.volume"}},
            {"kind": "ConfigMap", "metadata": {"name": "stray"}},
        ]
        found = collected(check_charts.check_pack_render, "p", docs, [])
        self.assertEqual(
            found,
            [
                "renders PersistentVolume stray.volume, which belongs to none of its apps",
                "renders ConfigMap stray, which belongs to none of its apps",
            ],
        )

    def test_an_apps_folder_belongs_to_the_app_whose_claim_it_binds(self):
        pv, claim = tree_pair("release-sonarr-data", "release.sonarr")
        parts, shared = check_charts.split_by_member([pv, claim], self.MEMBERS)
        self.assertEqual(parts["sonarr"], [pv, claim])
        self.assertEqual(shared, [])

    def test_pack_outputs_carry_their_apps_name(self):
        s = schema(
            outputs={
                "sonarr_url": output(format="uri"),
                "url": output(format="uri"),
                "radarr_api_key": output(format="secret"),
            }
        )
        found = collected(check_charts.check_pack_outputs, "p", s, self.MEMBERS)
        self.assertEqual(len(found), 1, found)
        self.assertIn("outputs.url does not start", found[0])


class Composable(unittest.TestCase):
    def test_every_app_a_pack_is_built_from_can_run_inside_it(self):
        found = []
        members = set()
        for chart_yaml in Path(check_charts.HERE).glob("*/Chart.yaml"):
            members.update(check_charts.pack_members(chart_yaml.read_text()))
        self.assertTrue(members)
        for member in sorted(members):
            check_charts.check_composable(
                member,
                os.path.join(check_charts.HERE, member),
                lambda app, msg: found.append(f"{app}: {msg}"),
            )
        self.assertEqual(found, [])

    def test_a_chart_that_would_render_twice_inside_a_pack_is_reported(self):
        with tempfile.TemporaryDirectory() as root:
            Path(root, "templates").mkdir()
            Path(root, "templates", "app.yaml").write_text(
                '{{ define "demo.manifests" }}{{ end }}{{ include "demo.manifests" . }}'
            )
            found = collected(check_charts.check_composable, "demo", root)
        self.assertEqual(len(found), 1, found)
        self.assertIn("global.stack", found[0])


class PackRendered(RenderedChart):
    CHART = "movies-tv"

    def named(self, docs, kind, name):
        return next(
            d for d in docs if d["kind"] == kind and d["metadata"]["name"] == name
        )

    def test_the_catalog_packs_pass_every_check(self):
        charts = check_charts.catalog_charts()
        packs = [
            str(p.parent)
            for p in Path(check_charts.HERE).glob("*/Chart.yaml")
            if check_charts.pack_members(p.read_text())
        ]
        self.assertTrue(packs)
        found = []
        for pack in packs:
            check_charts.check_pack(
                pack,
                charts,
                self.library,
                self.tmp.name,
                lambda w, m: found.append(f"{w}: {m}"),
            )
        self.assertEqual(found, [])

    def test_every_app_runs_in_its_own_pod_with_its_own_address(self):
        docs = self.docs({"yolab.serviceName": "media"})
        pods = self.deployments(docs)
        self.assertEqual(
            set(pods),
            {
                "jellyfin-gateway",
                "qbittorrent-gateway",
                "sonarr-gateway",
                "radarr-gateway",
                "prowlarr-gateway",
                "jellyseerr-gateway",
                "bazarr-gateway",
            },
        )
        register = next(
            c
            for c in pods["sonarr-gateway"]["initContainers"]
            if c["name"] == "wg-register"
        )
        env = {e["name"]: e.get("value") for e in register["env"]}
        self.assertEqual(env["SERVICE_NAME"], "media-sonarr")

    def test_the_apps_share_one_files_folder_made_by_the_pack_itself(self):
        docs = self.docs()
        claim = check_charts.files_claim(check_charts.FILES_FOLDER)
        shared = sorted(
            (d["kind"], d["metadata"]["name"])
            for d in docs
            if check_charts.is_files_doc(d)
        )
        self.assertEqual(
            shared,
            [
                ("PersistentVolume", f"{check_charts.NAMESPACE}.{claim}"),
                ("PersistentVolumeClaim", claim),
            ],
        )

    def test_each_app_of_a_pack_keeps_its_data_in_a_folder_of_its_own(self):
        folders = {
            d["metadata"]["name"]: check_charts.tree_path(d)
            for d in self.docs()
            if d["kind"] == "PersistentVolumeClaim" and not check_charts.is_files_doc(d)
        }
        self.assertEqual(folders["release-sonarr-data"], "release.sonarr")
        self.assertEqual(folders["release-radarr-data"], "release.radarr")
        self.assertEqual(len(set(folders.values())), len(folders))

    def test_choosing_the_packs_files_folder_moves_every_app_to_it(self):
        docs = self.docs(
            {"config.files_folder_existing": "true", "config.files_folder": "media"}
        )
        claim = check_charts.files_claim("media")
        mounted = {
            v["persistentVolumeClaim"]["claimName"]
            for _, spec in check_charts.pod_specs(docs)
            for v in spec.get("volumes") or []
            if v["name"] == "files"
        }
        self.assertEqual(mounted, {claim})
        files = [d for d in docs if check_charts.is_files_doc(d)]
        self.assertEqual({check_charts.tree_path(d) for d in files}, {"media"})

    def test_choosing_the_root_for_the_pack_moves_every_app_to_the_whole_tree(self):
        docs = self.docs(
            {"config.files_folder_existing": "true", "config.files_folder": "/"}
        )
        claim = check_charts.files_claim("/")
        mounted = {
            v["persistentVolumeClaim"]["claimName"]
            for _, spec in check_charts.pod_specs(docs)
            for v in spec.get("volumes") or []
            if v["name"] == "files"
        }
        self.assertEqual(mounted, {claim})
        files = [d for d in docs if check_charts.is_files_doc(d)]
        self.assertEqual({check_charts.tree_path(d) for d in files}, {""})

    def test_only_the_apps_that_use_files_mount_it_inside_the_pack(self):
        pods = self.deployments(self.docs())
        mounted = {
            pod
            for pod, spec in pods.items()
            if any(v["name"] == "files" for v in spec.get("volumes") or [])
        }
        self.assertIn("sonarr-gateway", mounted)
        self.assertIn("jellyfin-gateway", mounted)
        self.assertNotIn("prowlarr-gateway", mounted)
        for c in pods["prowlarr-gateway"]["containers"]:
            self.assertNotIn(
                "files", [m["name"] for m in c.get("volumeMounts") or []], c["name"]
            )

    def test_the_arrs_send_downloads_to_the_packs_own_qbittorrent(self):
        docs = self.docs()
        pods = self.deployments(docs)
        sonarr = self.env(pods["sonarr-gateway"], "configure")
        login = next(
            c
            for c in pods["qbittorrent-gateway"]["initContainers"]
            if c["name"] == "login"
        )
        password = {e["name"]: e.get("value") for e in login["env"]}["PASSWORD"]
        self.assertEqual(sonarr["DC_URL"], "http://qbittorrent:8080")
        self.assertEqual(sonarr["DC_PASSWORD"], password)
        self.assertEqual(sonarr["ROOT_FOLDER"], "/files/shows")
        self.assertEqual(
            self.env(pods["radarr-gateway"], "configure")["ROOT_FOLDER"],
            "/files/movies",
        )

    def test_prowlarr_is_handed_the_keys_the_arrs_actually_run_with(self):
        pods = self.deployments(self.docs())
        prowlarr = self.env(pods["prowlarr-gateway"], "configure")
        sonarr = self.env(pods["sonarr-gateway"], "sonarr")
        self.assertEqual(prowlarr["SONARR_URL"], "http://sonarr:8989")
        self.assertEqual(prowlarr["SONARR_KEY"], sonarr["SONARR__AUTH__APIKEY"])
        self.assertEqual(
            prowlarr["RADARR_KEY"],
            self.env(pods["radarr-gateway"], "radarr")["RADARR__AUTH__APIKEY"],
        )
        self.assertEqual(prowlarr["LIDARR_URL"], "")

    def test_generated_keys_live_in_the_packs_own_secret(self):
        secret = self.named(self.docs(), "Secret", "release-keys")
        pods = self.deployments(self.docs())
        self.assertEqual(
            set(secret["stringData"]),
            {"jellyfin", "sonarr", "radarr", "prowlarr", "qbittorrent"},
        )
        self.assertEqual(
            len(self.env(pods["sonarr-gateway"], "sonarr")["SONARR__AUTH__APIKEY"]), 32
        )

    def test_with_an_existing_download_client_the_pack_runs_no_qbittorrent(self):
        docs = self.docs(check_charts.VARIANTS["movies-tv"][0])
        pods = self.deployments(docs)
        self.assertNotIn("qbittorrent-gateway", pods)
        radarr = self.env(pods["radarr-gateway"], "configure")
        self.assertEqual(
            radarr["DC_URL"], check_charts.QBITTORRENT_LINK["internal_url"]
        )
        self.assertEqual(radarr["DC_PASSWORD"], "PlaceholderPw2026")

    def test_switching_an_app_off_leaves_its_folder_in_the_tree(self):
        docs = self.docs()
        volume = self.named(
            docs,
            "PersistentVolume",
            f"{check_charts.NAMESPACE}.release-qbittorrent-data",
        )
        self.assertEqual(volume["spec"]["persistentVolumeReclaimPolicy"], "Retain")
        self.assertEqual(check_charts.tree_path(volume), "release.qbittorrent")

    def test_each_app_prints_its_outputs_under_its_own_name(self):
        pods = self.deployments(self.docs())
        output = next(
            c
            for c in pods["jellyfin-gateway"]["initContainers"]
            if c["name"] == "stack-output"
        )
        self.assertIn("YOLAB_OUTPUT jellyfin_url", output["command"][2])
        configure = self.env(pods["sonarr-gateway"], "configure")
        self.assertEqual(configure["API_KEY_OUTPUT"], "sonarr_api_key")

    def stack_output(self, extra=None):
        pods = self.deployments(self.docs(extra))
        return next(
            c["command"][2]
            for c in pods["jellyfin-gateway"]["initContainers"]
            if c["name"] == "stack-output"
        )

    def test_an_app_switched_off_in_the_pack_prints_none_for_its_outputs(self):
        script = self.stack_output(
            {"config.requests": "false", "config.subtitles": "false"}
        )
        for key in (
            "jellyseerr_url",
            "jellyseerr_api_key",
            "bazarr_url",
            "bazarr_api_key",
        ):
            self.assertIn(f"YOLAB_OUTPUT {key} NONE", script)

    def test_apps_that_are_on_are_never_printed_none(self):
        script = self.stack_output()
        self.assertNotIn("jellyseerr", script)
        self.assertNotIn("bazarr", script)


class ServarrRendered(RenderedChart):
    CHART = "radarr"

    def radarr(self, extra=None):
        return self.deployments(self.docs(extra))["gateway"]

    def test_alone_it_keeps_the_names_an_installed_radarr_already_has(self):
        docs = self.docs()
        self.assertIn("gateway", self.deployments(docs))
        claims = [
            d["metadata"]["name"] for d in docs if d["kind"] == "PersistentVolumeClaim"
        ]
        self.assertEqual(
            claims,
            ["release-data", check_charts.files_claim(check_charts.FILES_FOLDER)],
        )
        claim = next(
            d
            for d in docs
            if d["kind"] == "PersistentVolumeClaim"
            and d["metadata"]["name"] == "release-data"
        )
        self.assertEqual(check_charts.tree_path(claim), check_charts.RELEASE)

    def test_without_a_key_of_its_own_radarr_keeps_the_one_it_made(self):
        spec = self.radarr({"config.api_key": ""})
        self.assertNotIn("RADARR__AUTH__APIKEY", self.env(spec, "radarr"))
        self.assertEqual(self.env(spec, "configure")["API_KEY"], "")

    def test_a_chosen_key_is_the_one_radarr_runs_with_and_reports(self):
        spec = self.radarr()
        key = self.env(spec, "radarr")["RADARR__AUTH__APIKEY"]
        self.assertEqual(self.env(spec, "configure")["API_KEY"], key)
        self.assertEqual(self.env(spec, "configure")["API_KEY_OUTPUT"], "api_key")

    def test_a_linked_download_client_reaches_the_wiring(self):
        spec = self.radarr(check_charts.VARIANTS["radarr"][0])
        env = self.env(spec, "configure")
        self.assertEqual(env["DC_URL"], check_charts.QBITTORRENT_LINK["internal_url"])
        self.assertEqual(env["CATEGORY_FIELD"], "movieCategory")

    def test_radarr_keeps_films_in_files_where_the_person_said(self):
        self.assertEqual(
            self.env(self.radarr(), "configure")["ROOT_FOLDER"], "/files/movies"
        )
        spec = self.radarr({"config.movies_path": "media/Films"})
        self.assertEqual(
            self.env(spec, "configure")["ROOT_FOLDER"], "/files/media/Films"
        )
        dirs = next(c for c in spec["initContainers"] if c["name"] == "files-dirs")
        self.assertIn('mkdir -p "/files/media/Films"', dirs["command"][2])
        self.assertIn('chown 1000:1000 "/files/media/Films"', dirs["command"][2])
        self.assertIn('chown 1000:1000 "/files"', dirs["command"][2])

    def test_a_path_that_climbs_out_of_files_is_refused_by_the_chart(self):
        for bad in ("../etc", "/etc", "a/../../etc"):
            self.assertIsNone(
                check_charts.render(
                    self.chart,
                    self.library,
                    self.tmp.name,
                    {"config.movies_path": bad},
                )[0],
                bad,
            )


class QbittorrentLogin(RenderedChart):
    CHART = "qbittorrent"

    def inits(self, extra=None):
        spec = self.deployments(self.docs(extra))["gateway"]
        return {c["name"]: c for c in spec["initContainers"]}

    def test_an_install_from_before_keeps_qbittorrents_temporary_password(self):
        self.assertNotIn("login", self.inits())

    def test_a_chosen_password_is_written_before_qbittorrent_starts(self):
        login = self.inits(check_charts.VARIANTS["qbittorrent"][0])["login"]
        env = {e["name"]: e.get("value") for e in login["env"]}
        self.assertEqual(env["PASSWORD"], "PlaceholderPw2026")
        self.assertEqual(env["SAVE_PATH"], "/files/downloads")
        self.assertEqual(env["PASSWORD_OUTPUT"], "password")

    def test_without_a_password_nothing_is_shown_for_it(self):
        off = self.inits()["login-off"]
        self.assertIn("YOLAB_OUTPUT username NONE", off["command"][2])
        self.assertIn("YOLAB_OUTPUT password NONE", off["command"][2])
        chosen = self.inits(check_charts.VARIANTS["qbittorrent"][0])
        self.assertNotIn("login-off", chosen)


class NavidromeFiles(RenderedChart):
    CHART = "navidrome"

    def test_navidrome_plays_the_music_in_files_where_the_person_said(self):
        spec = self.deployments(self.docs())["gateway"]
        self.assertEqual(self.env(spec, "navidrome")["ND_MUSICFOLDER"], "/files/music")
        spec = self.deployments(self.docs(check_charts.VARIANTS["navidrome"][0]))[
            "gateway"
        ]
        self.assertEqual(
            self.env(spec, "navidrome")["ND_MUSICFOLDER"], "/files/Music/My Library"
        )


class FileBrowserOnAFolder(RenderedChart):
    CHART = "filebrowser"

    def browsed(self, docs):
        spec = self.deployments(docs)["filebrowser"]
        browser = next(c for c in spec["containers"] if c["name"] == "filebrowser")
        mount = next(m for m in browser["volumeMounts"] if m["name"] == "files")
        claim = next(
            v["persistentVolumeClaim"]["claimName"]
            for v in spec["volumes"]
            if v["name"] == "files"
        )
        config = check_charts.configmap_data(docs, "-config", "config.yaml")
        sources = yaml.safe_load(config)["server"]["sources"]
        return mount, claim, sources

    def test_alone_it_browses_its_own_files_folder(self):
        docs = self.docs()
        mount, claim, sources = self.browsed(docs)
        self.assertEqual(mount["mountPath"], "/files")
        self.assertNotIn("subPath", mount)
        self.assertEqual([s["path"] for s in sources], ["/files"])
        files = [d for d in docs if check_charts.is_files_doc(d)]
        self.assertEqual({check_charts.tree_path(d) for d in files}, {"release.files"})
        self.assertEqual(claim, check_charts.files_claim("release.files"))

    def test_pointed_at_another_service_s_folder_it_browses_that_folder(self):
        docs = self.docs(
            {
                "config.files_folder_existing": "true",
                "config.files_folder": "movies-tv.files",
            }
        )
        _, claim, _ = self.browsed(docs)
        files = [d for d in docs if check_charts.is_files_doc(d)]
        self.assertEqual(
            {check_charts.tree_path(d) for d in files}, {"movies-tv.files"}
        )
        self.assertEqual(claim, check_charts.files_claim("movies-tv.files"))

    def test_pointed_at_the_root_it_browses_the_whole_tree(self):
        docs = self.docs(
            {"config.files_folder_existing": "true", "config.files_folder": "/"}
        )
        _, claim, _ = self.browsed(docs)
        files = [d for d in docs if check_charts.is_files_doc(d)]
        self.assertEqual({check_charts.tree_path(d) for d in files}, {""})
        self.assertEqual(claim, check_charts.files_claim("/"))
        volume = next(d for d in files if d["kind"] == "PersistentVolume")
        self.assertEqual(
            volume["spec"]["csi"]["volumeAttributes"]["rootPath"],
            check_charts.TREE_ROOT,
        )
        found = []
        check_charts.check_tree_volumes(
            self.CHART, docs, lambda _app, why: found.append(why)
        )
        self.assertEqual(found, [])

    def test_the_root_is_never_the_folder_a_service_keeps_its_own_data_in(self):
        docs = self.docs(
            {"config.files_folder_existing": "true", "config.files_folder": "/"}
        )
        homes = {
            check_charts.tree_path(d)
            for d in docs
            if d["kind"] == "PersistentVolumeClaim" and not check_charts.is_files_doc(d)
        }
        self.assertEqual(homes, {check_charts.RELEASE})

    def test_a_folder_left_in_the_form_with_the_switch_off_is_not_used(self):
        docs = self.docs(
            {
                "config.files_folder_existing": "false",
                "config.files_folder": "movies-tv.files",
            }
        )
        _, claim, _ = self.browsed(docs)
        self.assertEqual(claim, check_charts.files_claim("release.files"))

    def test_it_never_re_owns_everything_in_the_folder_it_is_pointed_at(self):
        spec = self.deployments(
            self.docs(
                {
                    "config.files_folder_existing": "true",
                    "config.files_folder": "immich",
                }
            )
        )["filebrowser"]
        for c in spec.get("initContainers") or []:
            script = " ".join(c.get("command") or [])
            if "/files" in script:
                self.assertNotIn("chown -R", script, c["name"])
        self.assertNotIn("fsGroup", spec.get("securityContext") or {})


class PackReadyToUse(RenderedChart):
    CHART = "movies-tv"

    def pods(self, extra=None):
        return self.deployments(self.docs(extra))

    def test_jellyfin_is_set_up_with_a_library_for_each_place_in_files(self):
        setup = self.env(self.pods()["jellyfin-gateway"], "setup")
        self.assertEqual(setup["ROOT"], "/files")
        self.assertEqual(
            json.loads(setup["LIBRARIES"]),
            [
                {"name": "Films", "kind": "movies", "path": "movies"},
                {"name": "Shows", "kind": "tvshows", "path": "shows"},
            ],
        )
        self.assertEqual(len(setup["PASSWORD"]), 32)

    def test_seerr_signs_in_with_the_admin_jellyfin_was_set_up_with(self):
        pods = self.pods()
        jellyfin = self.env(pods["jellyfin-gateway"], "setup")
        seerr = self.env(pods["jellyseerr-gateway"], "configure")
        self.assertEqual(seerr["JELLYFIN_URL"], "http://jellyfin:8096")
        self.assertEqual(seerr["JELLYFIN_USERNAME"], "admin")
        self.assertEqual(seerr["JELLYFIN_PASSWORD"], jellyfin["PASSWORD"])

    def test_seerr_and_bazarr_get_the_keys_the_arrs_run_with(self):
        pods = self.pods()
        radarr = self.env(pods["radarr-gateway"], "radarr")["RADARR__AUTH__APIKEY"]
        sonarr = self.env(pods["sonarr-gateway"], "sonarr")["SONARR__AUTH__APIKEY"]
        for pod in ("jellyseerr-gateway", "bazarr-gateway"):
            env = self.env(pods[pod], "configure")
            self.assertEqual(env["RADARR_KEY"], radarr, pod)
            self.assertEqual(env["SONARR_KEY"], sonarr, pod)

    def test_bazarr_looks_for_the_chosen_languages(self):
        env = self.env(
            self.pods({"config.subtitle_languages[0]": "fr"})["bazarr-gateway"],
            "configure",
        )
        self.assertEqual(env["LANGUAGES"], "fr")

    def test_without_requests_and_subtitles_neither_runs(self):
        pods = self.pods(check_charts.VARIANTS["movies-tv"][2])
        self.assertNotIn("jellyseerr-gateway", pods)
        self.assertNotIn("bazarr-gateway", pods)


class JellyfinSetup(RenderedChart):
    CHART = "jellyfin"

    def test_an_install_from_before_is_left_to_its_own_setup(self):
        spec = self.deployments(self.docs({"config.admin_password": ""}))["gateway"]
        self.assertNotIn("setup", {c["name"] for c in spec["containers"]})
        off = next(c for c in spec["initContainers"] if c["name"] == "setup-off")
        for key in ("username", "password", "api_key"):
            self.assertIn(f"YOLAB_OUTPUT {key} NONE", off["command"][2])

    def test_a_chosen_password_prints_no_none_for_its_sign_in(self):
        spec = self.deployments(self.docs())["gateway"]
        self.assertNotIn("setup-off", {c["name"] for c in spec["initContainers"]})

    def test_a_chosen_password_sets_jellyfin_up_and_reports_how_to_sign_in(self):
        spec = self.deployments(self.docs())["gateway"]
        env = self.env(spec, "setup")
        self.assertEqual(env["PASSWORD"], "PlaceholderPw2026")
        self.assertEqual(env["USERNAME_OUTPUT"], "username")
        self.assertEqual(env["ROOT"], "/files")

    def test_alone_it_offers_films_and_shows_in_files_and_makes_their_places(self):
        spec = self.deployments(self.docs())["gateway"]
        self.assertEqual(
            [lib["path"] for lib in json.loads(self.env(spec, "setup")["LIBRARIES"])],
            ["movies", "shows"],
        )
        dirs = next(c for c in spec["initContainers"] if c["name"] == "files-dirs")
        self.assertIn('mkdir -p "/files/movies"', dirs["command"][2])
        self.assertIn('mkdir -p "/files/shows"', dirs["command"][2])


def caddyfile_of(docs):
    return next(
        d["data"]["Caddyfile"]
        for d in docs
        if d["kind"] == "ConfigMap" and "Caddyfile" in (d.get("data") or {})
    )


def container_of(spec, name):
    return next(
        c
        for c in (spec.get("initContainers") or []) + spec["containers"]
        if c["name"] == name
    )


class RadicaleAccounts(RenderedChart):
    CHART = "radicale"

    def radicale(self, extra=None):
        return self.deployments(self.docs(extra))["radicale"]

    def config_script(self):
        return container_of(self.radicale(), "radicale-config")["command"][2]

    def test_every_account_is_kept_as_a_bcrypt_hash_and_sees_only_its_own_data(self):
        script = self.config_script()
        self.assertIn("htpasswd_encryption = bcrypt", script)
        self.assertIn("type = owner_only", script)
        self.assertIn("bcrypt.hashpw", script)

    def test_it_answers_on_both_address_families_the_cluster_uses(self):
        self.assertIn("hosts = 0.0.0.0:5232, [::]:5232", self.config_script())

    def test_the_accounts_reach_the_hasher_one_per_line(self):
        secret = next(
            d
            for d in self.docs()
            if d["kind"] == "Secret" and d["metadata"]["name"] == "release-accounts"
        )
        self.assertEqual(secret["stringData"]["logins"], "alice\tPlaceholderPw2026\n")

    def test_changing_the_accounts_restarts_radicale_so_they_take_effect(self):
        before = self.deployments_annotations()
        after = self.deployments_annotations(
            {"config.accounts[0].password": "OtherPw2026x"}
        )
        self.assertNotEqual(before, after)

    def deployments_annotations(self, extra=None):
        doc = next(
            d
            for d in self.docs(extra)
            if d["kind"] == "Deployment" and d["metadata"]["name"] == "radicale"
        )
        return doc["spec"]["template"]["metadata"]["annotations"]["yolab.io/accounts"]

    def test_without_an_account_the_chart_refuses_to_render(self):
        text, err = check_charts.render(
            self.chart, self.library, self.tmp.name, {"config.accounts": "{}"}
        )
        self.assertIsNone(text)
        self.assertIn("accounts", err)


class GristLogin(RenderedChart):
    CHART = "grist"

    def gateway(self, extra=None):
        return self.deployments(self.docs(extra))["gateway"]

    def test_grist_trusts_only_the_identity_caddy_derives_from_the_login(self):
        caddy = caddyfile_of(self.docs())
        auth = caddy.index("forward_auth localhost:9091")
        header = caddy.index(
            "header_up X-Grist-Email {http.request.header.Remote-User}@yolab.local"
        )
        self.assertLess(auth, header)
        env = self.env(self.gateway(), "grist")
        self.assertEqual(env["GRIST_FORWARD_AUTH_HEADER"], "X-Grist-Email")
        self.assertEqual(env["GRIST_HOST"], "127.0.0.1")

    def test_the_first_login_owns_the_site(self):
        env = self.env(self.gateway(), "grist")
        self.assertEqual(env["GRIST_DEFAULT_EMAIL"], "admin@yolab.local")

    def test_nobody_reaches_grist_or_its_widgets_without_signing_in(self):
        caddy = caddyfile_of(self.docs())
        auth = caddy.index("forward_auth localhost:9091")
        self.assertLess(auth, caddy.index("reverse_proxy localhost:8485"))
        self.assertLess(auth, caddy.index("reverse_proxy localhost:8484"))

    def test_grist_lists_the_widgets_from_the_server_next_to_it(self):
        env = self.env(self.gateway(), "grist")
        self.assertEqual(
            env["GRIST_WIDGET_LIST_URL"], "http://localhost:8485/manifest.json"
        )

    def test_the_homelab_widgets_ship_with_the_chart(self):
        widgets = next(
            d
            for d in self.docs()
            if d["kind"] == "ConfigMap" and d["metadata"]["name"] == "release-widgets"
        )
        self.assertEqual(
            sorted(widgets["data"]), ["map.html", "rank.html", "widgets.css"]
        )

    def run_widgets(self, extra_pages):
        script = container_of(self.gateway(), "widgets")["command"][2]
        shipped = Path(check_charts.HERE, "grist", "files", "widgets")
        with tempfile.TemporaryDirectory() as root:
            for sub in ("yolab", "shipped", "widgets"):
                Path(root, sub).mkdir()
            Path(root, "yolab", "env").write_text(
                "export YOLAB_FQDN=grist.example.test\n"
            )
            for page in shipped.iterdir():
                Path(root, "shipped", page.name).write_bytes(page.read_bytes())
            for name, body in extra_pages.items():
                Path(root, "widgets", name).write_text(body)
            local = (
                script.replace("/yolab", f"{root}/yolab")
                .replace("/shipped", f"{root}/shipped")
                .replace("/widgets", f"{root}/widgets")
                .replace(f"https://%s{root}/widgets/%s", "https://%s/widgets/%s")
            )
            out = subprocess.run(
                ["sh", "-c", local], capture_output=True, text=True, check=True
            )
            manifest = json.loads(Path(root, "widgets", "manifest.json").read_text())
        return manifest, out.stdout

    def test_each_widget_page_is_listed_with_the_access_it_asks_for(self):
        manifest, _ = self.run_widgets({})
        listed = {w["name"]: (w["url"], w["accessLevel"]) for w in manifest}
        self.assertEqual(
            listed,
            {
                "Binary ranker": (
                    "https://grist.example.test/widgets/rank.html",
                    "full",
                ),
                "2D map": ("https://grist.example.test/widgets/map.html", "read table"),
            },
        )

    def test_a_widget_someone_adds_to_the_folder_is_listed_too(self):
        page = '<title>Mine "quoted"</title><script>grist.ready({requiredAccess: "none"});</script>'
        manifest, _ = self.run_widgets({"mine.html": page})
        mine = next(w for w in manifest if w["widgetId"] == "yolab-mine")
        self.assertEqual(mine["name"], 'Mine "quoted"')
        self.assertEqual(mine["accessLevel"], "none")


class I2pRouter(RenderedChart):
    CHART = "i2p"

    def router(self, extra=None):
        return container_of(self.deployments(self.docs(extra))["gateway"], "i2p")

    def test_the_console_is_only_on_loopback_and_behind_the_login(self):
        env = self.env(self.deployments(self.docs())["gateway"], "i2p")
        self.assertEqual(env["IP_ADDR"], "127.0.0.1")
        caddy = caddyfile_of(self.docs())
        self.assertLess(
            caddy.index("forward_auth localhost:9091"),
            caddy.index("reverse_proxy localhost:7657"),
        )

    def test_the_router_shares_the_tunnels_network_so_peers_reach_it(self):
        names = [
            c["name"] for c in self.deployments(self.docs())["gateway"]["containers"]
        ]
        self.assertIn("wireguard", names)
        self.assertIn("i2p", names)

    def test_udp_answers_from_the_tunnel_address_peers_wrote_to(self):
        script = self.router()["command"][2]
        self.assertIn('set_key i2np.udp.bindInterface "$YOLAB_IPV6"', script)
        self.assertLess(
            script.index("until ip -6"), script.index("exec /entrypoint/entrypoint.sh")
        )

    def test_the_transport_port_is_not_the_images_default_and_stays_put(self):
        first = self.env(self.deployments(self.docs())["gateway"], "i2p")["EXT_PORT"]
        again = self.env(self.deployments(self.docs())["gateway"], "i2p")["EXT_PORT"]
        self.assertEqual(first, again)
        self.assertNotEqual(first, "12345")
        self.assertTrue(20000 <= int(first) < 50000)

    def test_a_setting_is_replaced_not_repeated_in_router_config(self):
        script = self.router()["command"][2]
        start = script.index("set_key() {")
        end = script.index("}\n", start) + 2
        with tempfile.TemporaryDirectory() as root:
            conf = Path(root, "router.config")
            conf.write_text("i2np.udp.port=1\nrouter.sharePercentage=10\n")
            subprocess.run(
                [
                    "sh",
                    "-c",
                    (
                        f'CONF="{conf}"\n{script[start:end]}\n'
                        "set_key router.sharePercentage 80\n"
                        "set_key router.sharePercentage 90\n"
                    ),
                ],
                check=True,
            )
            self.assertEqual(
                conf.read_text(), "i2np.udp.port=1\nrouter.sharePercentage=90\n"
            )

    def test_the_bandwidth_chosen_is_given_to_the_router(self):
        env = self.env(
            self.deployments(
                self.docs({"config.bandwidth_kbps": 2048, "config.share_percent": 50})
            )["gateway"],
            "i2p",
        )
        self.assertEqual(env["BANDWIDTH_KBPS"], "2048")
        self.assertEqual(env["SHARE_PERCENT"], "50")


class MatrixServer(RenderedChart):
    CHART = "matrix"

    def gateway(self, extra=None):
        return self.deployments(self.docs(extra))["gateway"]

    def script(self, name, extra=None):
        return container_of(self.gateway(extra), name)["command"][2]

    def test_other_servers_and_clients_find_this_server_on_its_own_address(self):
        caddy = caddyfile_of(self.docs())
        self.assertIn('respond `{"m.server":"{$YOLAB_HOST}:443"}`', caddy)
        self.assertIn('"livekit_service_url":"https://{$RTC_FQDN}/livekit/jwt"', caddy)
        self.assertIn("handle /_matrix/* {\n    reverse_proxy localhost:8008", caddy)

    def test_calls_get_their_own_two_addresses(self):
        wg = container_of(self.gateway(), "wg-register")
        aliases = next(e["value"] for e in wg["env"] if e["name"] == "ALIASES")
        self.assertEqual(aliases, "CALL_FQDN=release-call RTC_FQDN=release-rtc")

    def test_an_invite_code_is_required_to_sign_up_by_default(self):
        script = self.script("synapse")
        self.assertIn("enable_registration: true", script)
        self.assertIn("registration_requires_token: true", script)
        self.assertIn("enable_registration_without_verification: false", script)

    def test_closed_means_nobody_can_sign_up_and_element_hides_it(self):
        extra = {"config.registration": "closed"}
        self.assertIn("enable_registration: false", self.script("synapse", extra))
        self.assertIn(
            '"UIFeature.registration": false', self.script("matrix-secrets", extra)
        )

    def test_open_lets_anyone_sign_up_without_a_code(self):
        script = self.script("synapse", {"config.registration": "open"})
        self.assertIn("registration_requires_token: false", script)
        self.assertIn("enable_registration_without_verification: true", script)

    def test_without_an_invite_code_its_output_is_hidden(self):
        outputs = container_of(
            self.gateway({"config.registration": "closed"}), "outputs"
        )
        env = {e["name"]: e for e in outputs["env"]}
        self.assertEqual(env["OUTPUT_REGISTRATION_TOKEN"]["value"], "")
        admin = container_of(
            self.gateway({"config.registration": "closed"}), "matrix-admin"
        )
        self.assertNotIn("REGISTRATION_TOKEN", {e["name"] for e in admin["env"]})

    def test_the_invite_code_is_printed_from_its_secret(self):
        outputs = container_of(self.gateway(), "outputs")
        env = {e["name"]: e for e in outputs["env"]}
        self.assertEqual(
            env["OUTPUT_REGISTRATION_TOKEN"]["valueFrom"]["secretKeyRef"],
            {"name": "release-matrix", "key": "registration-token"},
        )

    def test_a_server_name_that_changed_stops_synapse_instead_of_forking_identity(self):
        script = self.script("synapse")
        self.assertLess(
            script.index('if [ "$SERVER_NAME" != "$YOLAB_HOST" ]'),
            script.index("exec python -m synapse.app.homeserver"),
        )

    def test_media_offers_only_the_tunnel_address_to_callers(self):
        script = self.script("matrix-secrets")
        self.assertIn("use_external_ip: false", script)
        self.assertIn("includes:\n      - wg0", script)
        self.assertLess(
            self.script("livekit").index("until ip -6"),
            self.script("livekit").index("exec /livekit-server"),
        )

    def test_the_service_reaches_its_own_names_inside_the_pod(self):
        self.assertIn(">> /etc/hosts", self.script("matrix-secrets"))

    def test_the_admin_sign_up_mac_is_the_one_synapse_checks(self):
        import hashlib
        import hmac

        script = self.script("matrix-admin")
        self.assertIn(
            'hmac.new(secret, "\\x00".join((nonce, user, password, "admin")).encode(), hashlib.sha1)',
            script,
        )
        documented = hmac.new(b"s", digestmod=hashlib.sha1)
        for part in (b"n", b"u", b"p"):
            documented.update(part)
            documented.update(b"\x00")
        documented.update(b"admin")
        parts = ("n", "u", "p", "admin")
        ours = hmac.new(b"s", "\x00".join(parts).encode(), hashlib.sha1)
        self.assertEqual(ours.hexdigest(), documented.hexdigest())

    def test_the_token_service_gets_a_shell_from_busybox_because_its_image_has_none(
        self,
    ):
        jwt = container_of(self.gateway(), "livekit-jwt")
        self.assertEqual(jwt["command"][:3], ["/rtc/busybox", "sh", "-c"])
        self.assertIn("cp /bin/busybox /rtc/busybox", self.script("matrix-secrets"))


AUTHELIA_ONE_TIME_CODE_EMAIL = """Date: 2026-11-08 12:00:00.000000000 +0000 UTC
Recipient: {Alice Example alice@example.com}
Subject: Confirm your identity
A ONE-TIME CODE HAS BEEN GENERATED TO COMPLETE A REQUESTED ACTION

Hi Alice Example,

This notification has been sent to you in order to verify your identity to change security details for your account at auth.example.test.

Do not share this notification or the content of this notification with anyone.

The following one-time code should only be used in the prompt displayed in your browser.

----------------------------------------

K7QF2MZP

----------------------------------------

If you did not initiate the process your credentials may have been compromised and you should:

 1. Revoke this code using the provided links below
 2. Reset your password or other login credentials
 3. Contact an Administrator

To revoke the code, click the link below:

https://auth.example.test/revoke/one-time-code?id=abc

----------------------------------------

This notification was intended for Alice Example. This one-time code was generated due to an action from 2001:db8::1. If you do not believe that your actions could have triggered this event or if you are concerned about your account's safety, please follow the explicit directions in this notification.

Powered by Authelia https://www.authelia.com


"""

AUTHELIA_EVENT_EMAIL = """Date: 2026-11-08 12:00:00.000000000 +0000 UTC
Recipient: {Alice Example alice@example.com}
Subject: Second Factor Method Added
Hi Alice Example,

This notification has been sent to you in order to notify you that a new Second Factor Method was added to your account.

----------------------------------------

Powered by Authelia https://www.authelia.com
"""


class AutheliaService(RenderedChart):
    CHART = "authelia"

    def gateway(self, extra=None):
        return self.deployments(self.docs(extra))["gateway"]

    def access(self, extra=None):
        script = container_of(self.gateway(extra), "authelia-config")["command"][2]
        config = script.split("<<EOF\n", 1)[1].split("\nEOF\n", 1)[0]
        return yaml.safe_load(config)

    def rules_for(self, config, domain):
        return [r for r in config["access_control"]["rules"] if r["domain"] == domain]

    def test_nothing_is_reachable_unless_a_rule_says_so(self):
        self.assertEqual(self.access()["access_control"]["default_policy"], "deny")

    def test_the_users_page_is_only_for_directory_admins(self):
        rules = self.rules_for(self.access(), "${USERS_FQDN}")
        self.assertEqual(rules[0]["subject"], "group:lldap_admin")
        self.assertEqual(rules[1]["policy"], "deny")

    def test_every_service_under_the_shared_domain_needs_the_chosen_sign_in(self):
        for extra, want in (
            (None, "one_factor"),
            ({"config.policy": "two_factor"}, "two_factor"),
        ):
            rules = self.access(extra)["access_control"]["rules"]
            self.assertEqual(
                rules[-1], {"domain": "*.${COOKIE_DOMAIN}", "policy": want}
            )

    def test_a_rule_with_groups_shuts_everyone_else_out_of_that_address(self):
        config = self.access(check_charts.VARIANTS["authelia"][0])
        grist = self.rules_for(config, "grist.${COOKIE_DOMAIN}")
        self.assertEqual(grist[0]["subject"], ["group:family"])
        self.assertEqual(grist[0]["policy"], "two_factor")
        self.assertEqual(
            grist[1], {"domain": "grist.${COOKIE_DOMAIN}", "policy": "deny"}
        )
        rules = config["access_control"]["rules"]
        self.assertLess(rules.index(grist[1]), len(rules) - 1)

    def test_a_rule_without_groups_only_changes_what_sign_in_needs(self):
        config = self.access(check_charts.VARIANTS["authelia"][0])
        self.assertEqual(
            self.rules_for(config, "media-sonarr.${COOKIE_DOMAIN}"),
            [{"domain": "media-sonarr.${COOKIE_DOMAIN}", "policy": "deny"}],
        )

    def test_a_full_address_in_a_rule_is_used_as_it_is(self):
        config = self.access(
            {
                "config.rules[0].subdomain": "wiki.example.org",
                "config.rules[0].policy": "one_factor",
            }
        )
        self.assertTrue(self.rules_for(config, "wiki.example.org"))

    def test_a_group_name_that_could_break_the_config_is_refused(self):
        _, err = check_charts.render(
            self.chart,
            self.library,
            self.tmp.name,
            {
                "config.rules[0].subdomain": "grist",
                "config.rules[0].policy": "one_factor",
                "config.rules[0].groups[0]": "a'b",
            },
        )
        self.assertIsNotNone(err)

    def test_people_sign_in_against_the_directory_next_to_it(self):
        ldap = self.access()["authentication_backend"]["ldap"]
        self.assertEqual(ldap["implementation"], "lldap")
        self.assertEqual(ldap["address"], "ldap://127.0.0.1:3890")
        env = self.env(self.gateway(), "authelia")
        self.assertEqual(
            env["AUTHELIA_AUTHENTICATION_BACKEND_LDAP_PASSWORD_FILE"],
            "/secret/admin_password",
        )
        self.assertEqual(
            self.env(self.gateway(), "lldap")["LLDAP_LDAP_USER_PASS_FILE"],
            "/secret/admin_password",
        )

    def test_the_form_password_is_the_directory_admin_password_on_every_start(self):
        env = self.env(self.gateway(), "lldap")
        self.assertEqual(env["LLDAP_FORCE_LDAP_USER_PASS_RESET"], "always")

    def test_changing_the_admin_password_restarts_the_pod(self):
        def stamp(extra):
            docs = self.docs(extra)
            gateway = next(
                d
                for d in docs
                if d["kind"] == "Deployment" and d["metadata"]["name"] == "gateway"
            )
            return gateway["spec"]["template"]["metadata"]["annotations"][
                "yolab.io/access"
            ]

        self.assertNotEqual(
            stamp(None), stamp({"config.admin_password": "AnotherPlaceholder2026"})
        )

    def test_the_directory_is_only_reachable_inside_the_pod(self):
        env = self.env(self.gateway(), "lldap")
        self.assertEqual(env["LLDAP_LDAP_HOST"], "127.0.0.1")
        self.assertEqual(env["LLDAP_HTTP_HOST"], "127.0.0.1")

    def test_other_services_reach_it_at_the_address_it_prints(self):
        docs = self.docs()
        service = next(
            d
            for d in docs
            if d["kind"] == "Service" and d["metadata"]["name"] == "authelia"
        )
        self.assertEqual(service["spec"]["selector"], {"app": "gateway"})
        self.assertEqual(service["spec"]["ports"][0]["port"], 9091)
        script = container_of(self.gateway(), "codes")["command"][2]
        self.assertIn(
            f"YOLAB_OUTPUT internal_url http://authelia.{check_charts.NAMESPACE}.svc.cluster.local:9091",
            script,
        )

    def test_its_cookie_is_not_the_one_a_services_own_login_sets(self):
        self.assertEqual(self.access()["session"]["name"], "yolab_auth")

    def run_code_reader(self, notification):
        script = container_of(self.gateway(), "codes")["command"][2]
        line = next(l for l in script.splitlines() if "code=$(awk" in l).strip()
        program = line.split("code=$(", 1)[1].rsplit(")", 1)[0]
        with tempfile.TemporaryDirectory() as root:
            Path(root, "notification.txt").write_text(notification)
            run = subprocess.run(
                [
                    "sh",
                    "-c",
                    program.replace(
                        "/data/notification.txt", f"{root}/notification.txt"
                    ),
                ],
                capture_output=True,
                text=True,
                check=True,
            )
        return run.stdout.strip()

    def test_the_confirmation_code_is_the_one_in_authelias_notification(self):
        self.assertEqual(
            self.run_code_reader(AUTHELIA_ONE_TIME_CODE_EMAIL),
            "K7QF2MZP for Alice Example",
        )

    def test_a_notification_without_a_code_shows_no_code(self):
        self.assertEqual(self.run_code_reader(AUTHELIA_EVENT_EMAIL), "")

    def test_as_a_dependency_it_renders_nothing_of_its_own(self):
        self.assertEqual(self.docs({"embedded": "true"}), [])


class LoginChoice:
    CHART = None
    BASE = {}

    def gateway(self, extra=None):
        return self.deployments(self.docs({**self.BASE, **(extra or {})}))["gateway"]

    def existing(self, url="http://authelia.yolab-authelia.svc.cluster.local:9091"):
        return {
            **check_charts.EXISTING_AUTHELIA,
            "config.auth_url": url,
            **self.EXTRA_EXISTING,
        }

    EXTRA_EXISTING = {}

    def test_its_own_login_runs_authelia_next_to_it(self):
        gateway = self.gateway()
        authelia = container_of(gateway, "authelia")
        self.assertTrue(
            authelia["image"].startswith("docker.io/authelia/authelia:4.39.28@sha256:")
        )
        container_of(gateway, "authelia-config")

    def test_an_existing_authelia_is_reached_through_a_bridge_on_the_same_port(self):
        gateway = self.gateway(self.existing())
        bridge = container_of(gateway, "authelia")
        self.assertIn("caddy", bridge["image"])
        env = {e["name"]: e.get("value") for e in bridge["env"]}
        self.assertEqual(
            env["AUTH_UPSTREAM"],
            "http://authelia.yolab-authelia.svc.cluster.local:9091",
        )
        self.assertEqual(env["AUTH_PATH"], "")
        self.assertIn("http://:9091", bridge["command"][2])
        names = [c["name"] for c in gateway.get("initContainers") or []]
        self.assertNotIn("authelia-config", names)

    def test_an_existing_authelia_needs_no_users_here(self):
        docs = self.docs({**self.BASE, **self.existing()})
        self.assertFalse(
            [
                d
                for d in docs
                if d["kind"] == "Secret" and d["metadata"]["name"].endswith("-auth")
            ]
        )

    def test_the_caddyfile_is_the_same_whichever_authelia_signs_people_in(self):
        own = caddyfile_of(self.docs(self.BASE))
        existing = caddyfile_of(self.docs({**self.BASE, **self.existing()}))
        self.assertEqual(own, existing)

    def test_an_existing_authelia_without_an_address_is_refused(self):
        _, err = check_charts.render(
            self.chart,
            self.library,
            self.tmp.name,
            {**self.BASE, "config.auth_mode": "existing"},
        )
        self.assertIsNotNone(err)
        self.assertIn("auth_url", err)


class GristLoginChoice(LoginChoice, RenderedChart):
    CHART = "grist"
    EXTRA_EXISTING = {"config.owner": "alice"}

    def test_with_an_existing_authelia_the_named_owner_owns_the_site(self):
        env = self.env(self.gateway(self.existing()), "grist")
        self.assertEqual(env["GRIST_DEFAULT_EMAIL"], "alice@yolab.local")

    def test_with_an_existing_authelia_an_owner_is_required(self):
        _, err = check_charts.render(
            self.chart,
            self.library,
            self.tmp.name,
            dict(check_charts.EXISTING_AUTHELIA),
        )
        self.assertIsNotNone(err)
        self.assertIn("owner", err)

    def bridge(self, url):
        return container_of(self.gateway(self.existing(url)), "authelia")

    def test_a_path_in_the_address_is_put_in_front_of_every_request(self):
        bridge = self.bridge("https://sso.example.com/authelia/")
        env = {e["name"]: e.get("value") for e in bridge["env"]}
        self.assertEqual(env["AUTH_UPSTREAM"], "https://sso.example.com")
        self.assertEqual(env["AUTH_PATH"], "/authelia")
        self.assertIn("rewrite * ${AUTH_PATH}{uri}", bridge["command"][2])

    def test_an_https_authelia_is_asked_for_by_its_own_name(self):
        self.assertIn(
            "header_up Host {upstream_hostport}",
            self.bridge("https://sso.example.com")["command"][2],
        )
        self.assertNotIn(
            "header_up Host",
            self.bridge("http://authelia.yolab-authelia.svc.cluster.local:9091")[
                "command"
            ][2],
        )

    def test_an_address_with_a_query_is_refused(self):
        _, err = check_charts.render(
            self.chart,
            self.library,
            self.tmp.name,
            self.existing("http://a.example/?x=1"),
        )
        self.assertIsNotNone(err)

    def test_the_bridge_keeps_the_forwarded_headers_only_from_inside_the_pod(self):
        script = self.bridge("http://a.example")["command"][2]
        self.assertIn("trusted_proxies static 127.0.0.1/32 ::1/128", script)
        self.assertIn("bind 127.0.0.1 ::1", script)

    def test_signing_out_clears_the_cookie_the_authelia_service_sets(self):
        bridge = self.bridge("http://a.example")
        env = {e["name"]: e.get("value") for e in bridge["env"]}
        script = bridge["command"][2]
        self.assertIn('COOKIE_DOMAIN="${YOLAB_FQDN#*.}"', script)
        self.assertIn(
            "${AUTH_COOKIE}=; Domain=${COOKIE_DOMAIN}; Path=/; Max-Age=0", script
        )
        self.assertEqual(env["AUTH_COOKIE"], "yolab_auth")

    def test_grist_signs_out_where_the_bridge_clears_the_cookie(self):
        env = self.env(self.gateway(self.existing()), "grist")
        self.assertEqual(env["GRIST_FORWARD_AUTH_LOGOUT_PATH"], "authelia/logout")
        self.assertIn(
            "handle /authelia/logout", self.bridge("http://a.example")["command"][2]
        )


class I2pLoginChoice(LoginChoice, RenderedChart):
    CHART = "i2p"


class MediaStackLoginChoice(LoginChoice, RenderedChart):
    CHART = "media-stack"


class SteamHeadlessLoginChoice(LoginChoice, RenderedChart):
    CHART = "steam-headless"


class ExcalidrawLoginChoice(LoginChoice, RenderedChart):
    CHART = "excalidraw"
    BASE = {"config.auth_enabled": "true"}

    def test_without_a_login_no_authelia_runs(self):
        gateway = self.deployments(self.docs())["gateway"]
        self.assertNotIn("authelia", [c["name"] for c in gateway["containers"]])


class LoginDependency(unittest.TestCase):
    def test_authelia_as_a_dependency_does_not_make_a_chart_a_pack(self):
        chart = Path(check_charts.HERE, "grist", "Chart.yaml").read_text()
        self.assertIn(
            "authelia", [d["name"] for d in check_charts.chart_dependencies(chart)]
        )
        self.assertEqual(check_charts.pack_members(chart), [])

    def test_every_chart_pins_the_authelia_of_this_tree(self):
        tree = check_charts.chart_field(
            Path(check_charts.HERE, "authelia", "Chart.yaml").read_text(), "version"
        )
        for chart_yaml in Path(check_charts.HERE).glob("*/Chart.yaml"):
            for dep in check_charts.chart_dependencies(chart_yaml.read_text()):
                if dep["name"] == "authelia":
                    self.assertEqual(str(dep["version"]), tree, chart_yaml.parent.name)

    def test_no_chart_uses_the_old_authelia_of_the_library(self):
        for template in Path(check_charts.HERE).glob("*/templates/*"):
            if template.parts[-3] == "yolab-common":
                continue
            self.assertNotIn("yolab-common.authelia", template.read_text(), template)


if __name__ == "__main__":
    unittest.main()
