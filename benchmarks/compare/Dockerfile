# The comparison image: the released `ursa` binary, the ursa-graph wheel, and
# igraph, rustworkx and NetworkX at pinned versions. For Fargate and CI; on EC2,
# ec2-run.sh installs the same versions natively.
#
#   docker build --platform linux/arm64 --build-arg URSA_VERSION=0.10.0 -t ursa-compare .
#   docker run --rm -v /work:/work ursa-compare --graph /work/cmp/bsky-5m --out /work/cmp/out
FROM python:3.11-slim-bookworm

ARG URSA_VERSION=0.10.0

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL https://cldixon.github.io/ursa/install.sh \
      | URSA_VERSION="${URSA_VERSION}" URSA_INSTALL_DIR=/usr/local/bin sh \
 && ursa --version

RUN pip install --no-cache-dir "ursa-graph==0.5.0" "igraph==1.0.0" "networkx==3.7" \
      "rustworkx==0.18.1" pyarrow numpy scipy boto3

COPY prep.py cell.py compare.py /opt/compare/
WORKDIR /work
ENTRYPOINT ["python", "/opt/compare/compare.py"]
CMD ["--help"]
