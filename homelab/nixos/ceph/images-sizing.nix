# The image RBD's size, as a shell fragment.
#
# In its own file so the CI check can drive it with stubbed `ceph` output. It
# is arithmetic that decides how much of the cluster one node's container store
# may eat, and getting it wrong walks every machine into full-ratio, which is
# not something to discover on hardware.
#
# Sets HOSTS, TOTAL_MB, REPLICAS, USABLE_MB and WANT_MB.
# Requires ceph, jq, awk and coreutils on PATH.
{
  poolName,
  shareOfPool,
  minSizeGb,
}: ''
  # Every machine sizes its own image store out of the SAME pool, so a fixed
  # per-node share is a promise the pool cannot keep: at four machines, 25%
  # each is the entire pool and there is nothing left for app data. The
  # comment on shareOfPool described exactly this failure and then hardcoded
  # the constant that causes it.
  HOSTS=$(timeout 30 ceph osd tree -f json 2>/dev/null \
    | jq '[.nodes[] | select(.type == "host" and (.children | length) > 0)] | length' 2>/dev/null \
    || echo "")
  case "''${HOSTS:-}" in
    ''' | *[!0-9]* | 0) HOSTS=1 ;;
  esac

  TOTAL_MB=$(timeout 30 ceph df -f json | jq -r '.stats.total_bytes / 1048576 | floor')
  case "''${TOTAL_MB:-}" in
    ''' | *[!0-9]* ) echo "could not read pool capacity — not sizing anything"; exit 0 ;;
  esac

  # total_bytes is RAW. Every logical MB of this image costs REPLICAS raw MB,
  # so the share and the ceiling below are both divided by it — otherwise
  # raising the pool to two copies silently doubles what the image consumes
  # and walks the cluster into full-ratio, which blocks writes for every app
  # on every machine rather than just image pulls.
  REPLICAS=$(timeout 20 ceph osd pool get ${poolName} size -f json 2>/dev/null \
    | jq -r '.size' 2>/dev/null || echo "")
  case "''${REPLICAS:-}" in
    ''' | *[!0-9]* | 0) REPLICAS=1 ;;
  esac
  USABLE_MB=$(( TOTAL_MB / REPLICAS ))

  WANT_MB=$(awk -v t="$USABLE_MB" -v s=${toString shareOfPool} 'BEGIN{printf "%d", t*s}')
  MIN_MB=$(( ${toString minSizeGb} * 1024 ))
  if [ "$WANT_MB" -lt "$MIN_MB" ]; then WANT_MB=$MIN_MB; fi

  # The ceiling, applied LAST so it also beats the minimum. Exceeding it is
  # the failure this whole file warns about: images fill the pool, Ceph hits
  # full-ratio, and writes block for every app on every machine. A too-small
  # image store only costs re-pulling images, so when the two limits conflict
  # this one has to win.
  CAP_MB=$(( USABLE_MB / (HOSTS * 2) ))
  if [ "$WANT_MB" -gt "$CAP_MB" ]; then
    echo "capping the image store at ''${CAP_MB}MB: ''${HOSTS} machine(s) share this pool at ''${REPLICAS} cop(ies) and images may claim at most half of it"
    WANT_MB=$CAP_MB
  fi
  if [ "$WANT_MB" -lt "$MIN_MB" ]; then
    echo "warning: ''${WANT_MB}MB is below the ''${MIN_MB}MB floor — this pool is small for ''${HOSTS} machine(s); add a disk"
  fi
''
