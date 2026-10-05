{
  config,
  facter,
  systems,
}: let
  declared = config.homelab.system or null;
  recorded = facter.system or null;
  chosen =
    if declared != null
    then declared
    else if recorded != null
    then recorded
    else "x86_64-linux";
in
  if builtins.elem chosen systems
  then chosen
  else throw "[homelab] system = \"${chosen}\" is not one YoLab builds for (${builtins.concatStringsSep ", " systems})"
