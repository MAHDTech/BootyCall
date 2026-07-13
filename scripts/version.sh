#!/usr/bin/env bash
set -euo pipefail

# ANSI color codes
COLOR_INFO="\033[1;34m"
COLOR_SUCCESS="\033[1;32m"
COLOR_WARN="\033[1;33m"
COLOR_ERROR="\033[1;31m"
COLOR_DEBUG="\033[1;30m"
COLOR_RESET="\033[0m"

VERBOSE=false

# Centralised logging functions
log_debug() {
	if [ "$VERBOSE" = true ]; then
		echo -e "${COLOR_DEBUG}[DEBUG] $*${COLOR_RESET}" >&2
	fi
}

log_info() {
	echo -e "${COLOR_INFO}[INFO] $*${COLOR_RESET}"
}

log_success() {
	echo -e "${COLOR_SUCCESS}[SUCCESS] $*${COLOR_RESET}"
}

log_warn() {
	echo -e "${COLOR_WARN}[WARN] $*${COLOR_RESET}" >&2
}

log_error() {
	echo -e "${COLOR_ERROR}[ERROR] $*${COLOR_RESET}" >&2
}

# Global variable to track the temporary file path for cleanup trap
TEMP_FILE=""

# Cleanup handler for exit, interrupt, and termination signals
cleanup() {
	if [ -n "${TEMP_FILE:-}" ] && [ -f "$TEMP_FILE" ]; then
		log_debug "Cleaning up temporary file: $TEMP_FILE"
		rm -f "$TEMP_FILE"
	fi
}

trap cleanup EXIT INT TERM

# Print script usage instructions
show_usage() {
	cat <<EOF
Usage: $(basename "$0") [options] [COMMAND]

Commands:
  CI                   Run CI version collision check
  TEST                 Run semver_gt self-tests
  MAJOR                Force bump MAJOR version (--major)
  MINOR                Force bump MINOR version (--minor)
  PATCH                Force bump PATCH version (--patch)
  (None)               Automatically bump based on conventional commits

Options:
  -v, --verbose        Enable verbose debug output
  -h, --help           Show this help message
EOF
}

# Verify that required command-line utilities are installed
check_dependency() {
	local dep="$1"
	local help_msg="$2"
	if ! command -v "$dep" &>/dev/null; then
		log_error "Required dependency '$dep' is missing."
		if [ -n "$help_msg" ]; then
			log_error "$help_msg"
		fi
		exit 1
	fi
}

# Resolve the workspace root and Cargo.toml path
get_cargo_toml_path() {
	local repo_root
	if git rev-parse --show-toplevel &>/dev/null; then
		repo_root=$(git rev-parse --show-toplevel)
		log_debug "Located git repository root: $repo_root"
		echo "$repo_root/Cargo.toml"
	else
		log_debug "Not in a git repository. Defaulting to local directory."
		echo "Cargo.toml"
	fi
}

# Parse package version from Cargo.toml path, content string, or stdin
parse_version() {
	local input="${1:-}"
	local content
	if [ -f "$input" ]; then
		content=$(cat "$input")
	elif [ -n "$input" ]; then
		content="$input"
	else
		content=$(cat)
	fi

	local version
	version=$(echo "$content" | sed -n '/\[workspace.package\]/,/^\[/p' | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
	if [ -z "$version" ]; then
		version=$(echo "$content" | grep '^version =' | head -n1 | cut -d'"' -f2 || true)
	fi
	echo "$version"
}

# Determine the default branch of the repository
get_default_branch() {
	local branch
	log_debug "Determining default branch..."

	# Try GitHub CLI if installed
	branch=$(gh repo view --json defaultBranchRef --jq .defaultBranchRef.name 2>/dev/null || true)
	if [ -n "$branch" ]; then
		log_debug "Found default branch via gh CLI: $branch"
		echo "$branch"
		return 0
	fi

	# Try remote origin HEAD symbolic ref
	branch=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || true)
	if [ -n "$branch" ]; then
		log_debug "Found default branch via git remote HEAD: $branch"
		echo "$branch"
		return 0
	fi

	# Fallback to common branch names
	for b in trunk main master; do
		if git rev-parse --verify "origin/$b" &>/dev/null || git rev-parse --verify "$b" &>/dev/null; then
			log_debug "Fallback default branch: $b"
			echo "$b"
			return 0
		fi
	done

	log_error "Could not determine default branch of the repository."
	exit 1
}

# Semver comparison helper (returns 0 if arg1 > arg2)
semver_gt() {
	local version1="$1"
	local version2="$2"
	local core1 prerelease1 core2 prerelease2
	local LC_ALL=C

	if [[ $version1 =~ - ]]; then
		core1="${version1%%-*}"
		prerelease1="${version1#*-}"
	else
		core1="$version1"
		prerelease1=""
	fi

	if [[ $version2 =~ - ]]; then
		core2="${version2%%-*}"
		prerelease2="${version2#*-}"
	else
		core2="$version2"
		prerelease2=""
	fi

	local -a c1 c2
	IFS='.' read -r -a c1 <<<"$core1"
	IFS='.' read -r -a c2 <<<"$core2"

	local i
	for ((i = ${#c1[@]}; i < 3; i++)); do c1[i]=0; done
	for ((i = ${#c2[@]}; i < 3; i++)); do c2[i]=0; done

	for ((i = 0; i < 3; i++)); do
		local comp1=${c1[i]:-0}
		local comp2=${c2[i]:-0}
		if ((comp1 > comp2)); then
			return 0
		elif ((comp1 < comp2)); then
			return 1
		fi
	done

	if [[ -z $prerelease1 && -z $prerelease2 ]]; then
		return 1
	elif [[ -z $prerelease1 && -n $prerelease2 ]]; then
		return 0
	elif [[ -n $prerelease1 && -z $prerelease2 ]]; then
		return 1
	else
		local -a p1 p2
		IFS='.' read -r -a p1 <<<"$prerelease1"
		IFS='.' read -r -a p2 <<<"$prerelease2"

		local len1=${#p1[@]}
		local len2=${#p2[@]}
		local min_len=$((len1 < len2 ? len1 : len2))

		for ((i = 0; i < min_len; i++)); do
			local id1="${p1[i]:-}"
			local id2="${p2[i]:-}"

			local id1_is_num=0
			local id2_is_num=0
			if [[ $id1 =~ ^[0-9]+$ ]]; then
				id1_is_num=1
			fi
			if [[ $id2 =~ ^[0-9]+$ ]]; then
				id2_is_num=1
			fi

			if ((id1_is_num && id2_is_num)); then
				local val1=$((10#$id1))
				local val2=$((10#$id2))
				if ((val1 > val2)); then
					return 0
				elif ((val1 < val2)); then
					return 1
				fi
			elif ((id1_is_num && !id2_is_num)); then
				return 1
			elif ((!id1_is_num && id2_is_num)); then
				return 0
			else
				if [[ $id1 > $id2 ]]; then
					return 0
				elif [[ $id1 < $id2 ]]; then
					return 1
				fi
			fi
		done

		if ((len1 > len2)); then
			return 0
		else
			return 1
		fi
	fi
}

assert_gt() {
	local v1="$1"
	local v2="$2"
	if ! semver_gt "$v1" "$v2"; then
		log_error "Assertion failed: Expected $v1 > $v2, but it was not."
		exit 1
	else
		log_info "Assertion passed: $v1 > $v2"
	fi
}

assert_not_gt() {
	local v1="$1"
	local v2="$2"
	if semver_gt "$v1" "$v2"; then
		log_error "Assertion failed: Expected NOT $v1 > $v2, but it was."
		exit 1
	else
		log_info "Assertion passed: NOT $v1 > $v2"
	fi
}

run_tests() {
	log_info "Running semver_gt unit tests..."

	# Core version checks
	assert_gt "1.0.0" "0.9.0"
	assert_not_gt "0.9.0" "1.0.0"
	assert_not_gt "1.0.0" "1.0.0"
	assert_gt "1.0.1" "1.0.0"
	assert_gt "1.1.0" "1.0.0"
	assert_gt "2.0.0" "1.9.9"

	# Normal vs Pre-release checks
	assert_not_gt "1.0.0-rc.1" "1.0.0"
	assert_gt "1.0.0" "1.0.0-rc.1"
	assert_gt "2.0.0-rc.1" "1.9.9"
	assert_not_gt "1.9.9" "2.0.0-rc.1"

	# Numeric pre-release checks
	assert_gt "1.0.0-beta.11" "1.0.0-beta.2"
	assert_not_gt "1.0.0-beta.2" "1.0.0-beta.11"

	# Lexical pre-release checks
	assert_gt "1.0.0-rc.1" "1.0.0-beta.11"
	assert_not_gt "1.0.0-beta.11" "1.0.0-rc.1"

	# Numeric vs Non-numeric pre-release checks
	assert_gt "1.0.0-alpha.beta" "1.0.0-alpha.1"
	assert_not_gt "1.0.0-alpha.1" "1.0.0-alpha.beta"

	# Field count pre-release checks
	assert_gt "1.0.0-alpha.1" "1.0.0-alpha"
	assert_not_gt "1.0.0-alpha" "1.0.0-alpha.1"

	# Complex tags with hyphens and dots
	assert_gt "1.0.0-rc.1" "1.0.0-beta"
	assert_gt "1.0.0-beta" "1.0.0-alpha"
	assert_not_gt "1.0.0-beta" "1.0.0-beta.1"
	assert_gt "2.0.0-rc.1" "2.0.0-beta.1"

	log_success "All semver_gt tests passed successfully!"
}

# Run the CI version collision checks
run_ci_check() {
	log_info "Running CI version collision check..."

	local default_branch
	default_branch=$(get_default_branch)
	local target="origin/$default_branch"

	log_debug "Target base branch: $target"

	# Check if any Rust/Cargo source or config files changed
	log_debug "Checking if Rust or Cargo files changed between $target and HEAD..."
	local files_changed
	local git_diff_out
	if ! git_diff_out=$(git diff --name-only "$target...HEAD" 2>&1); then
		log_error "Failed to run git diff against $target: $git_diff_out"
		exit 1
	fi
	files_changed="$git_diff_out"

	local rust_changes=false
	while read -r file; do
		if [ -z "$file" ]; then
			continue
		fi
		# Only need to bump the version if these files were changed
		if [[ $file =~ ^Cargo\.toml$ ]] ||
			[[ $file =~ ^Cargo\.lock$ ]] ||
			[[ $file =~ ^rust-toolchain\.toml$ ]] ||
			[[ $file =~ ^crates/ ]] ||
			[[ $file =~ ^static/ ]]; then
			rust_changes=true
			log_debug "Detected Rust/Cargo file change: $file"
			break
		fi
	done <<<"$files_changed"

	if [ "$rust_changes" = false ]; then
		log_success "No Rust/Cargo source or configuration changes detected. Bypassing version collision check."
		exit 0
	fi

	local target_cargo
	target_cargo=$(git show "$target:Cargo.toml" 2>/dev/null || true)
	if [ -z "$target_cargo" ]; then
		log_warn "Could not read Cargo.toml from $target. Skipping collision check."
		exit 0
	fi

	local base_version
	base_version=$(parse_version "$target_cargo")
	if [ -z "$base_version" ]; then
		log_warn "Could not find version in $target:Cargo.toml. Skipping collision check."
		exit 0
	fi

	# Calculate expected version using convco
	local expected_version
	expected_version=$(convco version --bump 2>/dev/null || echo "")
	expected_version="${expected_version#v}"
	if [ -z "$expected_version" ]; then
		expected_version="$base_version"
	fi

	log_info "Comparing versions:"
	log_info "  Base branch version:           $base_version"
	log_info "  Expected version (by convco):  $expected_version"
	log_info "  Actual PR version:             $CURRENT_VERSION"

	# Using global semver_gt comparison helper
	if semver_gt "$expected_version" "$base_version"; then
		# A bump is required!
		if [ "$CURRENT_VERSION" = "$base_version" ]; then
			log_error "This PR contains feature or fix commits that require a version bump."
			log_error "The expected version is v$expected_version, but Cargo.toml is still at v$base_version."
			log_error "Please run 'version' inside your devenv shell locally to bump the version, stage the changes, and push again."
			exit 1
		fi

		if ! semver_gt "$CURRENT_VERSION" "$base_version"; then
			log_error "PR version v$CURRENT_VERSION is not greater than base branch version v$base_version."
			exit 1
		fi

		# Verify tag collision
		local tag="v$CURRENT_VERSION"
		log_info "Checking if tag $tag already exists on remote origin..."
		if git ls-remote --tags origin "refs/tags/$tag" | grep -q "refs/tags/$tag"; then
			log_error "Release/Tag $tag already exists on the remote!"
			log_error "Please increment the version to a unique value under [workspace.package] in Cargo.toml."
			exit 1
		else
			log_success "Version $tag is valid and does not exist yet on the remote."
		fi
	else
		# No bump required
		if [ "$CURRENT_VERSION" != "$base_version" ]; then
			log_warn "Version was bumped in this PR even though convco calculated no bump is required."
		fi
		log_info "No version bump required. Bypassing version collision check."
	fi
}

# Main orchestrator function
main() {
	local bump_arg=""
	local command=""

	# Parse flags and arguments
	while [[ $# -gt 0 ]]; do
		case "$1" in
		-v | --verbose)
			VERBOSE=true
			shift
			;;
		-h | --help)
			show_usage
			exit 0
			;;
		*)
			if [ -z "$command" ]; then
				command=$(echo "$1" | tr '[:lower:]' '[:upper:]')
			else
				log_error "Unexpected argument: $1"
				show_usage
				exit 1
			fi
			shift
			;;
		esac
	done

	# Log start details in verbose mode
	log_debug "Verbose logging enabled."

	# Route TEST command early to avoid checking external dependencies
	if [ "$command" = "TEST" ]; then
		run_tests
		exit 0
	fi

	# Ensure we have the latest tags (fetch from remote if possible)
	log_debug "Fetching tags from origin remote..."
	git fetch --tags origin >/dev/null 2>&1 || true

	# Check dependencies
	check_dependency "convco" "Install convco to calculate version bumps from Conventional Commits."
	check_dependency "toml" "Install toml-cli to update version fields in Cargo.toml."

	# Resolve Cargo.toml path and get current version
	local cargo_toml_path
	cargo_toml_path=$(get_cargo_toml_path)
	log_debug "Using Cargo.toml at: $cargo_toml_path"

	CURRENT_VERSION=$(parse_version "$cargo_toml_path")
	if [ -z "$CURRENT_VERSION" ]; then
		log_error "Could not find current version in $cargo_toml_path"
		exit 1
	fi
	log_debug "Current workspace version parsed: $CURRENT_VERSION"

	# Route commands
	case "$command" in
	CI)
		run_ci_check
		exit 0
		;;
	TEST)
		run_tests
		exit 0
		;;
	MAJOR)
		bump_arg="--major"
		;;
	MINOR)
		bump_arg="--minor"
		;;
	PATCH)
		bump_arg="--patch"
		;;
	"")
		# Default run: automatic bump detection
		;;
	*)
		log_error "Invalid command '$command'."
		show_usage
		exit 1
		;;
	esac

	# Calculate next version
	log_debug "Calculating next version bump..."
	local next_version
	next_version=$(convco version --bump ${bump_arg} 2>/dev/null || echo "")
	next_version="${next_version#v}"

	if [ -z "$next_version" ] || [ "$next_version" = "$CURRENT_VERSION" ]; then
		log_info "No version bump needed (Current version: ${CURRENT_VERSION})."
		exit 0
	fi

	log_info "Bumping version: ${CURRENT_VERSION} -> ${next_version}"
	log_debug "Writing new version to $cargo_toml_path"

	# Update Cargo.toml
	TEMP_FILE=$(mktemp "${cargo_toml_path}.tmp.XXXXXX")
	toml set "$cargo_toml_path" workspace.package.version "${next_version}" >"$TEMP_FILE"
	mv "$TEMP_FILE" "$cargo_toml_path"
	TEMP_FILE=""

	# Update Cargo.lock if cargo is installed
	if command -v cargo &>/dev/null; then
		log_info "Updating Cargo.lock..."
		cargo check --quiet 2>/dev/null || true
	else
		log_debug "cargo command not found; skipping Cargo.lock check"
	fi

	log_success "Version successfully bumped to ${next_version}."
	log_info "Please review changes, stage them ('git add Cargo.toml Cargo.lock'), and commit."
}

main "$@"
