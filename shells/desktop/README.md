# YoLab desktop shell

A window pointed at your box. That is the whole design, and it is deliberate.

The obvious alternative is to ship the built web UI inside the app and have it
call the box's API. That sounds tidier and is worse in every way that matters
here: the bundle and the box drift apart between releases, the API calls become
cross-origin so the session cookie is no longer sent, `allow_origin(Any)` on the
box forbids credentials entirely, and recovering from that means inventing
device tokens, a pairing flow and a revocation list — several hundred lines of
new security surface to replace a cookie that already works.

Loading the UI from the box avoids all of it. The webview *is* on
`https://your-box`, so it is same-origin: the existing login page works, the
existing `HttpOnly; Secure; SameSite=Strict` cookie is sent, and nothing in
local-api changes. The UI is also always the version that box is running, so a
system update ships the interface with it and no app release is needed.

The one thing the shell owns is the address. On first launch it asks for it,
stores it, and from then on opens straight into the box.
