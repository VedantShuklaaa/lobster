#!/usr/bin/env bash
# Run on the instance: ./run-bench.sh  (from /opt/lobster/current)
# Saves output with the environment recorded next to it.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

commit=$(cat COMMIT)
out=/opt/lobster/results/${commit:0:7}-$(date -u +%Y%m%dT%H%M%SZ).txt
mkdir -p /opt/lobster/results

{
  echo "# commit   $commit"
  echo "# instance $(curl -s -m 2 -H "X-aws-ec2-metadata-token: $(curl -s -m 2 -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')" http://169.254.169.254/latest/meta-data/instance-type || echo unknown)"
  echo "# cpu      $(lscpu | sed -n 's/^Model name: *//p')"
  echo "# kernel   $(uname -r)"
  echo
  for b in stress dispatch; do
    echo "## $b"
    ./"$b"
    echo
  done
} | tee "$out"