#!/usr/bin/env bash
# Install this repository's versioned git hooks (.githooks/).
#
# Git does not track .git/hooks, so a hook committed to the repo does
# nothing until each clone opts in. Rather than copying files into
# .git/hooks — which silently goes stale the moment the tracked hook
# changes — this points core.hooksPath at the tracked directory, so
# every developer runs whatever .githooks/ currently holds.
#
# The path is set RELATIVE ('.githooks', not an absolute path) on
# purpose: git resolves a relative core.hooksPath against the root of
# the working tree the hook is running in, so a single setting works
# correctly across every `git worktree` of this repo, each using its own
# checked-out copy of the hooks.
#
# Usage:
#   scripts/install-git-hooks.sh
#
# Idempotent. Safe to re-run. Refuses, rather than clobbering, if
# core.hooksPath is already pointing somewhere else.

set -euo pipefail

repo_root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$repo_root"

hooks_dir=".githooks"

if [ ! -d "$hooks_dir" ]; then
    echo "error: $hooks_dir does not exist in $repo_root" >&2
    exit 1
fi

current="$(git config --local --get core.hooksPath || true)"
if [ -n "$current" ] && [ "$current" != "$hooks_dir" ]; then
    cat >&2 <<ERR_EOF
error: core.hooksPath is already set to '$current', not '$hooks_dir'.

Refusing to overwrite it — some other tooling may depend on it. Either
point that directory at $hooks_dir yourself, or clear the setting and
re-run this script:

    git config --local --unset core.hooksPath
    scripts/install-git-hooks.sh
ERR_EOF
    exit 1
fi

# Executability is a property of the file mode git tracks, but a clone
# made with a restrictive umask, or a checkout onto a filesystem that
# drops the bit, will leave the hook non-executable and git will then
# skip it WITHOUT saying anything. Fix it here rather than let the hook
# fail open.
for hook in "$hooks_dir"/*; do
    [ -f "$hook" ] || continue
    if [ ! -x "$hook" ]; then
        chmod +x "$hook"
        echo "made executable: $hook"
    fi
done

git config --local core.hooksPath "$hooks_dir"

echo "Installed: core.hooksPath = $hooks_dir"
echo
echo "Active hooks:"
for hook in "$hooks_dir"/*; do
    [ -f "$hook" ] || continue
    echo "  $(basename "$hook")"
done
echo
echo "To uninstall: git config --local --unset core.hooksPath"
