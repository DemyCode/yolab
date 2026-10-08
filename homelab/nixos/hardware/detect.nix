lib: report: let
  cpus = report.hardware.cpu or [];
  vendors = lib.unique (map (c: c.vendor_name or "") cpus);
in {
  known = report != {};
  intel = builtins.elem "GenuineIntel" vendors;
  amd = builtins.elem "AuthenticAMD" vendors;
}
