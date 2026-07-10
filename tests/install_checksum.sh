#!/bin/bash

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export ZDM_INSTALLER_SOURCE_ONLY=1
source "$ROOT/install.sh"

temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT
asset_name="zdm-x86_64-unknown-linux-gnu"
asset="$temp_dir/$asset_name"
checksums="$temp_dir/SHA256SUMS"

printf 'verified payload' > "$asset"
hash=$(sha256sum "$asset")
printf '%s  %s\n' "${hash%% *}" "$asset_name" > "$checksums"
verify_checksum "$asset" "$checksums" "$asset_name"

printf 'tampered payload' > "$asset"
if (verify_checksum "$asset" "$checksums" "$asset_name" >/dev/null 2>&1); then
    echo "tampered asset unexpectedly passed checksum verification" >&2
    exit 1
fi

curl() {
    local output=""
    while (( $# )); do
        if [[ "$1" == "-o" ]]; then
            output="$2"
            shift 2
        else
            shift
        fi
    done

    if [[ "$output" == *SHA256SUMS ]]; then
        local payload_hash
        payload_hash=$(printf '#!/bin/sh\nprintf "zdm v0.4.0\\n"\n' | sha256sum)
        printf '%s  %s\n' "${payload_hash%% *}" "$asset_name" > "$output"
    else
        printf '#!/bin/sh\nprintf "zdm v0.4.0\\n"\n' > "$output"
    fi
}

# TMPDIR is environment-controlled. Ensure cleanup never reparses it as shell code.
hostile_tmp="$temp_dir/tmp;touch injected;#"
mkdir -p "$hostile_tmp"
(
    cd "$temp_dir"
    TMPDIR="$hostile_tmp" \
        ZDM_INSTALL_DIR="$temp_dir/bin" \
        ZDM_VERSION="v0.4.0" \
        install >/dev/null
)
if [[ -e "$temp_dir/injected" ]]; then
    echo "installer cleanup executed shell code from TMPDIR" >&2
    exit 1
fi
