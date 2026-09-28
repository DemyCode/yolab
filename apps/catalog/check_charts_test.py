import unittest

import check_charts

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


if __name__ == "__main__":
    unittest.main()
