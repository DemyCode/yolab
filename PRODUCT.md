# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Two audiences read the same marketing page, and it has to serve both:

- **The buyer/installer** — technical enough to have run Plex, Home Assistant, or a
  Docker compose stack, and tired of the upkeep and the subscriptions. They will
  download an ISO, boot a spare PC, and follow the installer. This is who clicks.
- **The household beneficiary** — a partner, family member, or flatmate who will use
  the apps (photos, movies, files, passwords) and never sees the terminal. This is who
  has to *want* it for the buyer to bother.

The buyer needs proof and a real path; the beneficiary needs the emotional story. The
page must not talk down to the first or over the head of the second.

## Product Purpose

Turn an old computer into a private home server that replaces a stack of cloud
subscriptions. The visitor's success is completing setup — downloading the installer,
booting a spare machine, and ending with apps reachable from anywhere. YoLab exists
because self-hosting has always been possible but never been easy; it removes the two
hard parts, setup and remote access.

## Positioning

**The lead selling point is many machines working as one** (user decision 2026-10-07).
Every spare computer in the house joins the same private cloud with the same account
token: their disks become one pool, apps run on any of them, and from three machines on
the house keeps running when one stops. Umbrel, ZimaOS and TrueNAS Community each run
on a single computer, and that is the difference the site leads with. On the site it is
said as "machines", never "cluster". Never promise surviving a lost machine below three,
because two machines share a control plane that stalls when either one is gone.

The hard parts of self-hosting are done for you: a one-USB-stick NixOS-based installer
that shows no terminal, a curated catalogue of ~75 apps that each get their own address
a minute after you tap them, and a WireGuard tunnel that makes the house reachable from
anywhere without ever exposing it to the internet. A neighbouring product could copy
the app list; it could not truthfully copy "no Linux degree, no port forwarding, no
DynDNS, and your house stays private."

Privacy is the spine, not a footnote: nothing leaves the house, no third party is told
what you keep or watch, and no company can close your account over your own data. The
page must make a visitor believe two things at once — *I can actually do this myself*,
and *my stuff should live in my house* — with privacy carrying the second.

## Operating Context

- What the buyer needs: one spare PC, one USB stick, an internet connection, and a browser.
- Setup happens in an afternoon, in a browser, in three screens; the terminal never appears.
- Apps are reached at their own addresses from the sofa or from a beach.
- A console at `console.yolab.io` holds the account and the boxes.
- Money is only for traffic and storage actually used (about €2.40/month); the operating
  system is free and open source (MIT).
- The buyer is evaluating against what they already rent: Google Photos/iCloud, Netflix/
  Disney+/Spotify, 1Password/LastPass, Dropbox/Google Drive, game realms, and a dozen
  smaller subscriptions.

## Capabilities and Constraints

- Free, MIT-licensed, open-source operating system; paid tier covers traffic and storage.
- One-stick installer; no terminal; a curated app catalogue (~75 apps) covering photos,
  media, files, passwords, games, home automation, cameras, notes, budgets, documents.
- WireGuard-based remote access; the home network is never exposed.
- Backups and restore; a recovery key and token let a wiped or new cluster be rebuilt.
- The installer ISO is currently distributed through GitHub releases — a known weak
  link in the conversion path, not a product truth to enshrine.
- The marketing site and the box's own UI share one visual identity on purpose; they
  must keep looking like one product.

## Brand Commitments

- Name: **YoLab**.
- Voice: plain, concrete, confident. Banned register: "cluster", "64-bit", "ISO",
  "container" as selling words.
- Register: **a real product company** — clean, credible, product-led, high craft. The bar
  is the products the user named: **1Password, Umbrel, Linear, Vercel**. Execute that
  convention straight, at full fidelity, with no costume and no irony.
- **The product gets one new identity, from scratch.** The marketing site and the box's
  own UI (`homelab/client-ui`) share it and must keep looking like one product. The old
  warm-paper / pine-green world is not binding — but the two surfaces change together.
- Headline pitch: **"One cloud, as many machines as you like."** (was "Your own cloud, made simple.") Product and ease first; privacy and
  ownership are proof, not the opening line.

## Analytics and Identity

Umami is served first-party: Caddy proxies both the script and the collect
endpoint from this origin under names that say nothing about analytics (`/s`,
`/api/send`). This product's audience self-hosts and runs WireGuard — the most
ad-blocked demographic there is — so a third-party script would be blocked for a
large share of them, and the loss would read as a bad conversion rate rather
than as missing data.

The marketing site (`yolab.io`) stays anonymous and session-only: no cookies, no
identifiers, no consent banner needed.

The console (`console.yolab.io`) is different once someone signs in.
`identifyUser()` ties activity to a stable per-account id instead of an
anonymous per-session one, deliberately, so the same account's usage correlates
across devices and repeat visits. **That is a real identifier, not anonymous
analytics — the consent-banner question has to be revisited if this path ever
ships broadly.**

A user's DNS query names are never recorded. Which names they look up, and when,
is what they do with their machines, and the platform keeps none of it; the one
exception is a malformed record the operator configured themselves. That rule is
enforced by a test in `dns-server`, not by convention.

## Evidence on Hand

- **No screenshots of the real product have been captured yet** — they are to be produced
  from the running console / node UI and used on the page.
- **No demo video or GIF exists yet** — planned.
- **No real users or testimonials exist yet.** A placeholder may hold the space, clearly
  marked, and no testimonial may be fabricated.
- Real, usable proof that does exist: the open-source repository, the app catalogue, the
  installer, and the running product itself.

## Product Principles

1. **Show the product, don't describe it.** The page's job is to make the running thing
   visible; a drawing of a house is not evidence.
2. **One primary action.** Every path leads to starting the install; secondary links
   never compete with it.
3. **Honest about effort.** It is an afternoon and a spare PC — say so, and prove the
   three screens. Over-promising "one click" would undercut the trust the product needs.
4. **Two readers, one page.** Lead with the household's story, back it with the buyer's
   proof, never condescend to either.
5. **Look like the product.** The site is the first screen of the thing they will run.

## Accessibility & Inclusion

- Existing standard to preserve: visible `:focus-visible` rings, `prefers-reduced-motion`
  honoured, semantic headings. No additional product-specific requirement was confirmed.
