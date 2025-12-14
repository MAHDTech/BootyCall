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
declare -A DISK_IDS=()

# ANSI color codes (global constants)
RESET='\e[0m'
BLUE='\e[34m'   # DEBUG
GREEN='\e[32m'  # INFO
YELLOW='\e[33m' # WARN
RED='\e[31m'    # ERR

# A rudimentary phase tracker.
declare PHASE=0

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
		log WARN "Terminating ${#ACTIVE_PIDS[@]} active background process(es)..."

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

	# Wipe filesystem signatures in parallel
	PHASE=$((PHASE + 1))
	print_header "Phase ${PHASE}: Wiping filesystem signatures on all disks" "$GREEN"
	log DEBUG "Starting wipefs on ${#DISKS_TO_WIPE[@]} disk(s)"
	pids=()
	pid_to_disk=()
	ACTIVE_PIDS=()
	index=0
	for disk in "${DISKS_TO_WIPE[@]}"; do
		(
			# Disable errexit in subshell to handle errors explicitly
			set +e
			log DEBUG "Wiping filesystem signatures on $disk"
			if sudo wipefs -af "$disk" >/dev/null 2>&1; then
				log INFO "✓ Successfully wiped filesystem signatures on $disk"
				exit 0
			else
				log ERR "✗ Failed to wipe filesystem signatures on $disk"
				exit 1
			fi
		) &
		pids+=($!)
		ACTIVE_PIDS+=($!)
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

	# Clear active PIDs after completion
	ACTIVE_PIDS=()

	# NVMe secure erase (if enabled) in parallel for eligible disks
	if [ "$NVME_SECURE" = "true" ]; then
		PHASE=$((PHASE + 1))
		print_header "Phase ${PHASE}: NVMe secure erase on eligible disks" "$BLUE"
		log DEBUG "Starting NVMe secure erase"
		pids=()
		pid_to_disk=()
		ACTIVE_PIDS=()
		index=0
		for disk in "${DISKS_TO_WIPE[@]}"; do
			if [[ $disk =~ ^/dev/nvme ]] && [ "${disk_status[$disk]}" = "success" ]; then
				(
					set +e
					log DEBUG "Performing NVMe secure erase on $disk using '${nvme_sanitize_args[*]}'"
					if sudo nvme sanitize "$disk" "${nvme_sanitize_args[@]}" >/dev/null 2>&1; then
						log INFO "✓ Successfully performed NVMe secure erase on $disk"
						exit 0
					else
						log ERR "✗ Failed to perform NVMe secure erase on $disk"
						exit 1
					fi
				) &
				pids+=($!)
				ACTIVE_PIDS+=($!)
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

		# Clear active PIDs after completion
		ACTIVE_PIDS=()
	fi

	# Collect disks that need dd (non-NVMe or no secure erase, and still successful)
	dd_disks=()
	for disk in "${DISKS_TO_WIPE[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ] && ! { [[ $disk =~ ^/dev/nvme ]] && [ "$NVME_SECURE" = true ]; }; then
			dd_disks+=("$disk")
		fi
	done

	# Random fill in parallel (if enabled)
	if [ "$RANDOM_FILL" = "true" ] && [ ${#dd_disks[@]} -gt 0 ]; then
		PHASE=$((PHASE + 1))
		print_header "Phase ${PHASE}: Random data fill on ${#dd_disks[@]} disk(s): ${dd_disks[*]}" "$YELLOW"
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

	# Zero fill in parallel (if enabled)
	if [ "$ZERO_FILL" = "true" ] && [ ${#zero_disks[@]} -gt 0 ]; then
		PHASE=$((PHASE + 1))
		print_header "Phase ${PHASE}: Zero fill on ${#zero_disks[@]} disk(s): ${zero_disks[*]}" "$BLUE"
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

	print_header "Disks operations complete, displaying status" "$GREEN"
	# Build wiped/failed lists for summary
	for disk in "${DISKS_TO_WIPE[@]}"; do
		if [ "${disk_status[$disk]}" = "success" ]; then
			WIPED+=("$disk")
			log INFO "✓ All operations completed successfully on disk: $disk"
		else
			FAILED+=("$disk")
			log ERR "✗ Failed to perform all operations on disk: $disk"
		fi
	done
}

##################################################
# Health Check Functions
##################################################

# Helper: Update overall health status (only escalate, never downgrade)
update_health_status() {
	local current_status=$1
	local new_status=$2

	# Priority: CRITICAL > FAILED > WARNING > HEALTHY > UNKNOWN
	if [ "$new_status" = "CRITICAL" ] || [ "$current_status" = "UNKNOWN" ]; then
		echo "$new_status"
	elif [ "$new_status" = "WARNING" ] && [ "$current_status" != "CRITICAL" ] && [ "$current_status" != "FAILED" ]; then
		echo "$new_status"
	else
		echo "$current_status"
	fi
}

# Helper: Set health_color based on health_status
set_health_color() {
	local status=$1

	case "$status" in
	"CRITICAL" | "FAILED")
		echo "$RED"
		;;
	"WARNING")
		echo "$YELLOW"
		;;
	"HEALTHY")
		echo "$GREEN"
		;;
	*)
		echo "$YELLOW"
		;;
	esac
}

# Helper: Add metric with status to detail_parts array
# Usage: add_metric "Label" "value" "threshold_check_result" "optional_unit"
add_metric() {
	local label=$1
	local value=$2
	local status=$3
	local unit=${4:-}

	local display_value="${value}${unit}"

	if [ -z "$value" ] || [ "$value" = "N/A" ]; then
		detail_parts+=("${label}: N/A ${BLUE}(INFO)${RESET}")
	else
		case "$status" in
		"CRITICAL")
			detail_parts+=("${label}: ${display_value} ${RED}(CRITICAL)${RESET}")
			;;
		"WARNING")
			detail_parts+=("${label}: ${display_value} ${YELLOW}(WARNING)${RESET}")
			;;
		"INFO")
			detail_parts+=("${label}: ${display_value} ${BLUE}(INFO)${RESET}")
			;;
		"HEALTHY" | *)
			detail_parts+=("${label}: ${display_value} ${GREEN}(HEALTHY)${RESET}")
			;;
		esac
	fi
}

# Helper: Check error count metric (>0 = WARNING)
check_error_count() {
	local value=$1
	local label=$2
	local trigger_warning=${3:-true}

	if [ -n "$value" ] && [ "$value" -gt 0 ]; then
		if [ "$trigger_warning" = "true" ]; then
			health_status=$(update_health_status "$health_status" "WARNING")
			add_metric "$label" "$value" "WARNING"
		else
			add_metric "$label" "$value" "INFO"
		fi
	elif [ -n "$value" ]; then
		add_metric "$label" "$value" "HEALTHY"
	else
		add_metric "$label" "N/A" "INFO"
	fi
}

# Helper: Check temperature with thresholds
check_temperature() {
	local temp=$1

	if [ -n "$temp" ]; then
		if [ "$temp" -gt 70 ]; then
			health_status=$(update_health_status "$health_status" "WARNING")
			add_metric "Temp" "$temp" "WARNING" "°C"
		elif [ "$temp" -gt 60 ]; then
			add_metric "Temp" "$temp" "INFO" "°C"
		else
			add_metric "Temp" "$temp" "HEALTHY" "°C"
		fi
	else
		add_metric "Temp" "N/A" "INFO" "°C"
	fi
}

# Helper: Parse SMART attribute by ID (RAW value - field 10)
# Uses flexible whitespace matching to support different vendor formats
parse_smart_raw() {
	local smart_output=$1
	local attr_id=$2
	# Strip leading/trailing spaces from attr_id and use flexible regex
	attr_id=$(echo "$attr_id" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
	echo "$smart_output" | grep -E "^[[:space:]]*${attr_id}[[:space:]]" | awk '{print $10}'
}

# Helper: Parse SMART attribute by ID (normalized value - field 4)
# Uses flexible whitespace matching to support different vendor formats
parse_smart_normalized() {
	local smart_output=$1
	local attr_id=$2
	# Strip leading/trailing spaces from attr_id and use flexible regex
	attr_id=$(echo "$attr_id" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
	echo "$smart_output" | grep -E "^[[:space:]]*${attr_id}[[:space:]]" | awk '{print $4}'
}

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
		# NOTE: nvme-cli is in /usr/sbin/nvme so needs to be run with sudo
		if PATH=/usr/sbin:/usr/bin:/sbin:/bin command -v nvme &>/dev/null; then
			# Get smart-log data
			local nvme_output
			if nvme_output=$(sudo nvme smart-log "$disk" 2>/dev/null); then
				# Parse critical warnings
				critical_warning=$(echo "$nvme_output" | grep "critical_warning" | awk '{print $NF}')

				# Parse temperature (get numeric value, not unit)
				temp=$(echo "$nvme_output" | grep "temperature" | head -1 | awk '{print $(NF-1)}')

				# Parse available spare
				available_spare=$(echo "$nvme_output" | grep "available_spare" | head -1 | awk '{print $NF}')

				# Parse percentage used
				percentage_used=$(echo "$nvme_output" | grep "percentage_used" | awk '{print $NF}')

				# Parse power on hours (strip commas to avoid breaking comma-delimited output)
				power_on_hours=$(echo "$nvme_output" | grep "power_on_hours" | awk '{print $NF}' | tr -d ',')

				# Parse media errors
				error_count=$(echo "$nvme_output" | grep "media_errors" | awk '{print $NF}')

				# Determine health status and build details with per-metric status
				# Set baseline to HEALTHY if data parsed successfully
				health_status="HEALTHY"
				local detail_parts=()

				# Check critical warning
				if [ "$critical_warning" != "0" ] && [ -n "$critical_warning" ]; then
					health_status=$(update_health_status "$health_status" "CRITICAL")
					add_metric "Critical Warning" "$critical_warning" "CRITICAL"
				fi

				# Check wear (percentage used)
				if [ -n "$percentage_used" ] && [ "${percentage_used%\%}" -ge 90 ]; then
					health_status=$(update_health_status "$health_status" "WARNING")
					add_metric "Wear" "$percentage_used" "WARNING"
				elif [ -n "$percentage_used" ]; then
					add_metric "Wear" "$percentage_used" "HEALTHY"
				else
					add_metric "Wear" "N/A" "INFO"
				fi

				# Check available spare
				if [ -n "$available_spare" ] && [ "${available_spare%\%}" -lt 10 ]; then
					health_status=$(update_health_status "$health_status" "WARNING")
					add_metric "Spare" "$available_spare" "WARNING"
				elif [ -n "$available_spare" ]; then
					add_metric "Spare" "$available_spare" "HEALTHY"
				else
					add_metric "Spare" "N/A" "INFO"
				fi

				# Check media errors
				check_error_count "$error_count" "Errors"

				# Check temperature
				check_temperature "$temp"

				# Add power on hours
				add_metric "Hours" "${power_on_hours:-N/A}" "INFO"

				# Combine details
				details=$(
					IFS=", "
					echo "${detail_parts[*]}"
				)
			else
				health_status="ERROR"
				details="Wear: Unknown ${RED}(ERROR)${RESET}, Spare: Unknown ${RED}(ERROR)${RESET}, Temp: Unknown ${RED}(ERROR)${RESET}, Hours: Unknown ${RED}(ERROR)${RESET}"
			fi
		else
			health_status="NO_TOOL"
			details="Wear: Unknown ${YELLOW}(NO_TOOL)${RESET}, Spare: Unknown ${YELLOW}(NO_TOOL)${RESET}, Temp: Unknown ${YELLOW}(NO_TOOL)${RESET}, Hours: Unknown ${YELLOW}(NO_TOOL)${RESET}"
		fi
	else
		# SATA/SAS disk - use smartctl
		# NOTE: smartctl is in /usr/sbin/smartctl so needs to be run with sudo
		if PATH=/usr/sbin:/usr/bin:/sbin:/bin command -v smartctl &>/dev/null; then
			local smart_output
			# Don't check exit code - smartctl returns non-zero even with valid data
			# (e.g., bit 6=past errors in log, bit 7=self-test errors)
			smart_output=$(sudo smartctl -H -A "$disk" 2>/dev/null)
			# Check if we got valid output instead of relying on exit code
			if echo "$smart_output" | grep -q "SMART Attributes Data Structure"; then
				# Check overall health status
				if echo "$smart_output" | grep -q "PASSED"; then
					health_status="HEALTHY"
				elif echo "$smart_output" | grep -q "FAILED"; then
					health_status="FAILED"
				fi

				# Parse specific SMART attributes (ID numbers work with flexible whitespace)
				reallocated=$(parse_smart_raw "$smart_output" "5")
				pending=$(parse_smart_raw "$smart_output" "197")

				# Uncorrectable errors (ID 187, 188)
				local uncorrectable
				uncorrectable=$(parse_smart_raw "$smart_output" "187")
				[ -z "$uncorrectable" ] && uncorrectable=$(parse_smart_raw "$smart_output" "188")

				local crc_errors
				crc_errors=$(parse_smart_raw "$smart_output" "199")

				temp=$(parse_smart_raw "$smart_output" "194")
				[ -z "$temp" ] && temp=$(parse_smart_raw "$smart_output" "190")
				wear=$(parse_smart_normalized "$smart_output" "177")
				power_on_hours=$(parse_smart_raw "$smart_output" "9")

				local total_bytes_written
				total_bytes_written=$(parse_smart_raw "$smart_output" "241")

				# Build details string with per-metric status
				local detail_parts=()

				# Check error counts
				check_error_count "$reallocated" "Reallocated"
				check_error_count "$pending" "Pending"
				check_error_count "$uncorrectable" "Uncorrectable"
				check_error_count "$crc_errors" "CRC Errors" "false"

				# Check wear for SSDs
				if [ -n "$wear" ] && [ "$wear" -le 10 ]; then
					health_status=$(update_health_status "$health_status" "WARNING")
					add_metric "Wear" "$wear" "WARNING"
				elif [ -n "$wear" ] && [ "$wear" -le 20 ]; then
					add_metric "Wear" "$wear" "INFO"
				elif [ -n "$wear" ]; then
					add_metric "Wear" "$wear" "HEALTHY"
				fi

				# Check temperature
				check_temperature "$temp"

				# Add power on hours
				add_metric "Hours" "${power_on_hours:-N/A}" "INFO"

				# Add total bytes written for SSDs
				if [ -n "$total_bytes_written" ] && [ "$total_bytes_written" -gt 1000000 ]; then
					local tbw_gb=$((total_bytes_written / 1000))
					add_metric "Written" "${tbw_gb}" "INFO" " GB"
				elif [ -n "$total_bytes_written" ]; then
					add_metric "Written" "${total_bytes_written}" "INFO" " LBAs"
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
				details="Reallocated: Unknown ${RED}(ERROR)${RESET}, Pending: Unknown ${RED}(ERROR)${RESET}, Uncorrectable: Unknown ${RED}(ERROR)${RESET}, Temp: Unknown ${RED}(ERROR)${RESET}, Hours: Unknown ${RED}(ERROR)${RESET}"
			fi
		else
			health_status="NO_TOOL"
			details="Reallocated: Unknown ${YELLOW}(NO_TOOL)${RESET}, Pending: Unknown ${YELLOW}(NO_TOOL)${RESET}, Uncorrectable: Unknown ${YELLOW}(NO_TOOL)${RESET}, Temp: Unknown ${YELLOW}(NO_TOOL)${RESET}, Hours: Unknown ${YELLOW}(NO_TOOL)${RESET}"
		fi
	fi

	# Set color based on final status
	health_color=$(set_health_color "$health_status")

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

	PHASE=$((PHASE + 1))
	print_header "Phase ${PHASE}: Gathering disk health data" "$BLUE"
	log INFO "Running health checks on ${#DISKS_TO_WIPE[@]} disk(s)"

	for disk in "${DISKS_TO_WIPE[@]}"; do
		log DEBUG "Checking health of $disk"
		health_info=$(check_disk_health "$disk")
		status=$(echo "$health_info" | cut -d'|' -f1)
		details=$(echo "$health_info" | cut -d'|' -f2)

		local disk_id
		disk_id=$(get_disk_id "$disk")
		DISK_IDS["$disk"]="$disk_id"

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

get_disk_id() {
	local disk=$1
	local id="N/A"

	if [ -d "/dev/disk/by-id" ]; then
		for link in /dev/disk/by-id/*; do
			if [ -L "$link" ] && [[ ! $link =~ -part[0-9]+$ ]]; then
				local target
				target=$(readlink "$link")
				if [[ $target == */$(basename "$disk") ]] || [ "$(basename "$target")" = "$(basename "$disk")" ]; then
					id=$(basename "$link")
					break
				fi
			fi
		done
	fi

	echo "$id"
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

	echo
	echo -e "${GREEN}==================================================================================${RESET}"
	echo -e "${GREEN}         DISK OPERATIONS SUMMARY${RESET}"
	echo -e "${GREEN}==================================================================================${RESET}"
	echo

	# Disk Health Section
	if [ ${#DISK_HEALTH_STATUS[@]} -gt 0 ]; then
		echo -e "${BLUE}Disk Health Status:${RESET}"
		for d in "${DISKS_TO_WIPE[@]}"; do
			if [ -n "${DISK_HEALTH_STATUS[$d]}" ]; then
				# Extract plain status without color codes
				local plain_status="${DISK_HEALTH_STATUS[$d]}"
				plain_status="${plain_status//$GREEN/}"
				plain_status="${plain_status//$RED/}"
				plain_status="${plain_status//$YELLOW/}"
				plain_status="${plain_status//$BLUE/}"
				plain_status="${plain_status//$RESET/}"

				# Display drive with overall status
				echo -e "  $d: ${DISK_HEALTH_STATUS[$d]}"
				echo -e "    |_ id: ${DISK_IDS[$d]}"
				if [ -n "${DISK_HEALTH_DETAILS[$d]}" ]; then
					# Split by ", " as a single delimiter
					local old_ifs="$IFS"
					IFS=","
					read -ra DETAILS <<<"${DISK_HEALTH_DETAILS[$d]}"
					IFS="$old_ifs"
					for detail in "${DETAILS[@]}"; do
						# Trim leading space from each detail
						detail="${detail# }"
						echo -e "    |_ $detail"
					done
				else
					echo -e "    |_ No details available"
				fi
				echo
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

	echo -e "${GREEN}==================================================================================${RESET}"
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
