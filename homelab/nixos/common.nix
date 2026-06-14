{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:
let
  s = import ../shared.nix { inherit pkgs lib inputs; };
  k3sCfg = s.nodeCfg.k3s;

  # The first node initialises the embedded-etcd cluster (--cluster-init).
  # Every other node joins as an equal server peer via serverAddr.
  # After joining, all nodes are identical: control plane + worker + UI.
  isFirstNode = k3sCfg.server_addr == "";

  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);

  # Python helpers for parsing velero config at runtime (credentials never touch Nix store).
  parseVeleroEnv = pkgs.writeText "parse-velero-env.py" ''
    import tomllib, sys, shlex
    with open(sys.argv[1], 'rb') as f:
        c = tomllib.load(f)
    v = c.get('velero', {})
    E = str()
    def q(s): return shlex.quote(str(s))
    print(f"S3_ENDPOINT={q(v.get('s3_endpoint', 'https://s3.amazonaws.com'))}")
    print(f"S3_BUCKET={q(v.get('s3_bucket', E))}")
    print(f"S3_REGION={q(v.get('s3_region', 'us-east-1'))}")
    print(f"S3_ACCESS_KEY={q(v.get('s3_access_key', E))}")
    print(f"S3_SECRET_KEY={q(v.get('s3_secret_key', E))}")
  '';

  # Outputs K3s config.yaml YAML stanza for etcd → S3 shipping.
  # Tries yolab-external API first (if [yolab_external] is configured),
  # falls back to the manual [velero] section.  Credentials never touch the Nix store.
  parseK3sEtcdS3Config = pkgs.writeText "parse-k3s-etcd-s3.py" ''
    import tomllib, sys, json, re
    from urllib import request as urlreq

    with open(sys.argv[1], 'rb') as f:
        c = tomllib.load(f)

    E = str()
    s3 = None
    ye = c.get('yolab_external', {})
    ye_url = ye.get('url', E).rstrip('/')
    ye_token = ye.get('account_token', E)
    if ye_url and ye_token:
        try:
            req = urlreq.Request(
                f"{ye_url}/storage/s3",
                headers={"Authorization": f"Bearer {ye_token}"}
            )
            with urlreq.urlopen(req, timeout=10) as r:
                d = json.loads(r.read())
                s3 = {
                    "endpoint": d["endpoint"], "bucket": d["bucket_name"],
                    "region": d["region"], "access_key": d["access_key_id"],
                    "secret_key": d["secret_access_key"],
                }
        except Exception:
            pass

    if s3 is None:
        v = c.get('velero', {})
        if not v.get('enabled', False):
            sys.exit(0)
        s3 = {
            "endpoint": v.get('s3_endpoint', 's3.amazonaws.com'),
            "bucket": v.get('s3_bucket', E),
            "region": v.get('s3_region', 'us-east-1'),
            "access_key": v.get('s3_access_key', E),
            "secret_key": v.get('s3_secret_key', E),
        }

    endpoint_host = re.sub(r'^https?://', E, s3["endpoint"])
    def y(s): return '"' + str(s).replace('"', '\\"') + '"'
    print('etcd-s3: "true"')
    print(f'etcd-s3-endpoint: {y(endpoint_host)}')
    print(f'etcd-s3-bucket: {y(s3["bucket"])}')
    print(f'etcd-s3-region: {y(s3["region"])}')
    print(f'etcd-s3-access-key: {y(s3["access_key"])}')
    print(f'etcd-s3-secret-key: {y(s3["secret_key"])}')
    print('etcd-s3-folder: "etcd-snapshots"')
    print('etcd-snapshot-retention: 30')
  '';

  # Reads [yolab_external] from config.toml and emits YE_URL / YE_TOKEN shell vars.
  parseYolabExternalCreds = pkgs.writeText "parse-ye-creds.py" ''
    import tomllib, sys, shlex
    with open(sys.argv[1], 'rb') as f:
        c = tomllib.load(f)
    ye = c.get('yolab_external', {})
    E = str()
    def q(s): return shlex.quote(str(s))
    print(f"YE_URL={q(ye.get('url', E).rstrip('/'))}")
    print(f"YE_TOKEN={q(ye.get('account_token', E))}")
  '';

  # Reads S3 JSON (from yolab-external /storage/s3) on stdin and emits S3_* shell vars.
  parseS3CredsFromJson = pkgs.writeText "parse-s3-json.py" ''
    import json, sys, shlex
    d = json.load(sys.stdin)
    def q(s): return shlex.quote(str(s))
    print(f"S3_ENDPOINT={q(d['endpoint'])}")
    print(f"S3_BUCKET={q(d['bucket_name'])}")
    print(f"S3_REGION={q(d['region'])}")
    print(f"S3_ACCESS_KEY={q(d['access_key_id'])}")
    print(f"S3_SECRET_KEY={q(d['secret_access_key'])}")
  '';

  # Reads SFTP JSON (from yolab-external /storage/sftp) on stdin and emits SFTP_* shell vars.
  parseSftpCredsFromJson = pkgs.writeText "parse-sftp-json.py" ''
    import json, sys, shlex
    d = json.load(sys.stdin)
    def q(s): return shlex.quote(str(s))
    print(f"SFTP_HOST={q(d['host'])}")
    print(f"SFTP_PORT={q(d['port'])}")
    print(f"SFTP_USER={q(d['username'])}")
    print(f"SFTP_PASS={q(d['password'])}")
  '';
in
{
  # ── Module options ────────────────────────────────────────────────────────
  # Consumed by platform overlays (wsl.nix, darwin/configuration.nix …).
  # Defaults cover the standard bare-metal / QEMU case.
  options.yolab = {
    platform = lib.mkOption {
      type = lib.types.str;
      default = "nixos";
      description = "Platform identifier forwarded to local-api (nixos, wsl, …).";
    };
    flakeTarget = lib.mkOption {
      type = lib.types.str;
      default = "yolab";
      description = "Flake output name used by nixos-rebuild switch.";
    };
    repoPath = lib.mkOption {
      type = lib.types.str;
      default = "/etc/nixos";
      description = "Absolute path to the yolab repo on this machine.";
    };
  };

  config = {
    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    # ── DNS ───────────────────────────────────────────────────────────────
    # Point the node itself at IPv6-capable public resolvers.
    # The same servers are written to /etc/k3s-resolv.conf so that CoreDNS
    # and kubelet use them as upstreams — essential on an IPv6-only host.
    networking.nameservers = [
      "2606:4700:4700::1111" # Cloudflare
      "2001:4860:4860::8888" # Google
    ];

    environment.etc."k3s-resolv.conf".text = ''
      nameserver 1.1.1.1
      nameserver 8.8.8.8
      nameserver 2606:4700:4700::1111
      nameserver 2001:4860:4860::8888
    '';

    # ── Networking ────────────────────────────────────────────────────────
    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall.enable = false;

      # ── WireGuard ──────────────────────────────────────────────────────
      #
      # Topology: hub-and-spoke.  Every node has ONE peer — the external
      # WireGuard server in yolab-external.  The server relays traffic
      # between nodes (Node A → hub → Node B).  New nodes appear on the
      # hub automatically via the wireguard-manager daemon; existing nodes
      # never need a rebuild when the cluster grows.
      #
      # Each node gets two addresses on wg0:
      #   sub_ipv6         – public, routed by the external DNS server.
      #                      Caddy binds here to serve the management UI.
      #   sub_ipv6_private – private cluster IP used by K3s, Flannel VXLAN,
      #                      kubelet, and the local-api fan-out calls.
      #
      # Routing strategy — two complementary rules:
      #
      #  A. Destination route (main table):
      #       ip -6 route add <privateSubnet> dev wg0
      #     Any packet headed for another node's cluster IP exits wg0,
      #     regardless of source.  This is what makes VXLAN and kubelet
      #     traffic work — those sockets may use a source address that the
      #     source-based policy rule below wouldn't catch.
      #
      #  B. Source policy (table 51820):
      #       ip -6 rule add from <our IPs> lookup 51820
      #       ip -6 route add ::/0 dev wg0 table 51820
      #     Return / keepalive / outbound traffic originating from our own
      #     WireGuard addresses also exits wg0, preventing asymmetric routing
      #     for inbound tunnel connections.
      wireguard.interfaces.wg0 = {
        ips = [
          "${s.tunnelCfg.sub_ipv6}/128"
          "${s.tunnelCfg.sub_ipv6_private}/128"
        ];
        privateKey = s.tunnelCfg.wg_private_key;

        postSetup = ''
          # A. Destination route: all cluster-node IPs go through wg0
          ip -6 route replace ${s.privateSubnet} dev wg0 2>/dev/null || true

          # B. Source policy: sub_ipv6 (public/Caddy address) always exits through wg0.
          #    sub_ipv6_private is NOT added here — it is a ULA address only reachable
          #    within the fd00:cafe::/112 cluster subnet, which is already covered by
          #    the destination route above (rule A).  Adding a source policy for
          #    sub_ipv6_private sends API-server replies to pods via wg0 instead of
          #    the local bridge, causing i/o timeouts for all in-cluster service traffic.
          ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 route replace ::/0 dev wg0 table 51820 2>/dev/null || true

          # C. Default route in main table: allows pod traffic (fd00:42::/56) to reach
          #    external IPv6 via wg0.  Flannel's --flannel-ipv6-masq SNATs the pod source
          #    to sub_ipv6, which is then picked up by source policy B above.  This is
          #    needed so app WireGuard sidecars can reach the hub to establish their tunnel.
          #    metric 200 loses to any ISP-provided default route (single encapsulation path)
          #    and wins only when no ISP IPv6 exists (double encapsulation path, still works).
          ip -6 route replace ::/0 dev wg0 metric 200 2>/dev/null || true
        '';

        preShutdown = ''
          ip -6 route del ${s.privateSubnet} dev wg0 2>/dev/null || true
          ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 table 51820 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 metric 200 2>/dev/null || true
        '';

        peers = [
          {
            # The external hub is the only peer on every node.
            # It knows about all registered nodes and relays traffic between them.
            publicKey = s.tunnelCfg.wg_server_public_key;
            endpoint = s.tunnelCfg.wg_server_endpoint;
            allowedIPs = [ "::/0" ];
            persistentKeepalive = 25;
          }
        ];
      };
    };

    # ── SSH ───────────────────────────────────────────────────────────────
    services.openssh = {
      enable = true;
      ports = [ s.sshPort ];
      settings = {
        PermitRootLogin = "prohibit-password";
        PasswordAuthentication = false;
      };
    };

    # ── Kernel ────────────────────────────────────────────────────────────
    boot.kernelModules = [
      "wireguard"
      "ip6_tables"
      "ip6table_filter"
      "ip6table_nat"
      "iptable_nat"
      "xt_conntrack"
      "br_netfilter"
      "overlay"
      "nf_nat"
      "ceph"
    ];

    boot.kernel.sysctl = {
      "net.bridge.bridge-nf-call-iptables" = 1;
      "net.bridge.bridge-nf-call-ip6tables" = 1;
      "net.ipv4.ip_forward" = 1;
      "net.ipv6.conf.all.forwarding" = 1;
      # Keep Ceph daemons in RAM — they perform poorly when swapped out.
      "vm.swappiness" = 10;
      "vm.dirty_ratio" = 40;
      "vm.dirty_background_ratio" = 10;
    };

    # ── K3s ───────────────────────────────────────────────────────────────
    #
    # Every node runs as a K3s *server* (control plane + worker).
    # Apps can be scheduled on any node; the cluster is HA once there are
    # 3+ nodes (embedded etcd quorum = n/2 + 1).
    #
    # Flannel backend: vxlan — NOT wireguard-native.
    #   wg0 already encrypts all inter-node traffic end-to-end.
    #   wireguard-native would add a second WireGuard layer on top (double
    #   encapsulation, ~2× overhead, more complex routing).  With vxlan, pod
    #   traffic is encapsulated then encrypted once by wg0 — simpler and faster.
    #
    # --cluster-dns: the 10th address of the service CIDR (fd00:43::a).
    #   K3s normally infers this, but we set it explicitly because the
    #   auto-inference can silently pick the wrong address with a custom
    #   IPv6-only CIDR.
    #
    # --tls-san: adds sub_ipv6_private to the API-server TLS certificate.
    #   Without this, joining nodes get a certificate mismatch when they
    #   connect to https://[sub_ipv6_private]:6443.
    #
    # --advertise-address: tells the API server which address to advertise
    #   to the rest of the cluster — must be the private cluster IP so that
    #   other nodes (via the hub relay) can reach it.
    services.k3s = {
      enable = true;
      role = "server";
      inherit (k3sCfg) token;
      clusterInit = isFirstNode;
      serverAddr = k3sCfg.server_addr; # "" on the first node — K3s ignores it

      extraFlags = [
        # Traefik is not used — YoLab exposes apps via WireGuard sidecars and
        # Caddy handles the management UI.  Leaving Traefik enabled causes its
        # svclb DaemonSet to bind hostPorts 80/443 on every node, which
        # conflicts with Caddy and causes it to receive SIGTERM.
        "--disable=traefik"
        "--flannel-backend=vxlan"
        "--flannel-ipv6-masq"
        "--cluster-cidr=fd00:42::/56,10.42.0.0/16"
        "--service-cidr=fd00:43::/112,10.43.0.0/16"
        "--cluster-dns=fd00:43::a"
        "--advertise-address=${s.tunnelCfg.sub_ipv6_private}"
        "--tls-san=${s.tunnelCfg.sub_ipv6_private}"
        "--resolv-conf=/etc/k3s-resolv.conf"
      ];
    };

    # Detect the node's outbound IPv4 at boot and write it to K3s's config file
    # as node-ip alongside the private IPv6, enabling dual-stack pods.
    # Running before K3s and after WireGuard ensures the IPv6 address is up.
    systemd.services.k3s-node-ip = {
      description = "Write K3s dual-stack node-ip config";
      after = [
        "wireguard-wg0.service"
        "network-online.target"
      ];
      wants = [ "network-online.target" ];
      before = [ "k3s.service" ];
      wantedBy = [ "k3s.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "k3s-node-ip" ''
          IPV4=$(${pkgs.iproute2}/bin/ip -4 route get 1.1.1.1 2>/dev/null | grep -oP 'src \K\S+' || true)
          CONFIG="${config.yolab.repoPath}/homelab/ignored/config.toml"
          mkdir -p /etc/rancher/k3s
          {
            if [ -n "$IPV4" ]; then
              echo "node-ip: ${s.tunnelCfg.sub_ipv6_private},$IPV4"
            else
              echo "node-ip: ${s.tunnelCfg.sub_ipv6_private}"
            fi
            ${pkgs.python3}/bin/python3 ${parseK3sEtcdS3Config} "$CONFIG" 2>/dev/null || true
          } > /etc/rancher/k3s/config.yaml
          chmod 600 /etc/rancher/k3s/config.yaml
        '';
      };
    };

    # K3s must start after WireGuard so the node-ip is reachable before K3s
    # tries to register itself with the cluster.
    systemd.services.k3s = {
      after = [
        "wireguard-wg0.service"
        "k3s-node-ip.service"
      ];
      wants = [ "wireguard-wg0.service" ];
      serviceConfig.TimeoutStopSec = "30";
    };

    # ── Caddy ─────────────────────────────────────────────────────────────
    # Serves the management UI over HTTPS on the node's public tunnel address.
    # Caddy is the only service that needs the public sub_ipv6.
    # Everything else — app installs, kubectl, inter-node API calls — travels
    # over private WireGuard addresses inside the cluster subnet.
    services.caddy = {
      enable = true;
      configFile = pkgs.writeText "Caddyfile" ''
        ${tunnelDomain} {
          handle /api/* {
            reverse_proxy [::1]:3001
          }
          handle {
            root * ${s.clientUi}
            try_files {path} /index.html
            file_server
          }
        }
      '';
    };

    systemd.services.caddy = {
      after = [ "wireguard-wg0.service" ];
      wants = [ "wireguard-wg0.service" ];
    };

    # ── System-disk OSD ───────────────────────────────────────────────────────
    # Creates a loop-file image on the root filesystem (no partitioning) and
    # attaches it as /dev/loop0 on every boot.
    #
    # Image creation is idempotent: fallocate runs only when the file does not
    # exist, so a fresh install allocates once and subsequent reboots are a
    # no-op.  fallocate pre-allocates real disk blocks (no sparse regions) so
    # BlueStore label writes always land on already-allocated blocks — sparse
    # files can be interrupted mid-allocation, leaving a half-written label.
    #
    # There is intentionally NO ExecStop.  Detaching the loop device while
    # the Ceph OSD pod is running causes the OSD to lose its block device
    # mid-operation and corrupt the BlueStore label.  nixos-rebuild restarts
    # this service but the loop must stay attached for as long as the OS runs.
    systemd.services.yolab-system-osd = {
      description = "System-disk Ceph OSD (loop-file)";
      wantedBy = [ "multi-user.target" ];
      after = [ "local-fs.target" ];
      before = [ "k3s.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "system-osd-start" ''
          set -euo pipefail
          IMG=/var/lib/rook/system-osd.img

          mkdir -p /var/lib/rook
          set -- $(${pkgs.coreutils}/bin/df -B1 / | tail -1)
          TARGET=$(( $2 * 3 / 4 ))
          # Only create on first boot. Never resize an existing image: enlarging
          # the file shifts the BlueStore backup-label offsets so expand_devices
          # reads zeroes, decodes a malformed label, and aborts the OSD.
          if [ ! -f "$IMG" ]; then
            ${pkgs.util-linux}/bin/fallocate -l "$TARGET" "$IMG"
          fi

          # Attach to /dev/loop0 so the device name is stable across reboots.
          # --direct-io=on bypasses the page cache — Ceph manages its own cache
          # and double-buffering against the backing file creates coherence risks.
          ATTACHED=$(${pkgs.util-linux}/bin/losetup -j "$IMG" 2>/dev/null | grep "^/dev/loop0:" || true)
          if [ -z "$ATTACHED" ]; then
            ${pkgs.util-linux}/bin/losetup -d /dev/loop0 2>/dev/null || true
            ${pkgs.util-linux}/bin/losetup --direct-io=on /dev/loop0 "$IMG"
          fi
        '';
      };
    };

    # ── Local API ──────────────────────────────────────────────────────────
    # Runs on every node.  The node the user opens in their browser queries
    # its own local-api, which fans out disk / storage / node requests to
    # sibling nodes via their private IPv6 addresses (discovered from kubectl).
    systemd.services.yolab-local-api = {
      after = [
        "network.target"
        "k3s.service"
      ];
      wants = [ "k3s.service" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin";
        YOLAB_REPO_PATH = config.yolab.repoPath;
        YOLAB_CONFIG = "${config.yolab.repoPath}/homelab/ignored/config.toml";
        YOLAB_PLATFORM = config.yolab.platform;
        YOLAB_FLAKE_TARGET = config.yolab.flakeTarget;
        YOLAB_NODE_IPV6 = s.tunnelCfg.sub_ipv6_private;
        KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "5s";
        ExecStart = "${s.localApiEnv}/bin/local-api";
      };
    };

    # ── Users ─────────────────────────────────────────────────────────────
    users.users.root.openssh.authorizedKeys.keys = lib.optional (s.rootSshKey != "") s.rootSshKey ++ [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK4KqHP17dqZURgVG7NwJ4sRoPVpmmNb3fMhGiWD529z nixos@nixos"
    ];

    users.users.homelab = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      openssh.authorizedKeys.keys = s.allowedSshKeys;
      hashedPassword = lib.mkIf (s.homelabPasswordHash != "") s.homelabPasswordHash;
    };

    services.logind.settings.Login.HandleLidSwitchExternalPower = "ignore";

    environment.systemPackages =
      with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        just
        wireguard-tools
        kubectl
        gptfdisk # sgdisk — wipes disks before Rook claims them
        dysk
        dust
        ctop
        vim
        wget
        htop
        sshfs
        fuse3
      ];

    # ── Rook / Ceph ───────────────────────────────────────────────────────────
    # K3s watches /var/lib/rancher/k3s/server/manifests/ and auto-applies
    # any YAML placed there.  Symlinks into the Nix store so updates
    # propagate on nixos-rebuild without manual kubectl apply.
    systemd.tmpfiles.rules = [
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml  - - - - ${./rook/operator.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-cluster.yaml   - - - - ${./rook/cluster.yaml}"
      # Velero is always deployed so the user can enable backups from the UI
      # without requiring a NixOS rebuild.  The bootstrap service (below) exits
      # gracefully when no credentials are configured.
      "L+ /var/lib/rancher/k3s/server/manifests/velero.yaml  - - - - ${./velero/helmchart.yaml}"
    ];

    # Bootstrap Velero credentials and backup schedule from config.toml at runtime.
    # Runs after K3s starts; waits for the HelmChart to install Velero, then creates
    # the cloud-credentials Secret, BackupStorageLocation, and daily Schedule.
    # Exits early if no S3 credentials are available (yolab-external not configured
    # and [velero] section absent / disabled).  Credentials never touch the Nix store.
    systemd.services.yolab-velero-bootstrap = {
      description = "Bootstrap Velero S3 credentials and backup schedule";
      after = [
        "k3s.service"
        "network-online.target"
      ];
      wants = [ "k3s.service" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin";
        KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
        YOLAB_CONFIG = "${config.yolab.repoPath}/homelab/ignored/config.toml";
      };
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        Restart = "on-failure";
        RestartSec = "30s";
        ExecStart = pkgs.writeShellScript "velero-bootstrap" ''
          set -euo pipefail
          CONFIG="$YOLAB_CONFIG"

          # Resolve S3 credentials: prefer yolab-external, fall back to [velero] in config.toml.
          S3_JSON=""
          eval "$(${pkgs.python3}/bin/python3 ${parseYolabExternalCreds} "$CONFIG" 2>/dev/null || echo 'YE_URL=; YE_TOKEN=')"
          if [ -n "$YE_URL" ] && [ -n "$YE_TOKEN" ]; then
            S3_JSON=$(${pkgs.curl}/bin/curl -sf --max-time 30 \
              -H "Authorization: Bearer $YE_TOKEN" \
              "$YE_URL/storage/s3" 2>/dev/null || true)
          fi

          TMPENV=$(mktemp)
          trap 'rm -f "$TMPENV"' EXIT
          if [ -n "$S3_JSON" ]; then
            echo "$S3_JSON" | ${pkgs.python3}/bin/python3 ${parseS3CredsFromJson} > "$TMPENV"
          else
            ${pkgs.python3}/bin/python3 ${parseVeleroEnv} "$CONFIG" > "$TMPENV"
          fi

          # If no credentials available at all, skip silently.  The local-api
          # UI can trigger a re-run via 'systemctl restart yolab-velero-bootstrap'.
          . "$TMPENV"
          if [ -z "${S3_BUCKET:-}" ]; then
            echo "No S3 credentials configured — Velero bootstrap skipped."
            exit 0
          fi

          echo "Waiting for velero namespace to appear..."
          until kubectl get namespace velero &>/dev/null; do sleep 15; done

          # Create or refresh the AWS credentials secret Velero reads at startup.
          kubectl create secret generic cloud-credentials \
            -n velero \
            --from-literal=cloud="$(printf '[default]\naws_access_key_id = %s\naws_secret_access_key = %s\n' "$S3_ACCESS_KEY" "$S3_SECRET_KEY")" \
            --dry-run=client -o yaml | kubectl apply -f -

          echo "Waiting for Velero deployment to be ready..."
          kubectl -n velero rollout status deployment/velero --timeout=300s

          # BackupStorageLocation — tells Velero where to store backups.
          kubectl apply -f - <<BSLEOF
          apiVersion: velero.io/v1
          kind: BackupStorageLocation
          metadata:
            name: default
            namespace: velero
          spec:
            provider: aws
            objectStorage:
              bucket: $S3_BUCKET
            config:
              region: $S3_REGION
              s3ForcePathStyle: "true"
              s3Url: $S3_ENDPOINT
          BSLEOF

          # Daily full-cluster backup at 03:00, retained for 30 days.
          kubectl apply -f - <<SCHEOF
          apiVersion: velero.io/v1
          kind: Schedule
          metadata:
            name: daily-full
            namespace: velero
          spec:
            schedule: "0 3 * * *"
            template:
              ttl: 720h0m0s
              storageLocation: default
              defaultVolumesToFsBackup: true
          SCHEOF

          echo "Velero bootstrap complete."
        '';
      };
    };

    # ── SFTP virtual drive ─────────────────────────────────────────────────────
    # Mounts the user's yolab-external SFTP virtual drive (backed by a Hetzner
    # Storage Box sub-account) at /mnt/yolab-sftp.  Credentials are fetched at
    # runtime from the yolab-external API using the account_token in config.toml.
    # Silently exits if [yolab_external] is not configured or SFTP is not provisioned.
    systemd.services.yolab-sftp-mount = {
      description = "Mount yolab SFTP virtual drive";
      after = [
        "network-online.target"
        "wireguard-wg0.service"
      ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin";
        YOLAB_CONFIG = "${config.yolab.repoPath}/homelab/ignored/config.toml";
      };
      path = [ pkgs.util-linux pkgs.sshfs pkgs.fuse3 pkgs.curl pkgs.python3 ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "sftp-mount" ''
          set -euo pipefail
          CONFIG="$YOLAB_CONFIG"

          eval "$(${pkgs.python3}/bin/python3 ${parseYolabExternalCreds} "$CONFIG" 2>/dev/null || echo 'YE_URL=; YE_TOKEN=')"
          if [ -z "$YE_URL" ] || [ -z "$YE_TOKEN" ]; then
            echo "yolab-external not configured, skipping SFTP mount."
            exit 0
          fi

          SFTP_JSON=$(${pkgs.curl}/bin/curl -sf --max-time 30 \
            -H "Authorization: Bearer $YE_TOKEN" \
            "$YE_URL/storage/sftp" 2>/dev/null || true)
          if [ -z "$SFTP_JSON" ]; then
            echo "No SFTP storage provisioned yet, skipping mount."
            exit 0
          fi

          TMPENV=$(mktemp)
          PASSFILE=$(mktemp)
          chmod 600 "$PASSFILE"
          trap 'rm -f "$TMPENV" "$PASSFILE"' EXIT
          echo "$SFTP_JSON" | ${pkgs.python3}/bin/python3 ${parseSftpCredsFromJson} > "$TMPENV"
          . "$TMPENV"

          mkdir -p /mnt/yolab-sftp

          if ${pkgs.util-linux}/bin/mountpoint -q /mnt/yolab-sftp; then
            echo "Already mounted."
            exit 0
          fi

          printf '%s' "$SFTP_PASS" > "$PASSFILE"

          ${pkgs.sshfs}/bin/sshfs \
            "$SFTP_USER@$SFTP_HOST:/" /mnt/yolab-sftp \
            -p "$SFTP_PORT" \
            -o password_stdin \
            -o StrictHostKeyChecking=no \
            -o reconnect \
            -o ServerAliveInterval=15 \
            -o ServerAliveCountMax=3 \
            < "$PASSFILE"
          echo "SFTP drive mounted at /mnt/yolab-sftp."

          # Create a sparse image file on the storage box and attach a loop
          # device to it so Ceph can use it as an OSD alongside local disks.
          # The local-api reads the size_gb from its request body; we default
          # to whatever size was already written.  If no file exists yet the
          # local-api's POST /api/disks/cloud endpoint creates it — we just
          # need to re-attach any existing image on subsequent boots.
          CLOUD_IMG=/mnt/yolab-sftp/yolab-cloud-osd.img
          if [ -f "$CLOUD_IMG" ]; then
            # Re-attach if not already attached (e.g. after reboot).
            if ! losetup -j "$CLOUD_IMG" | grep -q /dev/loop; then
              losetup --find --show --direct-io=on "$CLOUD_IMG" || true
            fi
          fi
        '';
        ExecStop = pkgs.writeShellScript "sftp-unmount" ''
          ${pkgs.fuse3}/bin/fusermount3 -u /mnt/yolab-sftp 2>/dev/null \
            || ${pkgs.util-linux}/bin/umount /mnt/yolab-sftp 2>/dev/null \
            || true
        '';
      };
    };

    system.activationScripts.yolabVersion = ''
      mkdir -p /var/lib/yolab
      ${pkgs.git}/bin/git -C ${config.yolab.repoPath} rev-parse HEAD        > /var/lib/yolab/built-hash    2>/dev/null || true
      ${pkgs.git}/bin/git -C ${config.yolab.repoPath} log -1 --pretty=%s    > /var/lib/yolab/built-message 2>/dev/null || true
      ${pkgs.git}/bin/git -C ${config.yolab.repoPath} log -1 --pretty=%cI   > /var/lib/yolab/built-date    2>/dev/null || true
    '';

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.settings.extra-substituters = [ "https://cache.demycode.ovh/yolab" ];
    nix.settings.extra-trusted-public-keys = [ "yolab:p/dOzQU8mPkD7kCCU9J7isVtBUT2gjq0RJror0uzkEo=" ];
    nix.gc.automatic = true;

    swapDevices = [
      {
        device = "/var/lib/swapfile";
        size = 8192;
      }
    ];
    services.swapspace.enable = true;
  };
}
