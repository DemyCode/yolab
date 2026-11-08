#!/bin/sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/tree_remove.sh"
PASS=0
FAIL=0

tree_with() {
    TREE=$(mktemp -d)
    for p in "$@"; do
        mkdir -p "$(dirname "$TREE/$p")"
        echo x >"$TREE/$p"
    done
}

remove() {
    sh "$SCRIPT" "$TREE" "$@" || {
        echo "FAIL: the script exited $? for $*"
        FAIL=$((FAIL + 1))
    }
}

there() {
    if [ -e "$TREE/$1" ]; then
        PASS=$((PASS + 1))
    else
        echo "FAIL: $NAME: $1 was deleted"
        FAIL=$((FAIL + 1))
    fi
}

gone() {
    if [ -e "$TREE/$1" ]; then
        echo "FAIL: $NAME: $1 is still there"
        FAIL=$((FAIL + 1))
    else
        PASS=$((PASS + 1))
    fi
}

NAME="a folder another service mounts inside survives the rest being deleted"
tree_with jellyfin/config/db jellyfin/.hidden jellyfin/media/movies/film jellyfin/media/shows/episode other/file
remove 1 jellyfin jellyfin/media/movies
there jellyfin/media/movies/film
gone jellyfin/media/shows
gone jellyfin/config
gone jellyfin/.hidden
there other/file

NAME="removing the root empties the tree but what others mount"
tree_with test/file jellyfin/config/db filebrowser/state
remove 1 "" jellyfin
there .
there jellyfin/config/db
gone test
gone filebrowser

NAME="folders with spaces and nothing kept inside are deleted whole"
tree_with "my movies/film" notes/a
remove 2 "my movies" notes
gone "my movies"
gone notes

NAME="a folder another service mounts as it is stays whole"
tree_with media/film
remove 1 media media
there media/film

NAME="a link on the way to a kept folder is removed, not followed"
tree_with outside/file jellyfin/media/movies/film
ln -s "$TREE/outside" "$TREE/jellyfin/link"
remove 1 jellyfin jellyfin/link/file
there outside/file
gone jellyfin/link
gone jellyfin/media

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
