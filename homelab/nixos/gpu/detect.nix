lib: report: let
  cards = report.hardware.graphics_card or [];

  hexOf = id:
    if builtins.isAttrs id
    then lib.toLower (id.hex or "")
    else "";

  vendorOf = card: hexOf (card.vendor or null);
  deviceIdOf = card: let
    hex = hexOf (card.device or null);
  in
    if hex == ""
    then null
    else lib.fromHexString hex;

  drivers = card: (card.driver_modules or []) ++ lib.optional (card ? driver) card.driver;
  usesAny = names: card: lib.any (d: builtins.elem d names) (drivers card);

  isNvidia = card: vendorOf card == "10de" || usesAny ["nvidia" "nouveau"] card;
  isAmd = card: vendorOf card == "1002" || usesAny ["amdgpu" "radeon"] card;
  isIntel = card: vendorOf card == "8086" || usesAny ["i915" "xe"] card;

  within = id: low: high: id >= lib.fromHexString low && id <= lib.fromHexString high;

  keplerRanges = [
    ["0fc0" "0fff"]
    ["1000" "103f"]
    ["1180" "11ff"]
    ["1280" "12ff"]
  ];

  nvidiaGeneration = card: let
    id = deviceIdOf card;
  in
    if id == null
    then "turing"
    else if id >= lib.fromHexString "1e00"
    then "turing"
    else if id >= lib.fromHexString "1340"
    then "maxwell"
    else if lib.any (r: within id (builtins.elemAt r 0) (builtins.elemAt r 1)) keplerRanges
    then "kepler"
    else "fermi";

  branchOf = {
    turing = "stable";
    maxwell = "legacy_580";
    kepler = "legacy_470";
  };

  nvidiaCards = builtins.filter isNvidia cards;
  generations = map nvidiaGeneration nvidiaCards;
  drivable = builtins.filter (g: g != "fermi") generations;
  oldest =
    if builtins.elem "kepler" drivable
    then "kepler"
    else if builtins.elem "maxwell" drivable
    then "maxwell"
    else "turing";

  nvidia = {
    present = drivable != [];
    generations = lib.unique generations;
    nouveauOnly = generations != [] && drivable == [];
    unsupported = builtins.elem "fermi" generations;
    branch =
      if drivable == []
      then null
      else branchOf.${oldest};
    open = drivable != [] && oldest == "turing";
  };
  amd = lib.any isAmd cards;
  intel = lib.any isIntel cards;
in {
  inherit nvidia amd intel;
  any = nvidia.present || amd || intel;
}
