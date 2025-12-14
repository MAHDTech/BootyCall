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

##################################################
# Global Variables
##################################################

# Script name is used in log file.
SCRIPT_NAME="${BASH_SOURCE[0]##*/}"
SCRIPT_NAME="${SCRIPT_NAME%.*}"
LOG_FILE="${SCRIPT_NAME}.log"

# Global flags
declare ZERO_FILL="false"
declare RANDOM_FILL="false"
declare NVME_SECURE="false"

# Logging configuration
declare LOG_LEVEL="INFO"
declare LOG_FILE_LEVEL="DEBUG"

# Log level hierarchy (lower number = higher priority)
declare -A LOG_LEVELS=(
	[DEBUG]=0
	[INFO]=1
	[WARN]=2
	[ERR]=3
)

# Global arrays
declare -a DISKS_TO_WIPE=()
declare -a IGNORED_DISKS=()
declare -a WIPED=()
declare -a FAILED=()
declare -a ACTIVE_PIDS=()

# Global associative arrays for health check
declare -A DISK_HEALTH_STATUS=()
declare -A DISK_HEALTH_DETAILS=()

# ANSI color codes (global constants)
RESET='\e[0m'
BLUE='\e[34m'   # DEBUG
GREEN='\e[32m'  # INFO
YELLOW='\e[33m' # WARN
RED='\e[31m'    # ERR

##################################################
# Functions
##################################################

# Logging function with level filtering and file output
log() {
	local level=$1
	local msg=$2
	local color
	local timestamp
	timestamp=$(date '+%Y-%m-%d %H:%M:%S')

	# Determine color based on level
	case $level in
	DEBUG) color=$BLUE ;;
	INFO) color=$GREEN ;;
	WARN) color=$YELLOW ;;
	ERR) color=$RED ;;
	*) color=$RESET ;;
	esac

	# Log to file if level meets file threshold
	if [ "${LOG_LEVELS[$level]}" -ge "${LOG_LEVELS[$LOG_FILE_LEVEL]}" ]; then
		echo "[$timestamp] [$level] $msg" >>"$LOG_FILE"
	fi

	# Log to stdout if level meets stdout threshold
	if [ "${LOG_LEVELS[$level]}" -ge "${LOG_LEVELS[$LOG_LEVEL]}" ]; then
		echo -e "${color}[$level]${RESET} $msg"
	fi
}

# Print a colored header/separator
print_header() {
	local msg=$1
	local color=${2:-$BLUE}

	# Only print to stdout, not to log file
	echo -e "\n${color}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
	log INFO "$msg"
	echo -e "${color}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}\n"
}

cleanup() {
	local exit_code=$?

	# Show cursor in case it was hidden
	tput cnorm 2>/dev/null || true

	# If there are active background processes, kill them
	if [ ${#ACTIVE_PIDS[@]} -gt 0 ]; then
		echo -e "\n"
		log WARN "Interrupt received! Cleaning up..."
		log WARN "Terminating ${#ACTIVE_PIDS[@]} active dd process(es)..."

		for pid in "${ACTIVE_PIDS[@]}"; do
			if kill -0 "$pid" 2>/dev/null; then
				# Try graceful termination first
				kill -TERM "$pid" 2>/dev/null || true
			fi
		done

		# Wait a moment for graceful termination
		sleep 1

		# Force kill any remaining processes
		for pid in "${ACTIVE_PIDS[@]}"; do
			if kill -0 "$pid" 2>/dev/null; then
				kill -KILL "$pid" 2>/dev/null || true
			fi
		done

		log INFO "Cleanup complete. All processes terminated."
	fi

	# Exit with the original exit code
	exit "$exit_code"
}

# Trap Ctrl+C (SIGINT) and other termination signals
trap cleanup SIGINT SIGTERM EXIT

# Detect OS
detect_os() {
	if [ -f /etc/os-release ]; then
		# shellcheck disable=SC1091
		. /etc/os-release
		if [ "$ID" = "ubuntu" ] || [ "$ID" = "debian" ]; then
			log INFO "OS detected: $PRETTY_NAME"
		else
			log ERR "Unsupported OS: $PRETTY_NAME. This script only supports Ubuntu/Debian."
			return 1
		fi
	else
		log ERR "Cannot determine OS. /etc/os-release not found."
		return 1
	fi
}

# Install dependencies
install_deps() {
	local -a PACKAGE_NAMES=(util-linux nvme-cli smartmontools)

	log INFO "Updating package list"
	if ! sudo apt update -qq >/dev/null 2>&1; then
		# Non-fatal error, attempt package install anyway.
		log WARN "apt update failed, attempting package install"
	fi
	log INFO "Installing required packages: ${PACKAGE_NAMES[*]}"
	if ! sudo apt install -y -qq "${PACKAGE_NAMES[@]}" >/dev/null 2>&1; then
		log ERR "apt install failed."
		return 1
	fi
	log INFO "Dependencies installed successfully."
}

# Spinner animation for long-running operations
spinner() {
	local pid=$1
	local message=$2
	local color=$3
	local spin='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
	local i=0

	# Hide cursor
	tput civis

	while kill -0 "$pid" 2>/dev/null; do
		i=$(((i + 1) % 10))
		printf "\r%s%s %s%s" "$color" "${spin:i:1}" "$message" "$RESET"
		sleep 0.1
	done

	# Show cursor
	tput cnorm
	printf "\r"
}

# Wait for multiple PIDs with a spinner
wait_with_spinner() {
	local message=$1
	local color=$2
	shift 2
	local pids=("$@")

	if [ ${#pids[@]} -eq 0 ]; then
		return 0
	fi

	local spin='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
	local i=0

	# Hide cursor
	tput civis

	# Display initial message with flush
	echo -ne "${color}${spin:0:1} ${message}${RESET}"

	# Wait for all processes
	local all_done=false
	while [ "$all_done" = false ]; do
		all_done=true
		for pid in "${pids[@]}"; do
			if kill -0 "$pid" 2>/dev/null; then
				all_done=false
				break
			fi
		done

		if [ "$all_done" = false ]; then
			i=$(((i + 1) % 10))
			echo -ne "\r${color}${spin:i:1} ${message}${RESET}"
			sleep 0.1
		fi
	done

	# Clear the line and show completion
	echo -e "\r${GREEN}✓${RESET} ${message%...} complete"

	# Show cursor
	tput cnorm
}

##################################################
# Confirmation
##################################################

get_confirmation() {
	clear
	echo -e "\n"
	# Warning box (not logged to file, visual only)
	echo -e "\n${RED}╔══════════════════════════════════════════════════════════════════════╗${RESET}"
	echo -e "${RED}║${RESET}${YELLOW}                           ⚠️  WARNING  ⚠️                              ${RESET}${RED}║${RESET}"
	echo -e "${RED}║${RESET}${RED}                                                                      ${RED}║${RESET}"
	echo -e "${RED}║${RESET}${RED}            🔥 THIS OPERATION WILL PERMANENTLY ERASE DATA! 🔥         ${RESET}${RED}║${RESET}"
	echo -e "${RED}║${RESET}${RED}                                                                      ${RED}║${RESET}"
	echo -e "${RED}║${RESET}${YELLOW}    This script will wipe every fixed disk it finds on your system.   ${RESET}${RED}║${RESET}"
	echo -e "${RED}║${RESET}${YELLOW}                                                                      ${RESET}${RED}║${RESET}"
	echo -e "${RED}║${RESET}${YELLOW}    There is NO WAY to recover the data after running this script!    ${RESET}${RED}║${RESET}"
	echo -e "${RED}║${RESET}${RED}                                                                      ${RED}║${RESET}"
	echo -e "${RED}╚══════════════════════════════════════════════════════════════════════╝${RESET}"
	echo -e "\n"

	# Log the warning to file
	log WARN "User confirmation prompt displayed"
	log WARN "${YELLOW}Are you absolutely sure you want to continue? Type 'yes' to proceed:${RESET} [y/N]"
	local response
	read -rp "Response: " response
	[[ ${response:-} == [yY] ]]
}

##################################################
# Disk Detection
##################################################

get_disks() {
	local name type tran

	while read -r name type tran; do
		if [ "$type" = "disk" ] && [ "$tran" != "usb" ]; then
			DISKS_TO_WIPE+=("/dev/$name")
		elif [ "$type" = "rom" ] || [ "$tran" = "usb" ]; then
			IGNORED_DISKS+=("/dev/$name")
		fi
	done < <(sudo lsblk -dno NAME,TYPE,TRAN || true)

	# If DISKS_TO_WIPE is empty, return 1
	if [ ${#DISKS_TO_WIPE[@]} -eq 0 ]; then
		log ERR "No disks found to wipe."
		return 1
	fi
}

##################################################
# Wipe Operations
##################################################

wipe_all_disks() {
	local disk
	local nvme_sanitize_args=(--sanact=start-block-erase --ause)
	declare -A disk_status
	local pids
	local pid_to_disk
	local index
	local pid
	local i
	local dd_disks
	local zero_disks
	local dd_exit

	# Handle empty array case for set -u compatibility
	if [ ${#DISKS_TO_WIPE[@]} -eq 0 ]; then
		log ERR "No disks found to wipe."
		return 1
	fi

	for disk in "${DISKS_TO_WIPE[@]}"; do
		disk_status["$disk"]="success"
	done

	# Phase 1: Wipe filesystem signatures in parallel
	print_header "Phase 1: Wiping filesystem signatures on all disks" "$GREEN"
	log DEBUG "Starting wipefs on ${#DISKS_TO_WIPE[@]} disk(s)"
	pids=()
	pid_to_disk=()
	index=0
	for disk in "${DISKS_TO_WIPE[@]}"; do
		(
			# Disable errexit in subshell to handle errors explicitly
			set +e
			log DEBUG "Wiping filesystem signatures on $disk"
			if sudo wipefs -af "$disk" >/dev/null 2>&1; then
				log DEBUG "✓ Successfully wiped filesystem signatures on $disk"
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

	# Wait for all wipefs operations to complete
	if [ ${#pids[@]} -gt 0 ]; then
		for i in "${!pids[@]}"; do
			pid=${pids[$i]}
			disk=${pid_to_disk[$i]}
			if ! wait "$pid"; then
				disk_status["$disk"]="failed"
			fi
		done
	fi

	# Phase 2: NVMe secure erase (if enabled) in parallel for eligible disks
	if [ "$NVME_SECURE" = "true" ]; then
		print_header "Phase 2: NVMe secure erase on eligible disks" "$BLUE"
		log DEBUG "Starting NVMe secure erase"
		pids=()
		pid_to_disk=()
		index=0
		for disk in "${DISKS_TO_WIPE[@]}"; do
			if [[ $disk =~ ^/dev/nvme ]] && [ "${disk_status[$disk]}" = "success" ]; then
				(
					set +e
					log DEBUG "Performing NVMe secure erase on $disk using '${nvme_sanitize_args[*]}'"
					if sudo nvme sanitize "$disk" "${nvme_sanitize_args[@]}" >/dev/null 2>&1; then
						log DEBUG "✓ Successfully performed NVMe secure erase on $disk"
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

		# Wait for all NVMe secure erase operations to complete
		if [ ${#pids[@]} -gt 0 ]; then
			for i in "${!pids[@]}"; do
				pid=${pids[$i]}
				disk=${pid_to_disk[$i]}
				if ! wait "$pid"; then
					disk_status["$disk"]="failed"
				fi
			done
		fi
	fi

	# Collect disks that need dd (non-NVMe or no secure erase, and still successful)
	dd_disks=()
	for disk in "${DISKS_TO_WIPE[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ] && ! { [[ $disk =~ ^/dev/nvme ]] && [ "$NVME_SECURE" = true ]; }; then
			dd_disks+=("$disk")
		fi
	done

	# Phase 3: Random fill in parallel (if enabled)
	if [ "$RANDOM_FILL" = "true" ] && [ ${#dd_disks[@]} -gt 0 ]; then
		print_header "Phase 3: Random data fill on ${#dd_disks[@]} disk(s): ${dd_disks[*]}" "$YELLOW"
		log DEBUG "Starting random fill with dd"

		pids=()
		pid_to_disk=()
		ACTIVE_PIDS=()
		index=0
		for disk in "${dd_disks[@]}"; do
			(
				set +e
				log DEBUG "Starting random fill on $disk"
				# Redirect all dd output to null - we'll show a spinner instead
				sudo dd if=/dev/urandom of="$disk" bs=1M status=none >/dev/null 2>&1
				dd_exit=$?
				# dd exits with 1 when disk is full, which is normal/expected
				if [ $dd_exit -eq 0 ] || [ $dd_exit -eq 1 ]; then
					exit 0
				else
					exit 1
				fi
			) &
			pids+=($!)
			ACTIVE_PIDS+=($!)
			pid_to_disk["$index"]="$disk"
			((index++))
		done

		# Wait with animated spinner (only on stdout, not in log file)
		wait_with_spinner "Waiting for random pass to complete..." "$YELLOW" "${pids[@]}"

		# Clear active PIDs after completion
		ACTIVE_PIDS=()

		# Check results
		if [ ${#pids[@]} -gt 0 ]; then
			for i in "${!pids[@]}"; do
				pid=${pids[$i]}
				disk=${pid_to_disk[$i]}
				if ! wait "$pid"; then
					disk_status["$disk"]="failed"
					log ERR "Random fill failed on $disk"
				else
					log INFO "✓ Random fill completed on $disk"
				fi
			done
		fi
	fi

	# Recalculate dd_disks for zero phase (exclude any that failed random)
	zero_disks=()
	for disk in "${dd_disks[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ]; then
			zero_disks+=("$disk")
		fi
	done

	# Phase 4: Zero fill in parallel (if enabled)
	if [ "$ZERO_FILL" = "true" ] && [ ${#zero_disks[@]} -gt 0 ]; then
		print_header "Phase 4: Zero fill on ${#zero_disks[@]} disk(s): ${zero_disks[*]}" "$BLUE"
		log DEBUG "Starting zero fill with dd"

		pids=()
		pid_to_disk=()
		ACTIVE_PIDS=()
		index=0
		for disk in "${zero_disks[@]}"; do
			(
				set +e
				log DEBUG "Starting zero fill on $disk"
				# Redirect all dd output to null - we'll show a spinner instead
				sudo dd if=/dev/zero of="$disk" bs=1M status=none >/dev/null 2>&1
				dd_exit=$?
				# dd exits with 1 when disk is full, which is normal/expected
				if [ $dd_exit -eq 0 ] || [ $dd_exit -eq 1 ]; then
					exit 0
				else
					exit 1
				fi
			) &
			pids+=($!)
			ACTIVE_PIDS+=($!)
			pid_to_disk["$index"]="$disk"
			((index++))
		done

		# Wait with animated spinner (only on stdout, not in log file)
		wait_with_spinner "Waiting for zero pass to complete..." "$BLUE" "${pids[@]}"

		# Clear active PIDs after completion
		ACTIVE_PIDS=()

		# Check results
		if [ ${#pids[@]} -gt 0 ]; then
			for i in "${!pids[@]}"; do
				pid=${pids[$i]}
				disk=${pid_to_disk[$i]}
				if ! wait "$pid"; then
					disk_status["$disk"]="failed"
					log ERR "Zero fill failed on $disk"
				else
					log INFO "✓ Zero fill completed on $disk"
				fi
			done
		fi
	fi

	# Build wiped/failed lists for summary
	for disk in "${DISKS_TO_WIPE[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ]; then
			WIPED+=("$disk")
			log INFO "Successfully wiped $disk"
		else
			FAILED+=("$disk")
			log ERR "Failed to wipe $disk"
		fi
	done
}

##################################################
# Health Check
##################################################

# Check disk health using SMART and NVMe tools
check_disk_health() {
	local disk=$1
	local health_status="UNKNOWN"
	local health_color=$YELLOW
	local details=""
	local temp=""
	local wear=""
	local reallocated=""
	local pending=""
	local critical_warning=""
	local available_spare=""
	local percentage_used=""
	local power_on_hours=""
	local error_count=""

	# Check if disk is NVMe
	if [[ $disk =~ ^/dev/nvme ]]; then
		# NVMe disk - use nvme-cli
		if command -v nvme &>/dev/null; then
			# Get smart-log data
			local nvme_output
			if nvme_output=$(sudo nvme smart-log "$disk" 2>/dev/null); then
				# Parse critical warnings
				critical_warning=$(echo "$nvme_output" | grep "critical_warning" | awk '{print $NF}')

				# Parse temperature
				temp=$(echo "$nvme_output" | grep "temperature" | head -1 | awk '{print $NF}')

				# Parse available spare
				available_spare=$(echo "$nvme_output" | grep "available_spare" | head -1 | awk '{print $NF}')

				# Parse percentage used
				percentage_used=$(echo "$nvme_output" | grep "percentage_used" | awk '{print $NF}')

				# Parse power on hours
				power_on_hours=$(echo "$nvme_output" | grep "power_on_hours" | awk '{print $NF}')

				# Parse media errors
				error_count=$(echo "$nvme_output" | grep "media_errors" | awk '{print $NF}')

				# Determine health status
				if [ "$critical_warning" != "0" ] && [ -n "$critical_warning" ]; then
					health_status="CRITICAL"
					health_color=$RED
					details="Critical Warning: $critical_warning"
				elif [ -n "$percentage_used" ] && [ "${percentage_used%\%}" -ge 90 ]; then
					health_status="WARNING"
					health_color=$YELLOW
					details="High wear: ${percentage_used}"
				elif [ -n "$available_spare" ] && [ "${available_spare%\%}" -lt 10 ]; then
					health_status="WARNING"
					health_color=$YELLOW
					details="Low spare: ${available_spare}"
				elif [ -n "$error_count" ] && [ "$error_count" -gt 0 ]; then
					health_status="WARNING"
					health_color=$YELLOW
					details="Media errors: $error_count"
				else
					health_status="HEALTHY"
					health_color=$GREEN
					details="Wear: ${percentage_used:-N/A}, Spare: ${available_spare:-N/A}"
				fi

				# Add temperature if available
				if [ -n "$temp" ]; then
					details="$details, Temp: ${temp}°C"
				fi

				# Add power on hours if available
				if [ -n "$power_on_hours" ]; then
					details="$details, Hours: ${power_on_hours}"
				fi
			else
				health_status="ERROR"
				health_color=$RED
				details="Failed to read NVMe SMART data"
			fi
		else
			health_status="NO_TOOL"
			health_color=$YELLOW
			details="nvme-cli not installed"
		fi
	else
		# SATA/SAS disk - use smartctl
		if command -v smartctl &>/dev/null; then
			local smart_output
			if smart_output=$(sudo smartctl -H -A "$disk" 2>/dev/null); then
				# Check overall health status
				if echo "$smart_output" | grep -q "PASSED"; then
					health_status="HEALTHY"
					health_color=$GREEN
				elif echo "$smart_output" | grep -q "FAILED"; then
					health_status="FAILED"
					health_color=$RED
				fi

				# Parse specific SMART attributes
				# Reallocated sectors (ID 5)
				reallocated=$(echo "$smart_output" | grep "^  5" | awk '{print $10}')

				# Pending sectors (ID 197)
				pending=$(echo "$smart_output" | grep "^197" | awk '{print $10}')

				# Temperature (ID 194)
				temp=$(echo "$smart_output" | grep "^194" | awk '{print $10}')

				# Wear levelling (ID 177 for SSDs)
				wear=$(echo "$smart_output" | grep "^177" | awk '{print $10}')

				# Power on hours (ID 9)
				power_on_hours=$(echo "$smart_output" | grep "^  9" | awk '{print $10}')

				# Build details string
				local detail_parts=()

				# Check for critical issues
				if [ -n "$reallocated" ] && [ "$reallocated" -gt 0 ]; then
					health_status="WARNING"
					health_color=$YELLOW
					detail_parts+=("Reallocated: $reallocated")
				fi

				if [ -n "$pending" ] && [ "$pending" -gt 0 ]; then
					health_status="WARNING"
					health_color=$YELLOW
					detail_parts+=("Pending: $pending")
				fi

				# Add wear info for SSDs
				if [ -n "$wear" ]; then
					detail_parts+=("Wear: $wear")
					if [ "$wear" -le 10 ]; then
						health_status="WARNING"
						health_color=$YELLOW
					fi
				fi

				# Add temperature
				if [ -n "$temp" ]; then
					detail_parts+=("Temp: ${temp}°C")
					if [ "$temp" -gt 60 ]; then
						if [ "$health_status" = "HEALTHY" ]; then
							health_status="WARNING"
							health_color=$YELLOW
						fi
					fi
				fi

				# Add power on hours
				if [ -n "$power_on_hours" ]; then
					detail_parts+=("Hours: $power_on_hours")
				fi

				# Combine details
				if [ ${#detail_parts[@]} -gt 0 ]; then
					details=$(
						IFS=", "
						echo "${detail_parts[*]}"
					)
				else
					details="No issues detected"
				fi
			else
				health_status="ERROR"
				health_color=$RED
				details="Failed to read SMART data"
			fi
		else
			health_status="NO_TOOL"
			health_color=$YELLOW
			details="smartmontools not installed"
		fi
	fi

	# Return status, color, and details as a formatted string
	echo "${health_color}${health_status}${RESET}|${details}"
}

# Run health check on all disks
health_check() {
	local disk
	local health_info
	local status
	local details
	declare -A disk_health_status
	declare -A disk_health_details

	if [ ${#DISKS_TO_WIPE[@]} -eq 0 ]; then
		log DEBUG "No disks available for health check"
		return
	fi

	print_header "Checking disk health" "$BLUE"
	log INFO "Running health checks on ${#DISKS_TO_WIPE[@]} disk(s)"

	for disk in "${DISKS_TO_WIPE[@]}"; do
		log DEBUG "Checking health of $disk"
		health_info=$(check_disk_health "$disk")
		status=$(echo "$health_info" | cut -d'|' -f1)
		details=$(echo "$health_info" | cut -d'|' -f2)

		disk_health_status["$disk"]="$status"
		disk_health_details["$disk"]="$details"

		echo -e "  $disk: $status"
		if [ -n "$details" ]; then
			echo -e "    └─ $details"
		fi
	done

	echo
	log INFO "Health check complete"

	# Export to global arrays for summary
	for disk in "${!disk_health_status[@]}"; do
		DISK_HEALTH_STATUS["$disk"]="${disk_health_status[$disk]}"
		DISK_HEALTH_DETAILS["$disk"]="${disk_health_details[$disk]}"
	done
}

##################################################
# Summary
##################################################

print_summary() {
	local d

	if [ ${#DISKS_TO_WIPE[@]} -eq 0 ]; then
		log INFO "No disks to wipe found."
		return
	fi

	cat <<-EOF

		${GREEN}=========================================${RESET}
		${GREEN}         OPERATION SUMMARY${RESET}
		${GREEN}=========================================${RESET}

	EOF

	# Disk Health Section
	if [ ${#DISK_HEALTH_STATUS[@]} -gt 0 ]; then
		echo -e "${BLUE}Disk Health Status:${RESET}"
		for d in "${DISKS_TO_WIPE[@]}"; do
			if [ -n "${DISK_HEALTH_STATUS[$d]}" ]; then
				echo -e "  $d: ${DISK_HEALTH_STATUS[$d]}"
				if [ -n "${DISK_HEALTH_DETAILS[$d]}" ]; then
					echo -e "    └─ ${DISK_HEALTH_DETAILS[$d]}"
				fi
			fi
		done
		echo
	fi

	# Wiped disks section
	echo -e "${GREEN}Wiped disks (${#WIPED[@]}):${RESET}"
	if [ ${#WIPED[@]} -gt 0 ]; then
		for d in "${WIPED[@]}"; do
			echo -e "${GREEN}  ✓ $d${RESET}"
		done
	else
		echo -e "${YELLOW}  None${RESET}"
	fi

	echo

	echo -e "${RED}Failed wipes (${#FAILED[@]}):${RESET}"
	if [ ${#FAILED[@]} -gt 0 ]; then
		for d in "${FAILED[@]}"; do
			echo -e "${RED}  ✗ $d${RESET}"
		done
	else
		echo -e "${GREEN}  None${RESET}"
	fi

	echo

	echo -e "${BLUE}Ignored disks (${#IGNORED_DISKS[@]}):${RESET}"
	if [ ${#IGNORED_DISKS[@]} -gt 0 ]; then
		for d in "${IGNORED_DISKS[@]}"; do
			echo -e "${BLUE}  - $d${RESET}"
		done
	else
		echo -e "${YELLOW}  None${RESET}"
	fi

	echo -e "${GREEN}=========================================${RESET}"
}

##################################################
# Usage
##################################################

usage() {
	cat <<-EOF
		Usage: $0 [options]

		Options:
		  --help              Show this help message and exit
		  --zero              Zero the drives using dd and /dev/zero
		  --random            Fill drives with random data using dd and /dev/urandom
		  --nvme-secure       Use NVMe secure erase (sanitize revert) for NVMe disks instead of dd
		  --log-level LEVEL   Set stdout log level (DEBUG|INFO|WARN|ERR) [default: INFO]
		  --log-file-level LEVEL  Set file log level (DEBUG|INFO|WARN|ERR) [default: DEBUG]

		Notes:
		  - --random and --zero can be combined; random filling happens first, then zeroing.
		  - --nvme-secure only applies to NVMe disks and replaces dd operations.
		  - Without any flags, only filesystem signatures are wiped using wipefs.
		  - Ensure you have 'nvme-cli' installed for NVMe secure erase.
		  - Health checks are performed before wiping using SMART (smartmontools) and NVMe smart-log.
		  - For SSDs, secure erase is preferred for speed and endurance.
		  - dd exits with code 1 when the disk is full - this is normal and treated as success.
		  - Logs are written to: $LOG_FILE
	EOF
}

##################################################
# Main
##################################################

# Parse command-line arguments
while [[ $# -gt 0 ]]; do
	case $1 in
	--help)
		usage
		exit 0
		;;
	--zero)
		ZERO_FILL="true"
		shift
		;;
	--random)
		RANDOM_FILL="true"
		shift
		;;
	--nvme-secure)
		NVME_SECURE="true"
		shift
		;;
	--log-level)
		LOG_LEVEL="${2^^}"
		if [[ ! ${LOG_LEVELS[$LOG_LEVEL]+isset} ]]; then
			echo "ERROR: Invalid log level: $2"
			usage
			exit 1
		fi
		shift 2
		;;
	--log-file-level)
		LOG_FILE_LEVEL="${2^^}"
		if [[ ! ${LOG_LEVELS[$LOG_FILE_LEVEL]+isset} ]]; then
			echo "ERROR: Invalid log file level: $2"
			usage
			exit 1
		fi
		shift 2
		;;
	*)
		echo "ERROR: Unknown option: $1"
		usage
		exit 1
		;;
	esac
done

# Initialize log file
cat <<-EOF >"$LOG_FILE"
	========================================
	Disk Wipe Utility - Log File
	Started: $(date '+%Y-%m-%d %H:%M:%S')
	Log Level (stdout): $LOG_LEVEL
	Log Level (file): $LOG_FILE_LEVEL
	========================================
EOF

detect_os || {
	log ERR "Failed to detect OS."
	exit 1
}

install_deps || {
	log ERR "Failed to install dependencies."
	exit 1
}

log INFO "Starting disk wipe script"
if ! get_confirmation; then
	log INFO "Aborted by user."
	exit 0
fi

get_disks || {
	log ERR "Failed to get disks"
	exit 1
}

# Run health check before wiping
health_check

log INFO "Detected ${#DISKS_TO_WIPE[@]} disk(s) to wipe: ${DISKS_TO_WIPE[*]+"${DISKS_TO_WIPE[*]}"}"
log INFO "Detected ${#IGNORED_DISKS[@]} ignored disk(s): ${IGNORED_DISKS[*]+"${IGNORED_DISKS[*]}"}"

wipe_all_disks || {
	log ERR "Failed to wipe disks."
	exit 1
}

print_summary || {
	log ERR "Failed to print summary."
	exit 1
}
