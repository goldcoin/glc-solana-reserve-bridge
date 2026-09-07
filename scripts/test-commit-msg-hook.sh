#!/usr/bin/env bash
# Self-test for .githooks/commit-msg.
#
# A commit-msg hook is easy to get subtly wrong in the direction that
# matters least visibly: too broad, and it starts rejecting legitimate
# human co-authorship; too narrow, and the AI trailer it exists to stop
# sails straight through. Neither failure announces itself — the first
# looks like "git is being annoying", the second looks like nothing at
# all until an AI account turns up in the Contributors panel. So the
# hook gets a test.
#
# Two layers:
#
#   1. A pattern matrix run directly against the hook script, covering
#      both what must be ALLOWED and what must be REJECTED. The allow
#      cases are the important half — they are what stops a future
#      broadening of the patterns from silently eating real co-authors.
#   2. An end-to-end test in a throwaway repository, proving the hook is
#      actually wired up by core.hooksPath and that git really does
#      refuse the commit rather than merely printing something.
#
# The throwaway repo is created under a mktemp directory and removed on
# exit; this script never commits to, or otherwise touches, the
# repository it lives in.
#
# Usage:
#   scripts/test-commit-msg-hook.sh
#
# Exits 0 if every case behaved; non-zero, listing each failure, if not.

set -euo pipefail

repo_root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
hook="$repo_root/.githooks/commit-msg"

if [ ! -x "$hook" ]; then
    echo "error: $hook is missing or not executable" >&2
    exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

# --- layer 1: pattern matrix -------------------------------------------------

# check_allow/check_reject take a case name and the message on stdin.
check_allow() {
    local name="$1" file="$tmp/msg"
    cat > "$file"
    if "$hook" "$file" >/dev/null 2>&1; then pass "allow: $name"; else fail "allow: $name (was rejected)"; fi
}
check_reject() {
    local name="$1" file="$tmp/msg"
    cat > "$file"
    if "$hook" "$file" >/dev/null 2>&1; then fail "reject: $name (was allowed)"; else pass "reject: $name"; fi
}

echo "pattern matrix:"

check_allow "plain message" <<'EOF'
feat: a normal commit
EOF

check_allow "single human co-author" <<'EOF'
feat: x

Co-authored-by: Jane Doe <jane@example.com>
EOF

check_allow "several humans plus sign-off" <<'EOF'
feat: x

Co-authored-by: Jane Doe <jane@example.com>
Co-Authored-By: Bob Lee <bob@corp.io>
Signed-off-by: Someone <someone@example.com>
EOF

# The string appears in this repo's real history (2c91c466's body) as
# ordinary prose. Matching bare words rather than the trailer forms
# would reject it.
check_allow "the word claude in prose" <<'EOF'
feat: x

Removes the claude-sha.txt debris left by the old build.
EOF

# Git strips comment lines before creating the commit, so they are not
# part of the message and must not be scanned.
check_allow "trailer inside a comment line" <<'EOF'
feat: x

# Co-authored-by: Claude Fable 5 <noreply@anthropic.com>
# Claude-Session: https://claude.ai/code/session_01ABC
EOF

# Everything below the scissors line is the `git commit -v` diff, not
# the message. Without this exclusion the hook could not be committed,
# nor could its own tests or documentation.
check_allow "patterns below the -v scissors line" <<'EOF'
chore: add the hook

# ------------------------ >8 ------------------------
diff --git a/.githooks/commit-msg b/.githooks/commit-msg
+Co-authored-by: Claude Fable 5 <noreply@anthropic.com>
+Claude-Session: https://claude.ai/x
+AI-generated
EOF

check_reject "Claude co-author trailer" <<'EOF'
feat: x

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
EOF

check_reject "lowercase trailer key" <<'EOF'
feat: x

co-authored-by: claude sonnet 5 <someone@example.com>
EOF

# The address alone is enough: GitHub resolves it to the `claude`
# account regardless of the display name in front of it.
check_reject "Anthropic address behind another name" <<'EOF'
feat: x

Co-authored-by: Somebody Else <noreply@anthropic.com>
EOF

check_reject "Anthropic as co-author name" <<'EOF'
feat: x

Co-authored-by: Anthropic PBC <team@example.com>
EOF

check_reject "session-link trailer" <<'EOF'
feat: x

Claude-Session: https://claude.ai/code/session_01ABC
EOF

check_reject "generated-by trailer" <<'EOF'
feat: x

Generated-by: Claude Code
EOF

check_reject "AI-generated marker" <<'EOF'
feat: x

AI-generated commit.
EOF

check_reject "AI trailer mixed with a human co-author" <<'EOF'
feat: x

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Co-authored-by: Jane Doe <jane@example.com>
EOF

# --- layer 1b: the hook must never edit the message --------------------------
#
# Rejecting is the whole contract. If the hook ever started stripping
# instead, a mixed message like the one above would lose its AI trailer
# AND risk losing the human one, silently.
mixed="$tmp/mixed"
cat > "$mixed" <<'EOF'
feat: x

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Co-authored-by: Jane Doe <jane@example.com>
EOF
cp "$mixed" "$tmp/mixed.orig"
"$hook" "$mixed" >/dev/null 2>&1 || true
if cmp -s "$tmp/mixed.orig" "$mixed"; then
    pass "reject-only: message file left byte-for-byte unchanged"
else
    fail "reject-only: hook MODIFIED the message file"
fi

# --- layer 2: end-to-end through git -----------------------------------------

echo "end-to-end (throwaway repository):"

sandbox="$tmp/sandbox"
mkdir -p "$sandbox"
git -C "$sandbox" init -q .
git -C "$sandbox" config user.name "hook test"
git -C "$sandbox" config user.email "hook-test@example.invalid"
git -C "$sandbox" config core.hooksPath "$repo_root/.githooks"
echo content > "$sandbox/file.txt"
git -C "$sandbox" add file.txt

if git -C "$sandbox" commit -q -m "feat: x

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01ABC" >/dev/null 2>&1; then
    fail "prohibited attribution commit was CREATED"
else
    pass "prohibited attribution commit rejected"
fi

count="$(git -C "$sandbox" rev-list --count --all 2>/dev/null || echo 0)"
if [ "$count" = "0" ]; then
    pass "no commit object was created by the rejected attempt"
else
    fail "expected 0 commits after rejection, found $count"
fi

if git -C "$sandbox" commit -q -m "feat: x

Co-authored-by: Jane Doe <jane@example.com>" >/dev/null 2>&1; then
    pass "normal human co-author commit accepted"
else
    fail "normal human co-author commit was BLOCKED"
fi

if git -C "$sandbox" log -1 --format='%B' 2>/dev/null | grep -qx 'Co-authored-by: Jane Doe <jane@example.com>'; then
    pass "human co-author trailer preserved byte-for-byte in the commit"
else
    fail "human co-author trailer was altered or dropped"
fi

echo
if [ "$failures" -ne 0 ]; then
    echo "FAILED: $failures case(s)."
    exit 1
fi
echo "All cases passed."
