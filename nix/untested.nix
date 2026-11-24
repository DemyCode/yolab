{
  local-api = {
    "src/main.rs" = "builds the real machine access and starts the server; only a booted NixOS VM runs it";
    "src/tools.rs" = "builds RealHost and the real clients from main; only a booted NixOS VM runs it";
    "src/host.rs" = "RealHost runs kubectl, ceph, systemctl and lsblk on the machine; FakeHost stands in for it everywhere else";
    "src/ceph_cli.rs" = "spawns the ceph CLI; only a booted NixOS VM has a cluster to answer it";
    "src/exec.rs" = "spawns processes on the machine; only a booted NixOS VM runs it";
    "src/k8s.rs" = "talks to the k3s API server; only a booted NixOS VM has one";
    "src/http.rs" = "the real outbound HTTP client; only a booted NixOS VM runs it";
    "src/runtime/watch.rs" = "holds open watches on the k3s API server; only a booted NixOS VM has one";
  };
  installer = {};
  desktop-client = {};
}
