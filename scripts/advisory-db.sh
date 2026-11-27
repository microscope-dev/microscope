#!/usr/bin/env bash





set -euo pipefail

db="target/advisory-dbs/advisory-db-3157b0e258782691"
if [[ "$(cat "$db/.pinned" 2>/dev/null)" != "$MICROSCOPE_ADVISORY_DB" ]]; then
    rm -rf "$db"
    mkdir -p "$(dirname "$db")"
    cp -r --no-preserve=mode "$MICROSCOPE_ADVISORY_DB" "$db"
    date="@$MICROSCOPE_ADVISORY_DB_DATE +0000"
    git -C "$db" init --quiet
    git -C "$db" add --all
    GIT_AUTHOR_DATE="$date" GIT_COMMITTER_DATE="$date" git -C "$db" \
        -c user.name=advisory-db -c user.email=advisory-db@localhost -c core.hooksPath=/dev/null -c commit.gpgsign=false \
        commit --quiet --message "$MICROSCOPE_ADVISORY_DB"
    echo "$MICROSCOPE_ADVISORY_DB" > "$db/.pinned"
fi
echo "$db"
