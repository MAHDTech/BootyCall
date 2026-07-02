#!/usr/bin/env bash
set -euo pipefail

# Ensure we are not on trunk
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" = "trunk" ] || [ "$BRANCH" = "HEAD" ]; then
	echo "On trunk or detached HEAD, skipping version bump check."
	exit 0
fi

# Check if convco and toml are installed
if ! command -v convco &>/dev/null; then
	echo "Warning: convco not found. Skipping auto-version bump."
	exit 0
fi

if ! command -v toml &>/dev/null; then
	echo "Warning: toml (toml-cli) not found. Skipping auto-version bump."
	exit 0
fi

# Fetch tags silently in background or fail fast if offline
git fetch --tags origin >/dev/null 2>&1 || true

# Extract version from local Cargo.toml under [workspace.package]
CURRENT_VERSION=$(sed -n '/\[workspace.package\]/,/^\[/p' Cargo.toml | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
if [ -z "$CURRENT_VERSION" ]; then
	CURRENT_VERSION=$(grep '^version =' Cargo.toml | head -n1 | cut -d'"' -f2 || true)
fi

if [ -z "$CURRENT_VERSION" ]; then
	echo "Error: Could not find version in Cargo.toml"
	exit 1
fi

# Run convco version --bump to calculate the next version based on conventional commits
NEXT_VERSION=$(convco version --bump 2>/dev/null || echo "")
NEXT_VERSION="${NEXT_VERSION#v}"

if [ -z "$NEXT_VERSION" ] || [ "$NEXT_VERSION" = "$CURRENT_VERSION" ]; then
	echo "No version bump needed (Current version: $CURRENT_VERSION)."
	exit 0
fi

# A version bump is needed! Update Cargo.toml
echo "Version bump required by conventional commits: $CURRENT_VERSION -> $NEXT_VERSION"
echo "Updating Cargo.toml..."
toml set Cargo.toml workspace.package.version "$NEXT_VERSION" >Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

# Update Cargo.lock by running cargo check
if command -v cargo &>/dev/null; then
	echo "Updating Cargo.lock..."
	cargo check --quiet 2>/dev/null || true
fi

echo "Error: Cargo.toml version was automatically bumped to $NEXT_VERSION."
echo "Please stage Cargo.toml and Cargo.lock ('git add Cargo.toml Cargo.lock') and commit again."
exit 1
