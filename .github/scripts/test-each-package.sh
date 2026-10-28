#!/usr/bin/env bash
set -euo pipefail

cargo_bin="${CARGO:-cargo}"
metadata="$("$cargo_bin" metadata --no-deps --format-version 1)"
package_list="$(
    jq -r '. as $metadata | $metadata.packages[] | select(.id as $id | $metadata.workspace_members | index($id)) | .name' \
        <<<"$metadata" | sort
)"
if [[ -z "$package_list" ]]; then
    echo "cargo metadata listed no workspace packages" >&2
    exit 1
fi
mapfile -t packages <<<"$package_list"

failed=()
for package in "${packages[@]}"; do
    echo "::group::${package}"
    if "$cargo_bin" test --locked -p "$package"; then
        echo "::endgroup::"
    else
        echo "::endgroup::"
        failed+=("$package")
    fi
done

if ((${#failed[@]} > 0)); then
    printf 'Failed packages: %s\n' "${failed[*]}" >&2
    exit 1
fi

printf 'All %d workspace packages passed.\n' "${#packages[@]}"
