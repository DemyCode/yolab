# The Ceph dashboard, served from the host mgr instead of Rook.
#
# WHY THIS FILE EXISTS
# --------------------
# The dashboard link on the Storage page pointed at [fd00:43::cefd]:7000 — a
# Kubernetes ClusterIP for Rook's dashboard Service. Ceph moved out of
# Kubernetes (see ./default.nix), that Service went with it, and the link has
# been returning 502 ever since. The credentials shown next to it came from the
# `rook-ceph-dashboard-password` Secret, which no longer exists either, so the
# page displayed a masked empty string.
#
# WHY IT IS NOT SIMPLY A REVERSE PROXY TO [::1]:7000
# --------------------------------------------------
# The dashboard runs on the ACTIVE mgr only. Every node here runs one, so on any
# given machine the local mgr is usually a standby — and a standby does not
# serve the dashboard, it redirects to the active one. That redirect names the
# active mgr's own address, which is on the WireGuard mesh and unreachable from
# a browser out on the internet. Proxying to the local mgr therefore works or
# breaks depending on which machine happens to hold the active mgr, which is
# exactly the kind of invisible state this project keeps getting bitten by.
#
# So local-api proxies it instead: it asks Ceph which mgr is active
# (`ceph mgr services`) and forwards there over the mesh. Failover changes the
# answer and nothing else has to notice.
{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.dashboard;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
  cephPath = with pkgs; [ceph ceph-client coreutils openssl gnugrep gnused jq curl];
in {
  options.yolab.ceph.dashboard = {
    enable = mkEnableOption "the Ceph dashboard on this node's mgr" // {default = true;};

    port = mkOption {
      type = types.port;
      default = 7000;
      description = "Port the mgr dashboard listens on, on the cluster address.";
    };

    urlPrefix = mkOption {
      type = types.str;
      default = "/ceph-dashboard";
      description = ''
        Sub-path the dashboard is served under. It has to be told this: the
        dashboard is a single-page app that builds its own asset and API URLs,
        and without a prefix it generates links rooted at "/" that miss the
        proxy entirely and land on the YoLab UI instead.
      '';
    };

    passwordFile = mkOption {
      type = types.path;
      default = "/var/lib/ceph/dashboard-password";
      description = ''
        Where the generated admin password is kept. local-api reads this to show
        the credentials on the Storage page, so the two can never disagree —
        which is what happened when the password lived in a Kubernetes Secret
        and the dashboard did not.
      '';
    };
  };

  config = mkIf (cephCfg.enable && cfg.enable) {
    systemd.services.yolab-ceph-dashboard = {
      description = "Enable and configure the Ceph dashboard";
      after = ["ceph-mgr-${host}.service"];
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "oneshot";
        # `Type=oneshot` disables the start timeout by default; see the note on
        # yolab-ceph-bootstrap in default.nix.
        TimeoutStartSec = "300s";
      };
      path = cephPath;
      script = ''
        set -uo pipefail

        # Every Ceph call bounded: this runs at boot and on a timer, and an
        # unreachable cluster must not hold either.
        if ! timeout 20 ceph -s >/dev/null 2>&1; then
          echo "ceph not reachable yet — the dashboard will be configured on a later run"
          exit 0
        fi

        # The module has to be on before any of its config keys are accepted.
        if ! timeout 30 ceph mgr module ls --format json 2>/dev/null \
          | jq -e '.enabled_modules | index("dashboard")' >/dev/null; then
          echo "enabling the dashboard module"
          timeout 60 ceph mgr module enable dashboard || {
            echo "could not enable the dashboard module — will retry" >&2
            exit 0
          }
        fi

        # TLS off on purpose. Caddy terminates HTTPS at the edge and this is
        # reached only over the WireGuard mesh, so a second, self-signed
        # certificate underneath buys nothing and means the proxy has to be
        # told to trust it.
        timeout 20 ceph config set mgr mgr/dashboard/ssl false
        timeout 20 ceph config set mgr mgr/dashboard/url_prefix ${cfg.urlPrefix}
        timeout 20 ceph config set mgr mgr/dashboard/server_port ${toString cfg.port}
        timeout 20 ceph config set mgr mgr/dashboard/ssl_server_port ${toString cfg.port}

        # Bind the mesh address, not localhost: whichever node holds the active
        # mgr has to be reachable from the node the browser is talking to.
        timeout 20 ceph config set mgr mgr/dashboard/${host}/server_addr ${cephCfg.monAddr}

        # One password, generated once, kept on disk. Regenerating it on every
        # run would silently invalidate a session the user is in the middle of.
        if [ ! -s ${cfg.passwordFile} ]; then
          echo "generating the dashboard password"
          install -d -m 0755 "$(dirname ${cfg.passwordFile})"
          # printf, NOT `... | cut > file`. cut terminates its output with a
          # newline, so the file held "<password>\n" — Ceph was given the
          # newline as part of the password while local-api trims it before
          # showing it on the Storage page. The password on screen was then not
          # the password Ceph had stored, and logging in failed with "Invalid
          # credentials" while both halves looked correct.
          #
          # tr -dc keeps only alphanumerics: this value gets copied by hand, so
          # no character in it should need escaping or be confusable.
          PW=$(openssl rand -base64 32 | tr -dc 'A-Za-z0-9' | cut -c1-20)
          printf '%s' "$PW" > ${cfg.passwordFile}
          chmod 0600 ${cfg.passwordFile}
        fi

        # Repair a file written by the version that appended a newline. Without
        # this the stored password keeps being re-set to the untrimmed value on
        # every run and the mismatch never clears.
        if [ -n "$(tail -c 1 ${cfg.passwordFile} | tr -d 'A-Za-z0-9')" ]; then
          echo "trimming trailing whitespace from the stored dashboard password"
          TRIMMED=$(tr -d '[:space:]' < ${cfg.passwordFile})
          printf '%s' "$TRIMMED" > ${cfg.passwordFile}
          chmod 0600 ${cfg.passwordFile}
        fi

        # ac-user-create fails when the user already exists, which is the normal
        # case on every run after the first — so set the password instead and
        # only create when that fails.
        #
        # Neither output is discarded any more. Both were sent to /dev/null,
        # so a dashboard user that was never created looked exactly like one
        # that was, and the only symptom reached the person trying to log in.
        if SET_ERR=$(timeout 30 ceph dashboard ac-user-set-password admin \
             -i ${cfg.passwordFile} --force-password 2>&1); then
          echo "dashboard password re-applied for user admin"
        else
          echo "no existing admin user to update ($SET_ERR) — creating one"
          if CREATE_ERR=$(timeout 30 ceph dashboard ac-user-create admin \
               -i ${cfg.passwordFile} administrator --force-password 2>&1); then
            echo "dashboard user admin created"
          else
            echo "could not create the dashboard user: $CREATE_ERR" >&2
            exit 0
          fi
        fi

        # ── Prove the credentials work ───────────────────────────────────────
        #
        # Everything above can succeed and the login can still fail. That has
        # happened twice already: once because the password file ended in a
        # newline that Ceph kept and the Storage page trimmed, and once for a
        # reason no log recorded, because the errors were being discarded.
        #
        # `ac-user-show` only proves the account exists. The question that
        # matters is whether the password the Storage page displays actually
        # logs in, so ask the dashboard itself. When the answer is no, the
        # password is re-applied — which fixes any drift between the file and
        # what Ceph stored, whatever caused it.
        #
        # Only on failure, never routinely: re-applying invalidates any session
        # already open, and doing that every half hour would log the owner out
        # while they were reading a page.
        DASH_URL=$(timeout 20 ceph mgr services -f json 2>/dev/null | jq -r '.dashboard // empty')
        if [ -z "$DASH_URL" ]; then
          echo "no active mgr is serving the dashboard yet — cannot verify the login"
          exit 0
        fi

        PW=$(cat ${cfg.passwordFile})
        # The Ceph dashboard API refuses an unversioned request with 415, which
        # would otherwise read exactly like a rejected password.
        CODE=$(curl -sS -o /dev/null -w '%{http_code}' -m 15 -X POST \
          "''${DASH_URL%/}/api/auth" \
          -H 'Content-Type: application/json' \
          -H 'Accept: application/vnd.ceph.api.v1.0+json' \
          --data-binary "{\"username\":\"admin\",\"password\":\"$PW\"}" 2>/dev/null || echo 000)

        case "$CODE" in
          200|201)
            echo "dashboard login verified for user admin"
            ;;
          400|401)
            echo "the stored password does not log in (HTTP $CODE) — re-applying it" >&2
            if RESET_ERR=$(timeout 30 ceph dashboard ac-user-set-password admin \
                 -i ${cfg.passwordFile} --force-password 2>&1); then
              echo "password re-applied; it will be verified again on the next run"
            else
              echo "could not re-apply the password: $RESET_ERR" >&2
            fi
            ;;
          000)
            echo "could not reach $DASH_URL to verify the login" >&2
            ;;
          *)
            # 415 means the Accept header above is wrong for this Ceph version,
            # 404 that url_prefix and the proxy disagree. Neither is a password
            # problem and re-applying it would hide the real fault.
            echo "unexpected response $CODE from $DASH_URL while verifying the login — not a password problem" >&2
            ;;
        esac

        echo "dashboard configured on ${cephCfg.monAddr}:${toString cfg.port}${cfg.urlPrefix}"
      '';
    };

    # Re-assert periodically. A mgr failover moves the dashboard to another
    # node, and a node that has never been active has still had its config set
    # here, so the move needs nothing to happen. This is for the case where the
    # module got disabled or the config was cleared by hand.
    systemd.timers.yolab-ceph-dashboard = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "4min";
        OnUnitActiveSec = "30min";
      };
    };
  };
}
