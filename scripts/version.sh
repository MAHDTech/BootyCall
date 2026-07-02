#!/usr/bin/env bash
set -euo pipefail

# Ensure we have the latest tags
git fetch --tags origin >/dev/null 2>&1 || true

CURRENT_VERSION=$(sed -n '/\[workspace.package\]/,/^\[/p' Cargo.toml | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
if [ -z "$CURRENT_VERSION" ]; then
	CURRENT_VERSION=$(grep '^version =' Cargo.toml | head -n1 | cut -d'"' -f2 || true)
fi

if [ -z "$CURRENT_VERSION" ]; then
	echo "Error: Could not find current version in Cargo.toml"
	exit 1
fi

BUMP_ARG=""
if [ $# -gt 0 ]; then
	ARG_UPPER=$(echo "$1" | tr '[:lower:]' '[:upper:]')
	case "$ARG_UPPER" in
	MAJOR)
		BUMP_ARG="--major"
		;;
	MINOR)
		BUMP_ARG="--minor"
		;;
	PATCH)
		BUMP_ARG="--patch"
		;;
	*)
		echo "Error: Invalid argument '$1'. Expected MAJOR, MINOR, or PATCH."
		exit 1
		;;
	esac
fi

# Run convco version --bump with optional argument
NEXT_VERSION=$(convco version --bump $BUMP_ARG 2>/dev/null || echo "")
NEXT_VERSION="${NEXT_VERSION#v}"

if [ -z "$NEXT_VERSION" ] || [ "$NEXT_VERSION" = "$CURRENT_VERSION" ]; then
	echo "No version bump needed (Current version: $CURRENT_VERSION)."
	exit 0
fi

echo "Bumping version: $CURRENT_VERSION -> $NEXT_VERSION"
echo "Updating Cargo.toml..."
toml set Cargo.toml workspace.package.version "$NEXT_VERSION" >Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

if command -v cargo &>/dev/null; then
	echo "Updating Cargo.lock..."
	cargo check --quiet 2>/dev/null || true
fi

echo "Version successfully bumped to $NEXT_VERSION."
echo "Please review changes, stage them ('git add Cargo.toml Cargo.lock'), and commit."
