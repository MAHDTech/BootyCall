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

run_ci_check() {
	echo "Running CI version collision check..."

	# Determine default branch
	local DEFAULT_BRANCH
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
		echo "Error: Could not determine default branch"
		exit 1
	fi

	local TARGET="origin/$DEFAULT_BRANCH"
	local TARGET_CARGO
	TARGET_CARGO=$(git show "$TARGET:Cargo.toml" 2>/dev/null || true)
	if [ -z "$TARGET_CARGO" ]; then
		echo "Warning: Could not read Cargo.toml from $TARGET. Skipping collision check."
		exit 0
	fi

	local BASE_VERSION
	BASE_VERSION=$(echo "$TARGET_CARGO" | sed -n '/\[workspace.package\]/,/^\[/p' | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
	if [ -z "$BASE_VERSION" ]; then
		BASE_VERSION=$(echo "$TARGET_CARGO" | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
	fi

	if [ -z "$BASE_VERSION" ]; then
		echo "Warning: Could not find version in $TARGET:Cargo.toml. Skipping collision check."
		exit 0
	fi

	# Calculate expected version using convco
	local EXPECTED_VERSION
	EXPECTED_VERSION=$(convco version --bump 2>/dev/null || echo "")
	EXPECTED_VERSION="${EXPECTED_VERSION#v}"
	if [ -z "$EXPECTED_VERSION" ]; then
		EXPECTED_VERSION="$BASE_VERSION"
	fi

	echo "Comparing versions:"
	echo "  Base branch version:           $BASE_VERSION"
	echo "  Expected version (by convco):  $EXPECTED_VERSION"
	echo "  Actual PR version:             $CURRENT_VERSION"

	# Function to compare semver (returns 0 if arg1 > arg2)
	semver_gt() {
		local -a v1 v2
		IFS='.' read -r -a v1 <<<"$1"
		IFS='.' read -r -a v2 <<<"$2"

		for ((i = ${#v1[@]}; i < 3; i++)); do v1[i]=0; done
		for ((i = ${#v2[@]}; i < 3; i++)); do v2[i]=0; done

		for ((i = 0; i < 3; i++)); do
			if ((v1[i] > v2[i])); then
				return 0
			elif ((v1[i] < v2[i])); then
				return 1
			fi
		done
		return 1
	}

	if semver_gt "$EXPECTED_VERSION" "$BASE_VERSION"; then
		# A bump is required!
		if [ "$CURRENT_VERSION" = "$BASE_VERSION" ]; then
			echo "Error: This PR contains feature or fix commits that require a version bump."
			echo "The expected version is v$EXPECTED_VERSION, but Cargo.toml is still at v$BASE_VERSION."
			echo "Please run 'version' inside your devenv shell locally to bump the version, stage the changes, and push again."
			exit 1
		fi

		if ! semver_gt "$CURRENT_VERSION" "$BASE_VERSION"; then
			echo "Error: PR version v$CURRENT_VERSION is not greater than base branch version v$BASE_VERSION."
			exit 1
		fi

		# Verify tag collision
		local TAG="v$CURRENT_VERSION"
		echo "Checking if tag $TAG already exists on remote origin..."
		if git ls-remote --tags origin "refs/tags/$TAG" | grep -q "refs/tags/$TAG"; then
			echo "Error: Release/Tag $TAG already exists on the remote!"
			echo "Please increment the version to a unique value under [workspace.package] in Cargo.toml."
			exit 1
		else
			echo "Version $TAG is valid and does not exist yet on the remote."
		fi
	else
		# No bump required
		if [ "$CURRENT_VERSION" != "$BASE_VERSION" ]; then
			echo "Warning: Version was bumped in this PR even though convco calculated no bump is required."
		fi
		echo "No version bump required. Bypassing version collision check."
	fi
}

BUMP_ARG=""
if [ $# -gt 0 ]; then
	ARG_UPPER=$(echo "$1" | tr '[:lower:]' '[:upper:]')
	case "$ARG_UPPER" in
	CI)
		run_ci_check
		exit 0
		;;
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
		echo "Error: Invalid argument '$1'. Expected CI, MAJOR, MINOR, or PATCH."
		exit 1
		;;
	esac
fi

# Run convco version --bump with optional argument
NEXT_VERSION=$(convco version --bump ${BUMP_ARG:-} 2>/dev/null || echo "")
NEXT_VERSION="${NEXT_VERSION#v}"

if [ -z "$NEXT_VERSION" ] || [ "$NEXT_VERSION" = "$CURRENT_VERSION" ]; then
	echo "No version bump needed (Current version: ${CURRENT_VERSION})."
	exit 0
fi

echo "Bumping version: ${CURRENT_VERSION} -> ${NEXT_VERSION}"
echo "Updating Cargo.toml..."
toml set Cargo.toml workspace.package.version "${NEXT_VERSION}" >Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

if command -v cargo &>/dev/null; then
	echo "Updating Cargo.lock..."
	cargo check --quiet 2>/dev/null || true
fi

echo "Version successfully bumped to ${NEXT_VERSION}."
echo "Please review changes, stage them ('git add Cargo.toml Cargo.lock'), and commit."
