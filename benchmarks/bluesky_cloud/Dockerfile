# The Bluesky benchmark image: a released `ursa` binary and run.py.
#
# The same image runs on an EC2 instance (docker run) and as a Fargate task. It
# installs `ursa` with the public install script, which checks the download's
# SHA-256, so the image tests exactly what users install. Build it for the
# machine it runs on: linux/arm64 for Graviton (the default in README.md).
#
#   docker build --build-arg URSA_VERSION=0.9.0 -t ursa-bluesky-bench .
#   docker run --rm ursa-bluesky-bench --help
#
# Debian 12's glibc is 2.36; the prebuilt Linux binaries need 2.35 or later.
FROM debian:bookworm-slim

ARG URSA_VERSION=0.9.0

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl python3 python3-boto3 \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://cldixon.github.io/ursa/install.sh \
      | URSA_VERSION="${URSA_VERSION}" URSA_INSTALL_DIR=/usr/local/bin sh \
 && ursa --version

COPY run.py smoke_data.py /opt/bench/

# Data, the database and its stored indexes go here: put it on the large disk
# (Fargate's ephemeral storage, or an EBS volume mounted at /work on EC2).
WORKDIR /work
ENTRYPOINT ["python3", "/opt/bench/run.py"]
CMD ["--help"]
