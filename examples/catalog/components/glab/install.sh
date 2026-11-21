# Installs the GitLab CLI from its release package, checksum verified.
# Needs curl and ca-certificates (the base-tools component).
# NARSOR_COMPONENT_VERSION is the output of version.sh.
#
# glab reads GITLAB_TOKEN (the gitlab-token secret). For `git clone` and
# `git push` over HTTPS, let the task configure Git to ask glab:
#   glab auth setup-git   (or a credential helper in the workflow)
# SSH remotes do not work in the container: there are no SSH keys.
set -eu
version="${NARSOR_COMPONENT_VERSION:?the version script gives the version}"
case "$(uname -m)" in
    x86_64|amd64) arch=amd64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
package="glab_${version}_linux_${arch}.deb"
url="https://gitlab.com/gitlab-org/cli/-/releases/v${version}/downloads"
cd /tmp
curl -fsSLO "$url/$package"
curl -fsSL "$url/checksums.txt" | grep " ${package}\$" | sha256sum -c -
dpkg -i "$package"
rm -f "$package"
