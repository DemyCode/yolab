// What each app *is*, in the words someone would use who has never heard of it.
//
// This is the highest-leverage content in the product. The catalog is 50-odd
// names like Vikunja, Karakeep, Miniflux and Navidrome — to almost everyone
// that is 50 unknown words, and a grid of unknown words is not a shop, it is a
// wall. One line comparing each to something they already pay for turns the
// same grid into somewhere you can find what you came for.
//
// The comparisons name well-known products on purpose. They are *analogies*,
// not claims of parity or affiliation, and they are phrased as such ("like").
//
// This lives in the front end rather than in each Chart.yaml because it is
// presentation copy that wants editing as a set — you write it by reading the
// whole list and balancing it, not one chart at a time. If it starts drifting
// from the catalog, moving it into `yolab.io/tagline` annotations is the
// obvious fix; `taglineFor` already falls back to the chart's own description
// so apps from user-added repos are never blank.

export interface Group {
  id: string;
  label: string;
}

/** Ordered: the earlier a group appears, the more people came looking for it. */
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

interface AppMeta {
  /** One line. What it does, then what it is like. */
  tagline: string;
  group: string;
}

export const APP_META: Record<string, AppMeta> = {
  // Photos
  immich: {
    tagline: "Your photos and videos, like Google Photos",
    group: "photos",
  },
  photoprism: {
    tagline: "A photo library that can search itself",
    group: "photos",
  },

  // Films & TV
  jellyfin: {
    tagline: "Your films and TV, like Netflix but yours",
    group: "watch",
  },
  jellyseerr: { tagline: "Ask for a film and have it appear", group: "watch" },
  qbittorrent: { tagline: "Download large files", group: "watch" },
  metube: {
    tagline: "Save videos from the web to watch later",
    group: "watch",
  },

  // Music & Books
  navidrome: {
    tagline: "Your music collection, like Spotify",
    group: "listen",
  },
  audiobookshelf: {
    tagline: "Audiobooks and podcasts, like Audible",
    group: "listen",
  },
  kavita: {
    tagline: "Comics, manga and books in one library",
    group: "listen",
  },
  "calibre-web": {
    tagline: "Your ebooks, like a Kindle library",
    group: "listen",
  },

  // Files & Documents
  nextcloud: {
    tagline: "Files, calendar and contacts, like Google Drive",
    group: "files",
  },
  syncthing: {
    tagline: "Keep folders in sync between your devices",
    group: "files",
  },
  filebrowser: {
    tagline: "Browse and share the files on your box",
    group: "files",
  },
  "paperless-ngx": {
    tagline: "Scan your paperwork and actually find it again",
    group: "files",
  },
  "stirling-pdf": { tagline: "Merge, split and sign PDFs", group: "files" },

  // Notes & Planning
  appflowy: {
    tagline: "Notes, docs and projects, like Notion",
    group: "notes",
  },
  docmost: { tagline: "Shared documents and a team wiki", group: "notes" },
  memos: { tagline: "Quick notes you jot and forget", group: "notes" },
  bookstack: {
    tagline: "Organise what you know, like a personal wiki",
    group: "notes",
  },
  excalidraw: { tagline: "Sketch ideas on a whiteboard", group: "notes" },
  vikunja: { tagline: "To-do lists and plans, like Todoist", group: "notes" },
  planka: {
    tagline: "Task boards you drag around, like Trello",
    group: "notes",
  },

  // Reading
  freshrss: {
    tagline: "Follow sites without an algorithm, like Feedly",
    group: "read",
  },
  miniflux: { tagline: "A very quiet news reader", group: "read" },
  wallabag: {
    tagline: "Save articles to read later, like Pocket",
    group: "read",
  },
  linkwarden: {
    tagline: "Bookmarks that keep a copy of the page",
    group: "read",
  },
  karakeep: { tagline: "Everything you meant to come back to", group: "read" },

  // Money & Passwords
  vaultwarden: { tagline: "Your passwords, like 1Password", group: "personal" },
  "2fauth": { tagline: "Your two-factor codes, like Authy", group: "personal" },
  actual: { tagline: "Budget your money, like YNAB", group: "personal" },
  "firefly-iii": { tagline: "Track where your money goes", group: "personal" },
  monica: {
    tagline: "Remember birthdays, gifts and the people in your life",
    group: "personal",
  },
  wallos: {
    tagline: "See every subscription you're paying for, in one place",
    group: "personal",
  },
  openclaw: {
    tagline: "A personal AI assistant you can message like a friend",
    group: "personal",
  },

  // Home & Family
  mealie: {
    tagline: "Recipes and what you are eating this week",
    group: "home",
  },
  dawarich: {
    tagline: "Your own location history, kept private",
    group: "home",
  },
  minecraft: {
    tagline: "A Minecraft world for you and your friends",
    group: "home",
  },
  valheim: {
    tagline: "A Valheim server for you and your friends",
    group: "home",
  },
  ntfy: { tagline: "Send yourself notifications from anything", group: "home" },
  "home-assistant": {
    tagline: "Control your lights, thermostat and smart devices",
    group: "home",
  },
  frigate: {
    tagline: "Security cameras that recognise people, not just motion",
    group: "home",
  },
  grocy: {
    tagline: "Track groceries, chores and what's about to expire",
    group: "home",
  },
  romm: {
    tagline: "Your retro game collection, organised and playable",
    group: "home",
  },

  // Website & Chat
  ghost: { tagline: "Publish a blog or newsletter, like Medium", group: "web" },
  shlink: { tagline: "Short links you own, like Bitly", group: "web" },
  umami: {
    tagline: "See who visits your site, without tracking them",
    group: "web",
  },
  synapse: { tagline: "Run your own chat server", group: "web" },
  cinny: { tagline: "A friendly way into your chat server", group: "web" },
  strfry: {
    tagline: "Carry your own corner of the Nostr network",
    group: "web",
  },

  // Tools
  homepage: {
    tagline: "A start page linking everything you run",
    group: "tools",
  },
  searxng: { tagline: "Search the web without being profiled", group: "tools" },
  changedetection: {
    tagline: "Tell me when this web page changes",
    group: "tools",
  },
  n8n: { tagline: "Connect your apps together, like Zapier", group: "tools" },
  "open-webui": {
    tagline: "Chat with AI models, like ChatGPT",
    group: "tools",
  },
  "reactive-resume": { tagline: "Build a CV that looks good", group: "tools" },
  librespeed: {
    tagline: "Test how fast your connection really is",
    group: "tools",
  },
  "uptime-kuma": {
    tagline: "Get told the moment a site goes down",
    group: "tools",
  },
  grafana: { tagline: "Turn numbers into charts", group: "tools" },
  "it-tools": { tagline: "A drawer of small, handy utilities", group: "tools" },

  // Developer
  gitea: { tagline: "Host your code, like GitHub", group: "dev" },
  "code-server": { tagline: "VS Code in a browser tab", group: "dev" },
};

/** Chart categories, for apps we have no hand-written entry for. */
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

export function taglineFor(app: { id: string; description?: string }): string {
  return APP_META[app.id]?.tagline ?? app.description ?? "";
}

export function groupFor(app: { id: string; category?: string }): string {
  return (
    APP_META[app.id]?.group ?? CATEGORY_TO_GROUP[app.category ?? ""] ?? "tools"
  );
}

export function groupLabel(id: string): string {
  return GROUPS.find((g) => g.id === id)?.label ?? "Other";
}
