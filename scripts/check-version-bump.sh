#!/usr/bin/env bash
set -euo pipefail

# 1. Get current branch
BRANCH=$(git rev-parse --abbrev-ref HEAD)

if [ "$BRANCH" = "trunk" ] || [ "$BRANCH" = "HEAD" ]; then
	echo "On trunk or detached HEAD, skipping version bump check."
	exit 0
fi

# 2. Extract version from local Cargo.toml
# Find version under [workspace.package]
CURRENT_VERSION=$(sed -n '/\[workspace.package\]/,/^\[/p' Cargo.toml | grep '^version =' | head -n1 | cut -d'"' -f2 || true)

if [ -z "$CURRENT_VERSION" ]; then
	# Fallback to standard version = "..."
	CURRENT_VERSION=$(grep '^version =' Cargo.toml | head -n1 | cut -d'"' -f2 || true)
fi

if [ -z "$CURRENT_VERSION" ]; then
	echo "Error: Could not find version in Cargo.toml"
	exit 1
fi

# 3. Determine target branch to compare against
DEFAULT_BRANCH=$(gh repo view --json defaultBranchRef --jq .defaultBranchRef.name 2>/dev/null || true)
if [ -z "$DEFAULT_BRANCH" ]; then
	DEFAULT_BRANCH=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || true)
fi
if [ -z "$DEFAULT_BRANCH" ]; then
	for b in trunk main master; do
		if git rev-parse --verify "origin/$b" &>/dev/null || git rev-parse --verify "$b" &>/dev/null; then
			DEFAULT_BRANCH="$b"
			break
		fi
	done
fi

if [ -z "$DEFAULT_BRANCH" ]; then
	echo "Could not determine default branch. Skipping version bump check."
	exit 0
fi

TARGET="origin/$DEFAULT_BRANCH"
if ! git rev-parse --verify "$TARGET" &>/dev/null; then
	TARGET="$DEFAULT_BRANCH"
	if ! git rev-parse --verify "$TARGET" &>/dev/null; then
		echo "Could not find branch $DEFAULT_BRANCH locally or on remote. Skipping version bump check."
		exit 0
	fi
fi

# 4. Extract version from target Cargo.toml
TARGET_CARGO=$(git show "$TARGET:Cargo.toml" 2>/dev/null || true)
if [ -z "$TARGET_CARGO" ]; then
	echo "Warning: Could not read Cargo.toml from $TARGET. Skipping check."
	exit 0
fi

BASE_VERSION=$(echo "$TARGET_CARGO" | sed -n '/\[workspace.package\]/,/^\[/p' | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
if [ -z "$BASE_VERSION" ]; then
	BASE_VERSION=$(echo "$TARGET_CARGO" | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
fi

if [ -z "$BASE_VERSION" ]; then
	echo "Warning: Could not find version in $TARGET:Cargo.toml. Skipping check."
	exit 0
fi

echo "Comparing current version ($CURRENT_VERSION) against $TARGET version ($BASE_VERSION)..."

if [ "$CURRENT_VERSION" = "$BASE_VERSION" ]; then
	echo "Error: Version in Cargo.toml is unchanged ($CURRENT_VERSION)."
	echo "Please bump the version in Cargo.toml (under [workspace.package]) before committing."
	exit 1
fi

# Function to compare semver (returns 0 if arg1 > arg2)
semver_gt() {
	local -a v1 v2
	IFS='.' read -r -a v1 <<<"$1"
	IFS='.' read -r -a v2 <<<"$2"

	# Pad to 3 components if needed
	for ((i = ${#v1[@]}; i < 3; i++)); do v1[i]=0; done
	for ((i = ${#v2[@]}; i < 3; i++)); do v2[i]=0; done

	for ((i = 0; i < 3; i++)); do
		if ((v1[i] > v2[i])); then
			return 0
		elif ((v1[i] < v2[i])); then
			return 1
		fi
	done
	return 1 # equal
}

if ! semver_gt "$CURRENT_VERSION" "$BASE_VERSION"; then
	echo "Error: Version in Cargo.toml ($CURRENT_VERSION) is not greater than $TARGET ($BASE_VERSION)."
	echo "Please bump the version to a value strictly greater than the base branch."
	exit 1
fi

echo "Version bump check passed!"
exit 0
