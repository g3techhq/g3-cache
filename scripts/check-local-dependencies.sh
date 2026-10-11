#!/bin/sh
# Inspect the staged lockfile, not the working copy.
set -eu
shared='^(g3-auth(-macros)?|g3-cache(-macros)?|g3-native-plugins|g3-route-transitions|g3-ui)$'
staged=$(git diff --cached --name-only --diff-filter=ACMR)
if printf '%s\n' "$staged" | grep -qx 'Cargo.lock'; then
    # Packages defined inside this repository legitimately have no source.
    owned=$(
        git ls-files --cached -- 'Cargo.toml' '*/Cargo.toml' |
        while IFS= read -r manifest; do
            git show ":$manifest" | awk '
                /^\[package\]/ { package = 1; next }
                /^\[/ { package = 0 }
                package && /^name[[:space:]]*=/ {
                    split($0, fields, "\""); print fields[2]
                }
            '
        done | sort -u | tr '\n' '|'
    )
    git show :Cargo.lock | awk -v shared="$shared" -v owned="$owned" '
        function check() {
            if (name ~ shared && !source && index("|" owned, "|" name "|") == 0) {
                print "Commit blocked: Cargo.lock uses local " name "." > "/dev/stderr"
                failed = 1
            }
        }
        /^\[\[package\]\]/ { check(); name = ""; source = 0; next }
        /^name[[:space:]]*=/ { split($0, fields, "\""); name = fields[2] }
        /^source[[:space:]]*=/ { source = 1 }
        END { check(); exit failed }
    '
fi
if printf '%s\n' "$staged" | grep -qx '.cargo/config.toml'; then
    if git show :.cargo/config.toml | grep -Eq '^[[:space:]]*g3-(auth|cache|native-plugins|route-transitions|ui)[[:space:]]*='; then
        echo "Commit blocked: shared-library overrides belong outside the repository." >&2
        exit 1
    fi
fi
