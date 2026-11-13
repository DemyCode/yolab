export interface SshKeyView {
  id: string;
  kind: string;
  fingerprint: string;
  comment: string;
  label: string;
}

export interface SshKeys {
  user: string;
  port: number | null;
  keys: SshKeyView[];
}

export function sshCommand(user: string, host: string, port: number | null) {
  const target = host.includes(":") ? `${user}@[${host}]` : `${user}@${host}`;
  return port && port !== 22 ? `ssh -p ${port} ${target}` : `ssh ${target}`;
}

export function keyName(key: SshKeyView) {
  return key.label || key.comment || key.kind;
}

export function looksLikePrivateKey(text: string) {
  return text.includes("PRIVATE KEY");
}
