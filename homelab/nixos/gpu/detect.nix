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
  isAmd = card: vendorOf card == "1002" || usesAny ["amdgpu"] card;
  isIntel = card: vendorOf card == "8086" || usesAny ["i915" "xe"] card;

  firstMaxwell = lib.fromHexString "1340";
  firstTuring = lib.fromHexString "1e00";

  nvidiaGeneration = card: let
    id = deviceIdOf card;
  in
    if id == null
    then "turing"
    else if id >= firstTuring
    then "turing"
    else if id >= firstMaxwell
    then "maxwell"
    else "kepler";

  nvidiaCards = builtins.filter isNvidia cards;
  generations = map nvidiaGeneration nvidiaCards;
  drivable = builtins.filter (g: g != "kepler") generations;
  needsLegacy = builtins.elem "maxwell" drivable;

  nvidia = {
    present = drivable != [];
    unsupported = builtins.elem "kepler" generations;
    branch =
      if drivable == []
      then null
      else if needsLegacy
      then "legacy_580"
      else "stable";
    open = drivable != [] && !needsLegacy;
  };
  amd = lib.any isAmd cards;
  intel = lib.any isIntel cards;
in {
  inherit nvidia amd intel;
  any = nvidia.present || amd || intel;
}
