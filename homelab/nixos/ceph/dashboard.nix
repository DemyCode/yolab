{lib, ...}:
with lib; {
  options.yolab.ceph.dashboard = {
    enable = mkEnableOption "the Ceph dashboard on this node's mgr" // {default = true;};

    port = mkOption {
      type = types.port;
      default = 7000;
      description = "Port the mgr dashboard listens on, on the cluster address.";
    };

    urlPrefix = mkOption {
      type = types.str;
      default = "/ceph-dashboard";
      description = "Sub-path the dashboard is served under, so its single-page app builds URLs that reach the proxy.";
    };

    passwordFile = mkOption {
      type = types.path;
      default = "/var/lib/ceph/dashboard-password";
      description = "Where the generated admin password is kept, so local-api and the dashboard cannot disagree.";
    };
  };
}
