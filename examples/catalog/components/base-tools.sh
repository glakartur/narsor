# Common tools of a task: Git over HTTPS, certificates, curl and jq.
# Runs as root during the image build, with `sh`. No version script: this
# layer is rebuilt only when the base image changes.
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl git jq
rm -rf /var/lib/apt/lists/*
