{
  lib,
  cfg,
  failOn,
  ...
}: {
  wireguard-carries-only-exposure-and-mesh = let
    interfaces = cfg.networking.wireguard.interfaces;
    defaultRoutes = [
      "::/0"
      "0.0.0.0/0"
    ];
    isMainTableDefault = line:
      builtins.match ".*route (add|replace) (::/0|0\\.0\\.0\\.0/0|default) dev .*" line
      != null
      && builtins.match ".*table .*" line == null;
    routesPeerDefault = i:
      (i.allowedIPsAsRoutes or true)
      && lib.any (p: lib.any (a: builtins.elem a defaultRoutes) p.allowedIPs) i.peers;
    ipv6 = import ../../homelab/nixos/ipv6.nix {inherit lib;};
    prefixCases = {
      "2a01:4f8:1c1e:43ff::20" = "2a01:4f8:1c1e:43ff::/64";
      "2a01:4f8:1c1e:43ff:0:0:0:20" = "2a01:4f8:1c1e:43ff::/64";
      "2a01:4f8::20" = "2a01:4f8:0:0::/64";
      "fd00:1::" = "fd00:1:0:0::/64";
      "2001:db8:aa:bb:cc:dd:ee:ff" = "2001:db8:aa:bb::/64";
    };
    wg0Setup = interfaces.wg0.postSetup or "";
  in
    failOn "wireguard-carries-only-exposure-and-mesh" {
      problems =
        lib.optional (!(interfaces ? wg0)) "wg0 is gone, so this check proves nothing"
        ++ lib.concatLists (
          lib.mapAttrsToList (
            addr: want: let
              got = ipv6.prefix64 addr;
            in
              lib.optional (got != want) "prefix64 ${addr} gave ${got}, expected ${want}"
          )
          prefixCases
        )
        ++ lib.optional (!(lib.hasInfix "route replace fd00:1:0:0::/64 dev wg0" wg0Setup))
        "wg0 no longer routes YoLab's own address range through the tunnel, so packages cannot reach each other's public addresses from an IPv4-only home"
        ++ lib.concatLists (
          lib.mapAttrsToList (
            name: i:
              map (l: "${name} postSetup routes everything through the tunnel: ${l}") (
                builtins.filter isMainTableDefault (lib.splitString "\n" (i.postSetup or ""))
              )
              ++ lib.optional (routesPeerDefault i) "${name} turns a peer's catch-all allowedIPs into a main-table default route"
          )
          interfaces
        );
      explain = ''
        WireGuard exists to make public addresses reachable and to link the
        machines. Outbound internet traffic from the OS and from apps must leave
        through the machine's own connection, never through the YoLab server.'';
    };

  ssh-takes-only-keys = let
    ssh = cfg.services.openssh.settings;
    keyFiles = cfg.services.openssh.authorizedKeysFiles;
    rootKeys = cfg.users.users.root.openssh.authorizedKeys.keys;
  in
    failOn "ssh-takes-only-keys" {
      problems =
        lib.optional (builtins.length rootKeys > 1)
        "root may hold only the key this machine's own config.toml names, never one baked into every machine"
        ++ lib.optional (!(builtins.elem "/var/lib/yolab-ssh/%u.keys" keyFiles))
        "sshd must read /var/lib/yolab-ssh/%u.keys, the keys added from the Machines page"
        ++ lib.optional (ssh.PasswordAuthentication != false) "PasswordAuthentication must be false"
        ++ lib.optional (ssh.KbdInteractiveAuthentication != false)
        "KbdInteractiveAuthentication must be false: through PAM it asks for the homelab user's password, which is the UI password, and that user is in wheel"
        ++ lib.optional ((ssh.AuthenticationMethods or null) != "publickey") "AuthenticationMethods must be publickey"
        ++ lib.optional (ssh.PermitRootLogin != "prohibit-password") "PermitRootLogin must be prohibit-password"
        ++ lib.optional (!cfg.services.fail2ban.enable) "fail2ban must be on: SSH faces the internet on the tunnel address";
    };

  pods-cannot-reach-the-local-api = let
    rules = cfg.networking.firewall.extraCommands;
    sshPort = toString (builtins.head cfg.services.openssh.ports);
    wanted = [
      "for pods in cni0 flannel.1 flannel-v6.1; do"
      "for port in 3001 ${sshPort} 2379 2380; do"
      "iptables -I nixos-fw 1 -i \"$pods\" -p tcp --dport \"$port\" -j nixos-fw-refuse"
      "ip6tables -I nixos-fw 1 -i \"$pods\" -p tcp --dport \"$port\" -j nixos-fw-refuse"
    ];
  in
    failOn "pods-cannot-reach-the-local-api" {
      problems = builtins.filter (r: !(lib.hasInfix r rules)) wanted;
      explain = ''
        The host firewall trusts the pod interfaces, so a hacked service
        could call the local API on 3001; these refuse rules are gone:'';
    };
}
