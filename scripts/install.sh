#!/bin/bash
set -euo pipefail

REPO="uintptr/coin"
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"
TMP_DIR=""

cleanup() {
    if [ -n "${TMP_DIR}" ] && [ -d "${TMP_DIR}" ]; then
        rm -rf "${TMP_DIR}"
    fi
}
trap cleanup EXIT INT TERM

# Detect the release asset name for this platform.
# Releases ship x86_64 and arm64 musl builds for Linux, arm64 for macOS.
detect_asset() {
    local os

    case "$(uname -s)" in
        Linux*)  os="linux" ;;
        Darwin*) os="darwin" ;;
        *)
            echo "Error: Unsupported operating system: $(uname -s)" >&2
            exit 1
            ;;
    esac

    if [ "${os}" = "darwin" ]; then
        case "$(uname -m)" in
            arm64|aarch64) ;;
            *)
                echo "Error: Unsupported architecture for macOS: $(uname -m)" >&2
                exit 1
                ;;
        esac

        echo "coin-macos"
        return
    fi

    case "$(uname -m)" in
        x86_64|amd64)  echo "coin-linux" ;;
        arm64|aarch64) echo "coin-linux-arm64" ;;
        *)
            echo "Error: Unsupported architecture for Linux: $(uname -m)" >&2
            exit 1
            ;;
    esac
}

# Get latest release tag from GitHub.
#
# Failures are swallowed so the caller can report them in context. Without the
# `|| true`, `set -e` kills the script on a bare `curl: (22)` and the message
# below never prints, which is exactly backwards: the two common failures here
# are an unreleased repository and an anonymous API rate limit, and neither is
# self-explanatory from a 403 or a 404.
get_latest_version() {
    local response
    response=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null || true)
    echo "${response}" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || true
}

main() {
    local asset version coin_url

    echo "Installing coin..."

    asset=$(detect_asset)
    echo "Detected asset: ${asset}"

    version=$(get_latest_version)
    if [ -z "$version" ]; then
        echo "Error: Could not determine the latest version of ${REPO}." >&2
        echo "Either it has no published release yet, or the GitHub API rate" >&2
        echo "limit for unauthenticated requests has been reached. Check:" >&2
        echo "  https://github.com/${REPO}/releases" >&2
        exit 1
    fi
    echo "Latest version: ${version}"

    coin_url="https://github.com/${REPO}/releases/download/${version}/${asset}"

    # Create install directory if it doesn't exist
    mkdir -p "${INSTALL_DIR}"

    # Download binary
    TMP_DIR=$(mktemp -d)

    echo "Downloading coin from ${coin_url}..."
    if ! curl -fsSL -o "${TMP_DIR}/coin" "${coin_url}"; then
        echo "Error: Failed to download coin" >&2
        exit 1
    fi

    # Install binary
    chmod +x "${TMP_DIR}/coin"
    mv "${TMP_DIR}/coin" "${INSTALL_DIR}/coin"

    echo "Successfully installed coin to ${INSTALL_DIR}"

    # Check if install dir is in PATH
    if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
        echo ""
        echo "Note: ${INSTALL_DIR} is not in your PATH."
        echo "Add it by running:"
        echo "  echo 'export PATH=\"\${HOME}/.local/bin:\${PATH}\"' >> ~/.bashrc"
        echo "  source ~/.bashrc"
    fi

    # coin drives opencode as a child process and inherits its provider
    # credentials, so it cannot do anything useful on its own. Say so here
    # rather than letting the first debate fail with a launch error.
    if ! command -v opencode >/dev/null 2>&1; then
        echo ""
        echo "Note: coin runs debates through opencode, which is not on your PATH."
        echo "Install it and sign in to a provider before running a debate:"
        echo "  curl -fsSL https://opencode.ai/install | bash"
        echo "  opencode auth login"
    fi
}

main
