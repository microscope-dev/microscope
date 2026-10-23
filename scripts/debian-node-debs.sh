#!/usr/bin/env bash
# Prints the debianNodeDebs list for flake.nix: Debian's nodejs, libnode137,
# and libnode-dev with every package they depend on, recursively, as one
# snapshot of a suite names them. Each .deb is fetched from that snapshot,
# which keeps every version, with Debian's own pool as a second URL, and is
# pinned by the SHA256 the index records, so nothing is downloaded but the
# index.
#
#   scripts/debian-node-debs.sh 20261022T203737Z [sid] > /tmp/debs.nix
#
# Snapshots' timestamps are listed at https://snapshot.debian.org/archive/debian/.
# It runs on the host, as the dev shell has no curl: it needs curl, xz, gawk,
# and coreutils. PACKAGES=FILE reads a Packages index already decompressed
# instead of downloading one.
#
# Depends and Pre-Depends are followed; Recommends and Suggests are not. Of
# alternatives, the first the index has wins, through a package that provides
# it if none is real. Versions are not compared: one snapshot of one suite is
# consistent with itself.

set -euo pipefail

[[ $# -ge 1 && $# -le 2 ]] || { echo "usage: debian-node-debs.sh TIMESTAMP [SUITE]" >&2; exit 2; }
timestamp="$1"
suite="${2:-sid}"
snapshot="https://snapshot.debian.org/archive/debian/$timestamp"
roots="nodejs libnode137 libnode-dev"

index="${PACKAGES:-}"
if [[ -z "$index" ]]; then
    index="$(mktemp)"
    trap 'rm -f "$index"' EXIT
    curl -fsSL "$snapshot/dists/$suite/main/binary-amd64/Packages.xz" | xz -dc >"$index"
fi

# One line for each package of the closure: its Filename and its SHA256.
closure="$(gawk -v RS= -v FS='\n' -v roots="$roots" '
    {
        name = ""; depends = ""; provides = ""; file = ""; sha = ""; arch = ""
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^Package: /) name = substr($i, 10)
            else if ($i ~ /^Architecture: /) arch = substr($i, 15)
            else if ($i ~ /^(Pre-)?Depends: /) depends = depends ", " substr($i, index($i, ":") + 2)
            else if ($i ~ /^Provides: /) provides = substr($i, 11)
            else if ($i ~ /^Filename: /) file = substr($i, 11)
            else if ($i ~ /^SHA256: /) sha = substr($i, 9)
        }
        if (arch != "amd64" && arch != "all") next
        files[name] = file; hashes[name] = sha; deps[name] = depends
        n = split(provides, provided, /, */)
        for (i = 1; i <= n; i++) {
            sub(/ .*/, "", provided[i])
            if (!(provided[i] in providers)) providers[provided[i]] = name
        }
    }
    function bare(dependency) {
        sub(/^ +/, "", dependency); sub(/[ (].*/, "", dependency); sub(/:.*/, "", dependency)
        return dependency
    }
    function resolve(group,    n, alternatives, i, name) {
        n = split(group, alternatives, /\|/)
        for (i = 1; i <= n; i++) if ((name = bare(alternatives[i])) in files) return name
        for (i = 1; i <= n; i++) if ((name = bare(alternatives[i])) in providers) return providers[name]
        print "error: nothing satisfies " group > "/dev/stderr"
        failed = 1
        return ""
    }
    END {
        count = split(roots, queue, " ")
        for (i = 1; i <= count; i++) {
            if (!(queue[i] in files)) { print "error: no package " queue[i] > "/dev/stderr"; exit 1 }
            seen[queue[i]] = 1
        }
        for (head = 1; head <= count; head++) {
            n = split(deps[queue[head]], groups, /, */)
            for (i = 1; i <= n; i++) {
                if (groups[i] ~ /^ *$/) continue
                name = resolve(groups[i])
                if (name != "" && !(name in seen)) { seen[name] = 1; queue[++count] = name }
            }
        }
        if (failed) exit 1
        for (i = 1; i <= count; i++) print files[queue[i]], hashes[queue[i]]
    }
' "$index" | sort)"

echo "["
while read -r file sha; do
    hash="$(printf "$(sed 's/../\\x&/g' <<<"$sha")" | base64 -w0)"
    printf '  {\n    hash = "sha256-%s";\n    urls = [\n' "$hash"
    printf '      "%s/%s"\n' "$snapshot" "$file"
    printf '      "https://deb.debian.org/debian/%s"\n' "$file"
    printf '    ];\n  }\n'
done <<<"$closure"
echo "]"
echo "$(wc -l <<<"$closure") packages" >&2
