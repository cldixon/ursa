#!/bin/bash
# Run the comparison on an EC2 instance started by ../bluesky_cloud/bench-cloud.sh ec2-up.
# That instance's first boot puts the data in /work/data and the released `ursa` on
# the PATH. This script adds the pinned Python libraries, prepares each graph size,
# runs compare.py with its limits, and uploads the results.
#
#   ec2-run.sh BUCKET RUN_ID "SIZES" [compare.py args...]
#   ec2-run.sh ursa-bluesky-bench-bucket-... cmp-96m "bsky-96m" --timeout 2700 --mem-limit-gb 110
#
# Sizes: bsky-5m (before 2023-06-01), bsky-96m (before 2024-01-01),
# bsky-983m (before 2024-11-25), bsky-full (no cutoff; Ursa engines only, no dense ids).
# Run it detached (setsid nohup ... &) so it doesn't depend on the SSM session.
set -euo pipefail
BUCKET=$1; RUN_ID=$2; SIZES=$3; shift 3
HERE=$(cd "$(dirname "$0")" && pwd)
W=/work/cmp
mkdir -p "$W"
exec > >(tee -a "$W/$RUN_ID.log") 2>&1
echo "== $(date -u +%FT%TZ) $RUN_ID: $SIZES"
ursa --version

if [ ! -x "$W/venv/bin/python" ]; then
    dnf install -y -q python3.11 >/dev/null 2>&1 || true
    python3.11 -m venv "$W/venv"
    "$W/venv/bin/pip" install -q --upgrade pip
    "$W/venv/bin/pip" install -q "ursa-graph==0.5.0" "igraph==1.0.0" "networkx==3.7" \
        "rustworkx==0.18.1" pyarrow numpy scipy
fi
"$W/venv/bin/python" -c "import ursa, igraph, networkx, rustworkx; print('ursa-graph', ursa.__version__, 'igraph', igraph.__version__, 'networkx', networkx.__version__, 'rustworkx', rustworkx.__version__)"

SRC=/work/data/deidentified_follows_edgelist.parquet
for size in $SIZES; do
    case $size in
        bsky-5m) prep=(--before 2023-06-01) ;;
        bsky-96m) prep=(--before 2024-01-01) ;;
        bsky-983m) prep=(--before 2024-11-25) ;;
        bsky-full) prep=(--no-dense) ;;
        *) echo "unknown size $size"; exit 2 ;;
    esac
    [ -f "$W/$size/meta.json" ] || "$W/venv/bin/python" "$HERE/prep.py" --source "$SRC" --out "$W/$size" "${prep[@]}"
    BENCH_HOST_LABEL="EC2 $(curl -s -m 2 -H "X-aws-ec2-metadata-token: $(curl -s -m 2 -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')" http://169.254.169.254/latest/meta-data/instance-type)" \
        "$W/venv/bin/python" "$HERE/compare.py" --graph "$W/$size" --out "$W/out/$RUN_ID/$size" \
        --python "$W/venv/bin/python" "$@" || true
    aws s3 cp --recursive --only-show-errors --exclude "work/*" "$W/out/$RUN_ID/$size" "s3://$BUCKET/results/$RUN_ID/$size/"
    cp "$W/$size/meta.json" "$W/out/$RUN_ID/$size/graph-meta.json"
    aws s3 cp --only-show-errors "$W/out/$RUN_ID/$size/graph-meta.json" "s3://$BUCKET/results/$RUN_ID/$size/graph-meta.json"
done
aws s3 cp --only-show-errors "$W/$RUN_ID.log" "s3://$BUCKET/results/$RUN_ID/run.log"
echo "== $(date -u +%FT%TZ) done"
