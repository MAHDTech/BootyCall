#!/usr/bin/env bash

##################################################
# Disk Wipe Utility
##################################################
#
# This script wipes all fixed disks (non-USB, non-ROM) detected on the system.
# It uses the 'wipefs' command to erase filesystem signatures.
# Optionally, it can fill disks with random data from /dev/urandom,
# followed by zeroing with /dev/zero using 'dd'.
# For NVMe disks, it can perform a secure erase using 'nvme sanitize --sanitize revert'
# if the --nvme-secure option is used.
#
# WARNING: This operation permanently erases all data on the targeted disks.
# There is no way to recover data after running this script.
#
# This script uses sudo for privileged disk operations.
# Review the detected disks before confirming.
#
# Use at your own risk!
#
# Notes on NVMe/SSD:
# - NVMe secure erase (sanitize revert) is a fast, secure way to erase data
#   without physically overwriting, by reverting the drive to its factory state.
# - After data wiping with dd, SSDs may benefit from TRIM (via fstrim) to reclaim
#   space, but this script does not include that as it's typically done post-format.
# - No explicit sync or garbage collection is triggered here, as secure erase
#   handles data removal at the firmware level for NVMe, and dd overwrites data
#   fully (which is more secure than partial overwrites).
# - For SSD endurance, secure erase is preferred over dd to avoid unnecessary writes.
#
##################################################

set -euo pipefail

# Default flags
zero=false
random=false
nvme_secure=false

# Detect OS
detect_os() {
	if [ -f /etc/os-release ]; then
		# shellcheck disable=SC1091
		. /etc/os-release
		if [ "$ID" = "ubuntu" ] || [ "$ID" = "debian" ]; then
			log INFO "OS detected: $PRETTY_NAME"
		else
			log ERR "Unsupported OS: $PRETTY_NAME. This script only supports Ubuntu/Debian."
			exit 1
		fi
	else
		log ERR "Cannot determine OS. /etc/os-release not found."
		exit 1
	fi
}

# Install dependencies
install_deps() {
	log INFO "Updating package list"
	if ! sudo apt update -qq >/dev/null 2>&1; then
		# Non-fatal error, attempt package install anyway.
		log WARN "apt update failed, attempting package install"
	fi
	log INFO "Installing required packages: util-linux, nvme-cli"
	if ! sudo apt install -y -qq util-linux nvme-cli >/dev/null 2>&1; then
		log ERR "apt install failed."
		exit 1
	fi
	log INFO "Dependencies installed successfully."
}

# ANSI color codes for logging
reset='\033[0m'
blue='\033[34m'   # DEBUG
green='\033[32m'  # INFO
yellow='\033[33m' # WARN
red='\033[31m'    # ERR

log() {
	local level=$1
	local msg=$2
	case $level in
	DEBUG) color=$blue ;;
	INFO) color=$green ;;
	WARN) color=$yellow ;;
	ERR) color=$red ;;
	*) color=$reset ;;
	esac
	echo -e "${color}[$level]${reset} $msg"
}

##################################################
# Confirmation
##################################################

get_confirmation() {
	log WARN "This will wipe every fixed disk it finds. Are you sure about this? [y/N]"
	read -rp "Response: " response
	[[ ${response:-} == [yY] ]]
}

##################################################
# Disk Detection
##################################################

get_disks() {
	disks_to_wipe=()
	ignored_disks=()
	while read -r name type tran; do
		if [ "$type" = "disk" ] && [ "$tran" != "usb" ]; then
			disks_to_wipe+=("/dev/$name")
		elif [ "$type" = "rom" ] || [ "$tran" = "usb" ]; then
			ignored_disks+=("/dev/$name")
		fi
	done < <(sudo lsblk -dno NAME,TYPE,TRAN)
}

##################################################
# Wipe Operations
##################################################

wipe_all_disks() {
	wiped=()
	failed=()
	declare -A disk_status
	local nvme_sanitize_args=(--sanact=start-block-erase --ause)
	for disk in "${disks_to_wipe[@]}"; do
		disk_status["$disk"]="success"
	done

	# Phase 1: Wipe filesystem signatures in parallel
	log INFO "Starting filesystem signature wipe on all disks"
	pids=()
	pid_to_disk=()
	index=0
	for disk in "${disks_to_wipe[@]}"; do
		(
			log INFO "Wiping filesystem signatures on $disk"
			if sudo wipefs -af "$disk" >/dev/null 2>&1; then
				log INFO "✓ Successfully wiped filesystem signatures on $disk"
				exit 0
			else
				log ERR "✗ Failed to wipe filesystem signatures on $disk"
				exit 1
			fi
		) &
		pids+=($!)
		pid_to_disk["$index"]="$disk"
		((index++))
	done
	for i in "${!pids[@]}"; do
		pid=${pids[$i]}
		disk=${pid_to_disk[$i]}
		if ! wait "$pid"; then
			disk_status["$disk"]="failed"
		fi
	done

	# Phase 2: NVMe secure erase (if enabled) in parallel for eligible disks
	if [ "$nvme_secure" = true ]; then
		log INFO "Starting NVMe secure erase on eligible disks"
		pids=()
		pid_to_disk=()
		index=0
		for disk in "${disks_to_wipe[@]}"; do
			if [[ $disk =~ ^/dev/nvme ]] && [ "${disk_status[$disk]}" = "success" ]; then
				(
					log INFO "Performing NVMe secure erase on $disk using '${nvme_sanitize_args[*]}'"
					if sudo nvme sanitize "$disk" "${nvme_sanitize_args[@]}" >/dev/null 2>&1; then
						log INFO "✓ Successfully performed NVMe secure erase on $disk"
						exit 0
					else
						log ERR "✗ Failed to perform NVMe secure erase on $disk"
						exit 1
					fi
				) &
				pids+=($!)
				pid_to_disk["$index"]="$disk"
				((index++))
			fi
		done
		for i in "${!pids[@]}"; do
			pid=${pids[$i]}
			disk=${pid_to_disk[$i]}
			if ! wait "$pid"; then
				disk_status["$disk"]="failed"
			fi
		done
	fi

	# Collect disks that need dd (non-NVMe or no secure erase, and still successful)
	dd_disks=()
	for disk in "${disks_to_wipe[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ] && ! { [[ $disk =~ ^/dev/nvme ]] && [ "$nvme_secure" = true ]; }; then
			dd_disks+=("$disk")
		fi
	done

	# Phase 3: Random fill in parallel (if enabled)
	if [ "$random" = true ] && [ ${#dd_disks[@]} -gt 0 ]; then
		log INFO "Starting random data fill on ${#dd_disks[@]} disks"
		pids=()
		pid_to_disk=()
		index=0
		for disk in "${dd_disks[@]}"; do
			(
				log INFO "Filling $disk with random data using dd and /dev/urandom"
				if sudo dd if=/dev/urandom of="$disk" bs=1M status=progress; then
					log INFO "✓ Successfully filled $disk with random data"
					exit 0
				else
					log ERR "✗ Failed to fill $disk with random data"
					exit 1
				fi
			) &
			pids+=($!)
			pid_to_disk["$index"]="$disk"
			((index++))
		done
		for i in "${!pids[@]}"; do
			pid=${pids[$i]}
			disk=${pid_to_disk[$i]}
			if ! wait "$pid"; then
				disk_status["$disk"]="failed"
			fi
		done
	fi

	# Recalculate dd_disks for zero phase (exclude any that failed random)
	zero_disks=()
	for disk in "${dd_disks[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ]; then
			zero_disks+=("$disk")
		fi
	done

	# Phase 4: Zero fill in parallel (if enabled)
	if [ "$zero" = true ] && [ ${#zero_disks[@]} -gt 0 ]; then
		log INFO "Starting zero fill on ${#zero_disks[@]} disks"
		pids=()
		pid_to_disk=()
		index=0
		for disk in "${zero_disks[@]}"; do
			(
				log INFO "Zeroing $disk using dd and /dev/zero"
				if sudo dd if=/dev/zero of="$disk" bs=1M status=progress; then
					log INFO "✓ Successfully zeroed $disk"
					exit 0
				else
					log ERR "✗ Failed to zero $disk"
					exit 1
				fi
			) &
			pids+=($!)
			pid_to_disk["$index"]="$disk"
			((index++))
		done
		for i in "${!pids[@]}"; do
			pid=${pids[$i]}
			disk=${pid_to_disk[$i]}
			if ! wait "$pid"; then
				disk_status["$disk"]="failed"
			fi
		done
	fi

	# Build wiped/failed lists for summary
	for disk in "${disks_to_wipe[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ]; then
			wiped+=("$disk")
			log INFO "Successfully wiped $disk"
		else
			failed+=("$disk")
			log ERR "Failed to wipe $disk"
		fi
	done
}

##################################################
# Summary
##################################################

print_summary() {
	if [ ${#disks_to_wipe[@]} -eq 0 ]; then
		log INFO "No disks to wipe found."
		return
	fi

	echo -e "\n${green}=========================================${reset}"
	echo -e "${green}         OPERATION SUMMARY${reset}"
	echo -e "${green}=========================================${reset}"

	echo -e "${green}Wiped disks (${#wiped[@]}):${reset}"
	if [ ${#wiped[@]} -gt 0 ]; then
		for d in "${wiped[@]}"; do
			echo -e "${green}  ✓ $d${reset}"
		done
	else
		echo -e "${yellow}  None${reset}"
	fi

	echo

	echo -e "${red}Failed wipes (${#failed[@]}):${reset}"
	if [ ${#failed[@]} -gt 0 ]; then
		for d in "${failed[@]}"; do
			echo -e "${red}  ✗ $d${reset}"
		done
	else
		echo -e "${green}  None${reset}"
	fi

	echo

	echo -e "${blue}Ignored disks (${#ignored_disks[@]}):${reset}"
	if [ ${#ignored_disks[@]} -gt 0 ]; then
		for d in "${ignored_disks[@]}"; do
			echo -e "${blue}  - $d${reset}"
		done
	else
		echo -e "${yellow}  None${reset}"
	fi

	echo -e "${green}=========================================${reset}"
}

##################################################
# Usage
##################################################

usage() {
	echo "Usage: $0 [options]"
	echo
	echo "Options:"
	echo "  --help          Show this help message and exit"
	echo "  --zero          Zero the drives using dd and /dev/zero"
	echo "  --random        Fill drives with random data using dd and /dev/urandom"
	echo "  --nvme-secure   Use NVMe secure erase (sanitize revert) for NVMe disks instead of dd"
	echo
	echo "Notes:"
	echo "  - --random and --zero can be combined; random filling happens first, then zeroing."
	echo "  - --nvme-secure only applies to NVMe disks and replaces dd operations."
	echo "  - Without any flags, only filesystem signatures are wiped using wipefs."
	echo "  - Ensure you have 'nvme-cli' installed for NVMe secure erase."
	echo "  - For SSDs, secure erase is preferred for speed and endurance."
}

##################################################
# Main
##################################################

main() {
	# Parse command-line arguments
	while [[ $# -gt 0 ]]; do
		case $1 in
		--help)
			usage
			exit 0
			;;
		--zero)
			zero=true
			shift
			;;
		--random)
			random=true
			shift
			;;
		--nvme-secure)
			nvme_secure=true
			shift
			;;
		*)
			log ERR "Unknown option: $1"
			usage
			exit 1
			;;
		esac
	done

	detect_os
	install_deps

	log INFO "Starting disk wipe script"
	if ! get_confirmation; then
		log INFO "Aborted by user."
		exit 0
	fi

	get_disks
	log INFO "Detected ${#disks_to_wipe[@]} disk(s) to wipe: ${disks_to_wipe[*]:-None}"
	log INFO "Detected ${#ignored_disks[@]} ignored disk(s): ${ignored_disks[*]:-None}"

	wipe_all_disks
	print_summary
}

main "$@"
