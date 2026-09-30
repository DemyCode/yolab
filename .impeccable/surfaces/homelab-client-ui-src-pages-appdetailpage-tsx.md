---
version: 1
slug: "homelab-client-ui-src-pages-appdetailpage-tsx"
primary_target: "homelab/client-ui/src/pages/AppDetailPage.tsx"
related_targets: ["homelab/client-ui/src/components/AppAccess.tsx"]
---

# Surface: app page (box UI)

Scope: `/app/:instanceName` in the box UI. Visitor mode: **Operate**. Extension of the
established world (DESIGN.md, "App UI" section); no new identity.

## Audience, job, action

The person who installed an app, or someone in their household, opening one app's page.
They come to answer three questions in order: *can I open it*, *is it OK*, *is it safe*
(backed up). Primary action: **Open** the app. Everything else is occasional:
passwords, backups, restore, update, duplicate, remove, logs.

## States and ranges

ready · starting · copying · stopped (ran, then broke; retrying) · failed (never came up;
reason from the pod or helm) · removing · restoring · restore finished · restore failed.
0–3 web addresses, 0–6 access outputs (secrets, multiline, waiting), 0–n backups,
one to several pods. A failed or removing app gets a different page shape, not the
healthy page with a banner on top.

## Direction contract

**THESIS.** One app, one question at a time: open it, see it is fine, know it is safe.
Refuses the admin-panel stack of equal-weight buttons, badges and forms.

**OWN-WORLD.** The Quiet Machine at app scale: paper sections of hairline-separated rows,
section headings outside the card, one Signal Blue button, quiet blue row actions,
mono for every value, status as a coloured dot plus plain words.

**STORY.** The header says what it is and whether it is running, with Open beside it.
Below: Access, Backups, About (version, copy, ID, technical details), and Remove last.

**FIRST VIEWPORT.** Icon, name, status line and Open; the single status banner when
there is one; the Access section beginning beneath.

**FORM.** A settings-list detail page in the category standard (1Password item view,
iOS Settings), executed straight.

**FINISH.** Detector clean on changed files; finish review with source evidence (no live
render is possible without a running box); DESIGN.md already carries the App UI rules.

## Unresolved

- No screenshots can be taken without a running box; visual verification is by the user.
