#!/usr/bin/env bash
# Installs one release on the instance. Runs as root, sent by CD through SSM (no SSH involved).
#
#   remote-deploy.sh <download-url> <sha256-of-tarball> <commit-sha>
#
# Downloads the tarball, checks its hash, unpacks it into releases/<commit>, checks the binaries,
# swaps the `current` symlink atomically and restarts the live server.
set -euo pipefail

url=${1:?download url}
want=${2:?sha256}
sha=${3:?commit sha}
root=${LOBSTER_ROOT:-/opt/lobster}
health=${HEALTH_URL:-http://127.0.0.1:8080/health}
bins="stress dispatch latency replay pipeline pipeline_latency lobster-server"

die() { echo "deploy failed: $*" >&2; exit 1; }
[[ $sha =~ ^[0-9a-f]{40}$ ]] || die "bad commit sha"
[[ $want =~ ^[0-9a-f]{64}$ ]] || die "bad sha256"

rel=$root/releases/$sha
tmp=$(mktemp -d)
trap 'rm -rf "$tmp" "$rel.partial"' EXIT

curl -fsS --retry 3 -m 120 -o "$tmp/release.tgz" "$url" || die "download"
echo "$want  $tmp/release.tgz" | sha256sum -c - >/dev/null || die "tarball hash mismatch"

mkdir -p "$root/releases"
rm -rf "$rel.partial"
mkdir -p "$rel.partial"
tar -xzf "$tmp/release.tgz" --no-same-owner -C "$rel.partial"
chmod 755 "$rel.partial" "$rel.partial"/*

[ "$(cat "$rel.partial/COMMIT")" = "$sha" ] || die "COMMIT file does not match"
for b in $bins; do
  [ -x "$rel.partial/$b" ] || die "missing binary $b"
  out=$(ldd "$rel.partial/$b" 2>&1 || true)
  if grep -q "not found" <<<"$out"; then die "missing library for $b"; fi
done

rm -rf "$rel"
mv -T "$rel.partial" "$rel"
ln -sfn "$rel" "$root/current.new"
mv -Tf "$root/current.new" "$root/current"
(cd "$root/releases" && ls -1t | tail -n +6 | xargs -r rm -rf)
echo "deployed $sha"

# Restart the live server on the new binary. Skipped when it is not installed yet (first deploy)
# or a benchmark is running (run-bench.sh starts the server again itself).
if ! systemctl cat lobster-server >/dev/null 2>&1; then
  echo "lobster-server is not installed yet: run setup-ec2.sh once"; exit 0
fi
if [ -e "$root/bench.lock" ]; then
  echo "benchmark running: leaving the live server alone"; exit 0
fi
systemctl restart lobster-server
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if curl -fsS -m 2 "$health" >/dev/null; then
    echo "live server healthy on $sha"; exit 0
  fi
  sleep 1
done
journalctl -u lobster-server -n 30 --no-pager || true
die "live server did not become healthy"
