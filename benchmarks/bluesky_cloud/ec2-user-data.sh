#!/bin/bash
# First boot of the live-work instance (bench-cloud.sh ec2-up fills in @...@).
# Installs Docker and the AWS CLI, pulls the benchmark image, copies the data
# to /work, and installs the same ursa natively, for interactive use.
set -euxo pipefail
dnf install -y docker
systemctl enable --now docker
usermod -aG docker ssm-user 2>/dev/null || true

mkdir -p /work/data
chmod 1777 /work
aws s3 cp --region @REGION@ --recursive s3://@BUCKET@/data/ /work/data/

aws ecr get-login-password --region @REGION@ | docker login --username AWS --password-stdin "$(echo @REPO@ | cut -d/ -f1)"
docker pull "@REPO@:ursa-@VERSION@" || echo "image @REPO@:ursa-@VERSION@ not pushed yet"

curl -fsSL https://cldixon.github.io/ursa/install.sh | URSA_VERSION=@VERSION@ URSA_INSTALL_DIR=/usr/local/bin sh

cat > /etc/motd <<'EOF'
Ursa Bluesky benchmark instance. Data: /work/data. This instance costs money
every hour: run `bench-cloud.sh ec2-down` from your machine when done.

  ursa /work/bsky.ursa                                  # interactive
  docker run --rm -v /work:/work @REPO@:ursa-@VERSION@ \
    --data /work/data --work /work/run --results s3://@BUCKET@/results/ec2-$(date -u +%Y%m%dT%H%M%SZ)
EOF
