{lib}: {
  prefix64 = addr: let
    halves = lib.splitString "::" addr;
    groups = str: builtins.filter (g: g != "") (lib.splitString ":" str);
    front = groups (builtins.elemAt halves 0);
    back =
      if builtins.length halves > 1
      then groups (builtins.elemAt halves 1)
      else [];
    full = front ++ lib.genList (_: "0") (8 - builtins.length front - builtins.length back) ++ back;
  in "${lib.concatStringsSep ":" (lib.take 4 full)}::/64";
}
