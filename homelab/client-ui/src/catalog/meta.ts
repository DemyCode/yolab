export interface Group {
  id: string;
  label: string;
}

export const GROUPS: Group[] = [
  { id: "photos", label: "Photos" },
  { id: "watch", label: "Films & TV" },
  { id: "listen", label: "Music & Books" },
  { id: "files", label: "Files & Documents" },
  { id: "notes", label: "Notes & Planning" },
  { id: "read", label: "Reading" },
  { id: "personal", label: "Money & Passwords" },
  { id: "home", label: "Home & Family" },
  { id: "web", label: "Website & Chat" },
  { id: "tools", label: "Tools" },
  { id: "dev", label: "Developer" },
];

export const APP_GROUP: Record<string, string> = {
  immich: "photos",
  photoprism: "photos",
  jellyfin: "watch",
  jellyseerr: "watch",
  "media-stack": "watch",
  qbittorrent: "watch",
  metube: "watch",
  navidrome: "listen",
  audiobookshelf: "listen",
  kavita: "listen",
  "calibre-web": "listen",
  nextcloud: "files",
  syncthing: "files",
  filebrowser: "files",
  "paperless-ngx": "files",
  "stirling-pdf": "files",
  appflowy: "notes",
  docmost: "notes",
  memos: "notes",
  bookstack: "notes",
  excalidraw: "notes",
  vikunja: "notes",
  planka: "notes",
  freshrss: "read",
  miniflux: "read",
  wallabag: "read",
  linkwarden: "read",
  karakeep: "read",
  vaultwarden: "personal",
  "2fauth": "personal",
  actual: "personal",
  "firefly-iii": "personal",
  monica: "personal",
  wallos: "personal",
  openclaw: "personal",
  mealie: "home",
  dawarich: "home",
  minecraft: "home",
  valheim: "home",
  "steam-headless": "home",
  ntfy: "home",
  "home-assistant": "home",
  frigate: "home",
  grocy: "home",
  romm: "home",
  ghost: "web",
  shlink: "web",
  umami: "web",
  synapse: "web",
  cinny: "web",
  strfry: "web",
  homepage: "tools",
  searxng: "tools",
  changedetection: "tools",
  n8n: "tools",
  "open-webui": "tools",
  "reactive-resume": "tools",
  librespeed: "tools",
  "uptime-kuma": "tools",
  grafana: "tools",
  "it-tools": "tools",
  gitea: "dev",
  "code-server": "dev",
};

const CATEGORY_TO_GROUP: Record<string, string> = {
  media: "watch",
  productivity: "notes",
  utilities: "tools",
  monitoring: "tools",
  communication: "web",
  gaming: "home",
  development: "dev",
  security: "personal",
  ai: "tools",
};

export function taglineFor(app: {
  tagline?: string;
  description?: string;
}): string {
  return app.tagline?.trim() || app.description || "";
}

export function groupFor(app: { id: string; category?: string }): string {
  return (
    APP_GROUP[app.id] ?? CATEGORY_TO_GROUP[app.category ?? ""] ?? "tools"
  );
}

export function groupLabel(id: string): string {
  return GROUPS.find((g) => g.id === id)?.label ?? "Other";
}
