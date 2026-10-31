#!/usr/bin/env bash
# Publish one already-rendered recipe to the shared tap. Never run code from the tap.
set -euo pipefail
[ "$#" -eq 3 ] || { echo 'usage: publish-homebrew.sh VERSION RECIPE RENDERED_FILE' >&2; exit 2; }
version=$1 recipe=$2 rendered=$3
case "$recipe" in Casks/corti.rb|Formula/vagus.rb) ;; *) echo 'invalid recipe path' >&2; exit 2;; esac
[ -s "$rendered" ] || { echo 'missing rendered recipe' >&2; exit 2; }
[ ! -L "$rendered" ] || { echo 'rendered recipe is a symlink' >&2; exit 2; }
# Stable, canonical semver only; no prerelease/build metadata or shell/git metacharacters.
valid='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
[[ "$version" =~ $valid ]] || { echo 'invalid stable version' >&2; exit 2; }
# Ignore runner-global filters/hooks while materializing untrusted tap files.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
remote=${TAP_REMOTE:-https://github.com/vasovagal/homebrew-tap.git}
branch=${TAP_BRANCH:-main}
# The workflow passes only the fixed GitHub URL. A local remote override is for offline tests.
if [ -n "${TAP_TOKEN:-}" ]; then
  [ "$remote" = https://github.com/vasovagal/homebrew-tap.git ] || { echo 'unexpected authenticated remote' >&2; exit 2; }
  auth=$(printf 'x-access-token:%s' "$TAP_TOKEN" | base64 | tr -d '\n')
  export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.https://github.com/vasovagal/homebrew-tap.git.extraheader
  export GIT_CONFIG_VALUE_0="AUTHORIZATION: basic $auth"
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$work" init -q
git -C "$work" remote add origin "$remote"
git -C "$work" config core.hooksPath /dev/null
git -C "$work" config user.name 'homebrew-publisher[bot]'
git -C "$work" config user.email 'homebrew-publisher[bot]@users.noreply.github.com'
# Fetch the latest base on every attempt; a non-fast-forward push must not drop the other product's commit.
for attempt in 1 2 3 4 5; do
  git -C "$work" fetch -q origin "$branch"
  git -C "$work" reset -q --hard FETCH_HEAD
  [ -f "$work/$recipe" ] && [ ! -L "$work/$recipe" ] && [ ! -L "$work/${recipe%/*}" ] || {
    echo "tap recipe missing or symlinked: $recipe" >&2; exit 1;
  }
  current=$(python3 - "$work/$recipe" "$recipe" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
pattern = r'^  version "([^"]+)"$' if sys.argv[2].startswith('Casks/') else r'^  version "([^"]+)"$'
found = re.findall(pattern, text, re.M)
if len(found) != 1:
    sys.exit('missing or ambiguous tap version')
print(found[0])
PY
  )
  # Numeric tuples (not lexicographic strings); reject noncanonical existing versions rather than guess.
  decision=$(python3 - "$current" "$version" <<'PY'
import re, sys
pattern = r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'
a, b = sys.argv[1:]
if not re.fullmatch(pattern, a):
    sys.exit('invalid existing tap version')
a, b = tuple(map(int, a.split('.'))), tuple(map(int, b.split('.')))
print('older' if b < a else 'equal' if b == a else 'newer')
PY
  )
  [ "$decision" != older ] || { echo "refusing downgrade: $current -> $version" >&2; exit 1; }
  if [ "$decision" = equal ]; then
    if cmp -s "$rendered" "$work/$recipe"; then echo "already published: $version"; exit 0; fi
    echo "refusing to replace differing recipe at same version: $version" >&2; exit 1
  fi
  rendered_version=$(python3 - "$rendered" <<'PY'
import pathlib, re, sys
found = re.findall(r'^  version "([^"]+)"$', pathlib.Path(sys.argv[1]).read_text(), re.M)
if len(found) != 1:
    sys.exit('missing or ambiguous rendered version')
print(found[0])
PY
  )
  [ "$rendered_version" = "$version" ] || { echo 'rendered version does not match tag' >&2; exit 1; }
  cp "$rendered" "$work/$recipe"
  git -C "$work" add -- "$recipe"
  [ "$(git -C "$work" diff --cached --name-only)" = "$recipe" ] || { echo 'unexpected staged changes' >&2; exit 1; }
  [ -z "$(git -C "$work" status --porcelain --untracked-files=all | grep -v "^M  $recipe$" || true)" ] || { echo 'unexpected tap changes' >&2; exit 1; }
  git -C "$work" diff --cached --check
  git -C "$work" commit -qm "${recipe##*/} bumped to $version"
  if git -C "$work" push origin "HEAD:refs/heads/$branch"; then
    echo "published $recipe $version"
    exit 0
  fi
  echo "push failed (attempt $attempt/5); fetching latest tap" >&2
  sleep "$attempt"
done
echo 'tap push failed after 5 attempts' >&2
exit 1
