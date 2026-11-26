{
  builds,
  crates,
  ...
}: {
  client-ui-mutants = builds.clientUiMutants;
  local-api-mutants = crates.local-api.mutants;
  installer-mutants = crates.installer.mutants;
  desktop-client-mutants = crates.desktop-client.mutants;
}
