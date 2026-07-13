#!/usr/bin/env bash
#
# Refresh the vendored BIDS schema and metaschema from their pinned upstream commits.
#
# bidslake embeds two committed, in-tree inputs so that builds stay fully offline:
#   - the compiled BIDS schema      (bids-standard/bids-schema)
#   - the BIDS metaschema           (bids-standard/bids-specification)
# They live in *different* upstream repos and are maintained separately, so each is
# pinned by its own `.pinned-commit`. This script re-fetches both files at those pins.
# Run it only to update the vendored copies (e.g. after bumping a pin); normal builds
# never touch the network.
#
# To bump a pin: edit the relevant `.pinned-commit`, then re-run this script and commit.
#
# Requires the GitHub CLI (`gh`), authenticated.
set -euo pipefail
cd "$(dirname "$0")/.."

# Keep in sync with SCHEMA_VERSION_DIR in crates/bids-schema/build.rs.
SCHEMA_VERSION_DIR=1.11.1

schema_pin=$(tr -d '[:space:]' < third_party/bids-schema/.pinned-commit)
meta_pin=$(tr -d '[:space:]' < third_party/bids-specification/.pinned-commit)

echo "schema     pin: ${schema_pin}  (bids-standard/bids-schema)"
echo "metaschema pin: ${meta_pin}  (bids-standard/bids-specification)"

# Fetch raw file content at a specific commit. repo=$1 path=$2 ref=$3 out=$4
fetch() {
  gh api -H "Accept: application/vnd.github.raw" "repos/$1/contents/$2?ref=$3" > "$4"
}

fetch bids-standard/bids-schema \
  "versions/${SCHEMA_VERSION_DIR}/schema.json" "${schema_pin}" \
  "third_party/bids-schema/versions/${SCHEMA_VERSION_DIR}/schema.json"

fetch bids-standard/bids-specification \
  "src/metaschema.json" "${meta_pin}" \
  "third_party/bids-specification/src/metaschema.json"

echo "Refreshed. Review the diff and commit."
