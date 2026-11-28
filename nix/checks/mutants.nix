{
  lib,
  x86,
  builds,
  crates,
  ...
}: let
  localApiShards = 8;
in
  lib.optionalAttrs x86 (
    lib.listToAttrs (lib.genList (shard:
      lib.nameValuePair "local-api-mutants-${toString (shard + 1)}-of-${toString localApiShards}"
      (crates.local-api.mutantsShard shard localApiShards))
    localApiShards)
    // {
      client-ui-mutants = builds.clientUiMutants;
      installer-mutants = crates.installer.mutants;
      desktop-client-mutants = crates.desktop-client.mutants;
    }
  )
