#!/usr/bin/env bash
set -euo pipefail


# Function to resolve the script path
get_script_dir() {
    local SOURCE=$0
    while [ -h "$SOURCE" ]; do # Resolve $SOURCE until the file is no longer a symlink
        DIR=$(cd -P "$(dirname "$SOURCE")" && pwd)
        SOURCE=$(readlink "$SOURCE")
        [[ $SOURCE != /* ]] && SOURCE=$DIR/$SOURCE # If $SOURCE was a relative symlink, resolve it relative to the symlink base directory
    done
    DIR=$(cd -P "$(dirname "$SOURCE")" && pwd)
    echo "$DIR"
}

# Get the directory
SCRIPT_DIR=$(get_script_dir)
SYNC_SCRIPT="${SCRIPT_DIR}/../../scripts/get-git-dir.sh"
echo "The script is located in: $SCRIPT_DIR"

"$SYNC_SCRIPT" "https://github.com/apache/fory-site.git" "docs" "${SCRIPT_DIR}/fory-docs" "main"

"$SYNC_SCRIPT" "https://github.com/apache/fory.git" "compiler" "${SCRIPT_DIR}/fory-compiler" "main"

# "$SYNC_SCRIPT" "https://github.com/n0-computer/docs.iroh.computer.git"