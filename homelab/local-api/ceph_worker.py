import errno
import json
import os
import sys
import threading

import rados
from ceph_argparse import (
    find_cmd_target,
    json_command,
    parse_json_funcsigs,
    validate_command,
)

CONNECT_TIMEOUT = int(sys.argv[1])
TIMED_OUT = "Connection timed out. Please check the client's permission and connection."

replies = os.fdopen(os.dup(1), "w", buffering=1)
os.dup2(2, 1)
sys.stdout = sys.stderr
write_lock = threading.Lock()
sig_lock = threading.Lock()
sigdicts = {}


def send(message):
    line = json.dumps(message)
    with write_lock:
        replies.write(line + "\n")
        replies.flush()


def reply(rid, ret, out, outs):
    stderr = ""
    if ret < 0:
        name = errno.errorcode.get(-ret, "Unknown")
        stderr = "Error {0}: {1}".format(name, outs)
    send({"id": rid, "ret": ret, "stdout": out, "stderr": stderr})


def signatures(cluster, target, timeout, refresh):
    with sig_lock:
        if refresh or target not in sigdicts:
            ret, outbuf, outs = json_command(
                cluster,
                target=target,
                prefix="get_command_descriptions",
                timeout=timeout,
            )
            if ret:
                where = "{0}.{1}".format(*target)
                raise RuntimeError(
                    "problem getting command descriptions from {0}: {1}".format(
                        where, outs
                    )
                )
            sigdicts[target] = parse_json_funcsigs(outbuf.decode("utf-8"), "cli")
        return sigdicts[target]


def run(cluster, request):
    rid = request["id"]
    args = request["args"]
    timeout = request["timeout"]
    fmt = request.get("format", "")
    try:
        target = find_cmd_target(args)
        if target[1] == "*":
            reply(rid, -errno.EINVAL, "", "wildcard targets need the ceph CLI")
            return
        valid = validate_command(signatures(cluster, target, timeout, False), args)
        if not valid:
            valid = validate_command(signatures(cluster, target, timeout, True), args)
        if not valid:
            reply(rid, -errno.EINVAL, "", "invalid command")
            return
        if fmt:
            valid["format"] = fmt
        ret, outbuf, outs = json_command(
            cluster, target=target, argdict=valid, timeout=timeout
        )
        if ret == errno.ETIMEDOUT:
            ret = -ret
            outs = outs or TIMED_OUT
        out = outbuf.decode("utf-8", "replace")
        if fmt.startswith("json"):
            out = "\n" + out
        reply(rid, ret, out, outs)
    except Exception as e:
        reply(rid, -errno.EIO, "", "ceph worker: {0!r}".format(e))


def main():
    cluster = rados.Rados(
        name="client.admin",
        conffile=rados.Rados.DEFAULT_CONF_FILES,
        conf_defaults={"log_to_stderr": "true", "err_to_stderr": "true"},
    )
    cluster.connect(timeout=CONNECT_TIMEOUT)
    send({"ready": True})
    running = []
    for line in sys.stdin:
        t = threading.Thread(target=run, args=(cluster, json.loads(line)))
        t.start()
        running = [w for w in running if w.is_alive()] + [t]
    for t in running:
        t.join()
    cluster.shutdown()


main()
