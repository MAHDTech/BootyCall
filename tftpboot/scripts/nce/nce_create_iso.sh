#!/usr/bin/env bash

clear
set -euo pipefail

##################################################
# Name: nce_create_iso.sh
# Description: Nutanix Community Edition iPXE ISO creator
# NOTES:
#   - Based on the docs/NCE_iso.md file.
##################################################

##################################################
# Variables
##################################################

SCRIPT_NAME=${0##*/}
SCRIPT_DIR="$(cd "${BASH_SOURCE[0]%/*}" && pwd)"
ENV_FILE="${SCRIPT_DIR}/../../../.env"

function usage() {
	cat <<-EOF
		${SCRIPT_NAME} is designed to run from the root of the BootyCall project.

		When run, it will create a custom Nutanix Community Edition ISO for use with iPXE.

		Usage: ${SCRIPT_NAME} [options]

		Options:
		    -h, --help            Display this help message and exit

		Variables:

		    Create a .env file in the root directory of the project using the following variables names.

		    If you use devenv, this file will be sourced automatically, otherwise export these in your shell environment.

		    NCE_ISO_NAME                         The filename of the Nutanix Community Edition ISO
		    NCE_ISO_NAME_IPXE                    The filename of the Nutanix Community Edition ISO for iPXE

		    RSYNC_ENABLED                        If enabled will rsync the created ISO to a remote server. (If you are not creating the ISO directly on the iPXE server, you will need to enable this)
		    RSYNC_USER                           The username to use when syncing files
		    RSYNC_OPTS                           The options to use when syncing files
		    RSYNC_REMOTE_USER                    Optional: Set ownership permissions for the files copied to this user.
		    RSYNC_REMOTE_GROUP                   Optional: Set ownership permissions for the files copied to this group.

		    IPXE_HOST                            The IP or DNS address of the iPXE server
		    IPXE_HOST_ISO_DIR                    The directory where the Nutanix Community Edition ISO is located on the iPXE server
		    IPXE_HOST_ISO_EXTRACTED_DIR          The directory where the Nutanix Community Edition ISO is extracted on the iPXE server

		    LOG_SCREEN_LEVEL                     The log level for the screen output
		    LOG_FILE_LEVEL                       The log level for the log file

		Examples:

		    ${SCRIPT_NAME} --help

		    ${SCRIPT_NAME}
	EOF
}

if [[ -f ${ENV_FILE} ]]; then
	# shellcheck disable=SC1090
	. "${ENV_FILE}" || {
		echo "ERROR sourcing ${ENV_FILE}, please check the file syntax"
		exit 1
	}
else
	usage
	exit 1
fi

#########################
# Variable validation
#########################

: "${NCE_ISO_NAME:?ERROR: NCE_ISO_NAME required}"
: "${NCE_ISO_NAME_IPXE:?ERROR: NCE_ISO_NAME_IPXE required}"

: "${IPXE_HOST:?ERROR: IPXE_HOST required}"
: "${IPXE_HOST_ISO_DIR:?ERROR: IPXE_HOST_ISO_DIR required}"
: "${IPXE_HOST_ISO_EXTRACTED_DIR:?ERROR: IPXE_HOST_ISO_EXTRACTED_DIR required}"

#########################
# Variable derived variables from .env
#########################

# Paths relative to the project root.
TFTPBOOT_DIR="$(pwd)/tftpboot"
SCRIPTS_DIR="${TFTPBOOT_DIR}/scripts/nce"

# The folders where the ISO and extracted ISO files are stored
LOCAL_ISO_DIR="${TFTPBOOT_DIR}/iso"
LOCAL_ISO_EXTRACTED_DIR="${TFTPBOOT_DIR}/iso-extracted"

# Paths to the created ISO files locally.
LOCAL_ISO_PATH="${LOCAL_ISO_DIR}/${NCE_ISO_NAME}"
LOCAL_ISO_PATH_IPXE="${LOCAL_ISO_DIR}/${NCE_ISO_NAME_IPXE}"

# Logging setup.
LOG_FILE="/tmp/${SCRIPT_NAME}.log"
# Use level numbers for easier comparison.
get_log_level_num() {
	case "$1" in
	DEBUG) echo 1 ;;
	INFO) echo 2 ;;
	WARN) echo 3 ;;
	ERROR) echo 4 ;;
	*) echo 3 ;;
	esac
}
LOG_SCREEN_LEVEL_NUM="$(get_log_level_num "${LOG_SCREEN_LEVEL:-WARN}")"
LOG_FILE_LEVEL_NUM="$(get_log_level_num "${LOG_FILE_LEVEL:-INFO}")"

# Rsync
RSYNC_ENABLED="${RSYNC_ENABLED:-false}"
# Set default options for rsync if not specified
RSYNC_OPTS="${RSYNC_OPTS:--avz --progress --partial --inplace}"
RSYNC_USER="${RSYNC_USER:-root}"
RSYNC_REMOTE_USER="${RSYNC_REMOTE_USER:-}"
RSYNC_REMOTE_GROUP="${RSYNC_REMOTE_GROUP:-}"

# Temporary directory for intermediate files which is created as needed.
declare NCE_TEMP

##################################################
# Constants
##################################################

DEPS=(
	mount
	rsync
	zcat
	cpio
	gzip
	sha256sum
	tee
	sed
	find
)

# Temporary directory names.
TEMP_ISO_EXTRACTED="iso-extracted"
TEMP_INITRD_EXTRACTED="initrd-extracted"

# The path to grub.cfg relative to the extracted ISO directory.
GRUB_CFG="EFI/BOOT/grub.cfg"

##################################################
# Functions
##################################################

function cleanup() {
	logger DEBUG "Starting cleanup operations on ${NCE_TEMP}"

	# Failsafe: Try to unmount any iso.XXXXXX inside.
	while IFS='' read -r DIR; do
		if [[ -n ${DIR} ]]; then
			logger DEBUG "Attempting unmount on ${DIR}"
			sudo umount "${DIR}" || {
				logger ERROR "Failed to unmount ${DIR}"
				return 1
			}
		fi
	done < <(find "${NCE_TEMP}" -type d -name 'iso.*')

	# Make sure not inside the temp directory when its removed.
	cd "${HOME}" || {
		logger ERROR "Failed to change directory to ${HOME}"
		return 1
	}
	sudo rm -rf "${NCE_TEMP}" || {
		logger ERROR "Failed to remove ${NCE_TEMP}, please cleanup manually."
		return 1
	}
	logger DEBUG "Cleanup complete"
}

function logger() {
	local LEVEL="$1"
	shift
	local MESSAGE="$*"

	case "${LEVEL}" in
	DEBUG) LEVEL_NUM=1 ;;
	INFO) LEVEL_NUM=2 ;;
	WARN) LEVEL_NUM=3 ;;
	ERROR) LEVEL_NUM=4 ;;
	*) return 0 ;;
	esac

	# Log to screen without the timestamp.
	if [[ ${LEVEL_NUM} -ge ${LOG_SCREEN_LEVEL_NUM} ]]; then
		echo "[${LEVEL}] ${MESSAGE}"
	fi

	# Log to file including a timestamp.
	if [[ ${LEVEL_NUM} -ge ${LOG_FILE_LEVEL_NUM} ]]; then
		printf '%s [%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "${LEVEL}" "${MESSAGE}" >>"${LOG_FILE}"
	fi
}

function check_deps() {
	logger INFO "Checking dependencies..."
	for DEP in "${DEPS[@]}"; do
		command -v "${DEP}" >/dev/null || {
			logger ERROR "Missing dependency: ${DEP}"
			return 1
		}
	done
	logger DEBUG "All deps OK"
}

function check_vars() {
	logger INFO "Validating config vars..."
	[ -f "${LOCAL_ISO_PATH}" ] || {
		logger ERROR "LOCAL_ISO_PATH was not found: ${LOCAL_ISO_PATH}"
		return 1
	}
	[ -d "${SCRIPTS_DIR}" ] || {
		logger ERROR "SCRIPTS_DIR invalid: ${SCRIPTS_DIR}"
		return 1
	}
	logger DEBUG "Vars OK"
}

function setup_temp() {
	logger INFO "Creating temp workspace..."
	NCE_TEMP="$(mktemp -d /tmp/nce.XXXXXX)" || {
		logger ERROR "Failed to create temp dir"
		return 1
	}
	export NCE_TEMP

	mkdir -p \
		"${NCE_TEMP}/${TEMP_ISO_EXTRACTED}" \
		"${NCE_TEMP}/${TEMP_INITRD_EXTRACTED}" || {
		logger ERROR "Failed to create dirs"
		return 1
	}

	trap cleanup EXIT INT TERM HUP
	logger DEBUG "Temp setup: ${NCE_TEMP}"
}

function extract_iso() {
	local ISO_SOURCE=$1
	local ISO_MOUNT
	local ISO_DEST=$2

	logger DEBUG "Creating temporary mount point"
	ISO_MOUNT="$(mktemp -d "${NCE_TEMP}/iso.XXXXXX")" || {
		logger ERROR "Failed to create temp dir"
		return 1
	}

	logger INFO "Mounting ISO file $ISO_SOURCE"
	sudo mount -o loop "${ISO_SOURCE}" "${ISO_MOUNT}" || {
		logger ERROR "Failed to mount ISO"
		return 1
	}

	# Ensure the destination directory exists
	mkdir -p "${ISO_DEST}" || {
		logger ERROR "Failed to create destination directory"
		return 1
	}

	logger INFO "Extracting ISO contents from ${ISO_SOURCE}"
	sudo rsync \
		--archive \
		--delete \
		"${ISO_MOUNT}/" \
		"${ISO_DEST}/" ||
		{
			logger ERROR "Failed to rsync ISO contents from ${ISO_SOURCE}/ to ${ISO_DEST}/"
			return 1
		}

	logger INFO "Unmounting ISO file $ISO_SOURCE"
	sudo umount "${ISO_MOUNT}" || {
		logger ERROR "Failed to unmount original ISO"
		return 1
	}

	logger DEBUG "Removing temporary mount point"
	rm -rf "${ISO_MOUNT}" || {
		logger WARN "Failed to remove temporary mount point, will retry during cleanup operations."
	}

	logger DEBUG "ISO extraction complete for file $ISO_SOURCE"
}

function modify_grub_cfg() {
	local GRUB_CFG_PATH="${NCE_TEMP}/${TEMP_ISO_EXTRACTED}/${GRUB_CFG}"
	logger INFO "Modifying grub.cfg..."
	cat <<-EOF | sudo tee "${GRUB_CFG_PATH}" >/dev/null
		set default="CEInstaller iPXE"

		insmod part_gpt
		insmod part_msdos

		set timeout=30

		search --no-floppy --set=root -l 'PHOENIX'

		menuentry 'CEInstaller' {
		    linuxefi /boot/kernel init=/ce_installer intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs
		    initrdefi /boot/initrd
		}

		menuentry 'CEInstaller iPXE' {
		    linuxefi /boot/kernel init=/ce_installer intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs mpt3sas.prot_mask=1 LIVEFS_URL=http://__HOST__/iso-extracted/__SUBDIR__/squashfs.img PHOENIX_BASE=http://__HOST__/iso-extracted/__SUBDIR__ PHOENIX_ISO=http://__HOST__/iso/__BASENAME__ UPDATES_CONFIG_URL=http://__HOST__/iso-extracted/__SUBDIR__/updates_config.json rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.debug CE_IPXE=1
		    initrdefi /boot/initrd
		}

		menuentry 'Debug Shell for Stage 1 (initramfs)' {
		    linuxefi /boot/kernel intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 mpt3sas.prot_mask=1 LIVEFS_URL=http://__HOST__/iso-extracted/__SUBDIR__/squashfs.img PHOENIX_BASE=http://__HOST__/iso-extracted/__SUBDIR__ PHOENIX_ISO=http://__HOST__/iso/__BASENAME__ rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.shell=1 rd.break=pre-mount rd.debug
		    initrdefi /boot/initrd
		}

		menuentry 'Debug Shell for Stage 2 (squashfs)' {
		    linuxefi /boot/kernel init=/usr/bin/bash intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs mpt3sas.prot_mask=1 LIVEFS_URL=http://__HOST__/iso-extracted/__SUBDIR__/squashfs.img PHOENIX_BASE=http://__HOST__/iso-extracted/__SUBDIR__ PHOENIX_ISO=http://__HOST__/iso/__BASENAME__ rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.shell=1 rd.break=mount rd.debug
		    initrdefi /boot/initrd
		}

	EOF

	# The iPXE hostname.
	sudo sed -i "s/__HOST__/${IPXE_HOST}/g" "${GRUB_CFG_PATH}"

	# The extracted iso sub-directory where the iso files live.
	sudo sed -i "s/__SUBDIR__/phoenix/g" "${GRUB_CFG_PATH}"

	# The modified ISO name.
	sudo sed -i "s/__BASENAME__/${NCE_ISO_NAME_IPXE}/g" "${GRUB_CFG_PATH}"

	logger DEBUG "grub.cfg modified"

}

function unpack_initrd() {
	logger INFO "Unpacking initrd..."
	mkdir -p "${NCE_TEMP}/${TEMP_INITRD_EXTRACTED}"
	(cd "${NCE_TEMP}/${TEMP_INITRD_EXTRACTED}" && zcat "${NCE_TEMP}/${TEMP_ISO_EXTRACTED}/boot/initrd" | cpio -idmv) || return 1
	logger DEBUG "Initrd unpacked"
}

function copy_scripts() {
	local CUSTOM_SCRIPTS

	logger INFO "Copying custom scripts to initrd..."
	CUSTOM_SCRIPTS=(
		livecd.sh
		do_ce_installer.sh
		ce_functions.sh
	)

	for SCRIPT in "${CUSTOM_SCRIPTS[@]}"; do
		[[ -f "${SCRIPTS_DIR}/${SCRIPT}" ]] || {
			logger ERROR "Missing script: ${NCE_SCRIPTS_DIR}/${SCRIPT}"
			return 1
		}
		cp -fv "${SCRIPTS_DIR}/${SCRIPT}" "${NCE_TEMP}/${TEMP_INITRD_EXTRACTED}/${SCRIPT}" || {
			logger ERROR "Failed to copy script: ${SCRIPT}"
			return 1
		}
	done
	logger DEBUG "Custom scripts copied"
}

function repack_initrd() {
	local OLD_INITRD
	local NEW_INITRD

	logger INFO "Repacking initrd..."
	pushd "${NCE_TEMP}/${TEMP_INITRD_EXTRACTED}" || {
		logger ERROR "Failed to change directory to ${NCE_TEMP}/${TEMP_INITRD_EXTRACTED}"
		return 1
	}
	OLD_INITRD=$(sha256sum "${NCE_TEMP}/${TEMP_ISO_EXTRACTED}/boot/initrd" | awk '{print $1}')

	find . -print0 | cpio --null -o --format=newc | gzip -9 | sudo tee "${NCE_TEMP}/${TEMP_ISO_EXTRACTED}/boot/initrd" >/dev/null

	NEW_INITRD=$(sha256sum "${NCE_TEMP}/${TEMP_ISO_EXTRACTED}/boot/initrd" | awk '{print $1}')

	logger INFO "Old initrd hash: ${OLD_INITRD}"
	logger INFO "New initrd hash: ${NEW_INITRD}"

	popd || {
		logger ERROR "Failed to pop directory"
		return 1
	}
	logger DEBUG "Initrd repacked"
}

function rebuild_iso() {
	local ISO_BASE
	local ISO_OUTPUT
	local ISO_DETAILS

	ISO_BASE="${NCE_ISO_NAME_IPXE%.iso}"
	ISO_OUTPUT="${NCE_TEMP}/${ISO_BASE}.iso"

	logger INFO "Rebuilding ISO..."

	pushd "${NCE_TEMP}/${TEMP_ISO_EXTRACTED}" || {
		logger ERROR "Failed to change directory to ${NCE_TEMP}/${TEMP_ISO_EXTRACTED}"
		return 1
	}

	sudo chmod +x make_iso.sh
	# The make_iso.sh script will drop the ISO into the ${NCE_TEMP} directory.
	sudo ./make_iso.sh "${ISO_BASE}" || {
		logger ERROR "Failed to rebuild ISO"
		return 1
	}

	if [[ -f ${ISO_OUTPUT} ]]; then
		ISO_DETAILS=$(file "${ISO_OUTPUT}")
		logger INFO "ISO has been created, logging file details"
		logger INFO "${ISO_DETAILS}"
		mv "${ISO_OUTPUT}" "${LOCAL_ISO_PATH_IPXE}" || {
			logger ERROR "Failed to move ISO to ${LOCAL_ISO_PATH_IPXE}"
			return 1
		}
		logger INFO "ISO saved: ${LOCAL_ISO_PATH_IPXE}"
	else
		logger ERROR "Failed to find generated ISO: ${ISO_OUTPUT}"
		return 1
	fi
	logger DEBUG "ISO rebuilt"
}
function rsync_files() {
	local SOURCE=$1
	local DEST=$2

	# Determine the remote permissions if either the remote user or group is set
	if [[ -n ${RSYNC_REMOTE_USER} || -n ${RSYNC_REMOTE_GROUP} ]]; then
		RSYNC_CHOWN="--chown=${RSYNC_REMOTE_USER:-}${RSYNC_REMOTE_GROUP:+:${RSYNC_REMOTE_GROUP}}"
	fi

	if [[ ${RSYNC_ENABLED,,} == "true" ]]; then
		logger INFO "syncing files from ${SOURCE} to ${DEST} using rsync with options ${RSYNC_OPTS} --delete"
		eval "rsync ${RSYNC_OPTS} --delete ${RSYNC_CHOWN:-} '${SOURCE}' '${RSYNC_USER}@${DEST}'" || {
			logger ERROR "Failed to sync files from ${SOURCE} to ${DEST}"
			return 1
		}
	else
		logger INFO "rsync is disabled. Set RSYNC_ENABLED=true if you want to sync to a remote server"
	fi
	logger DEBUG "Files synced"
}

function parse_args() {
	while [[ $# -gt 0 ]]; do
		case "$1" in
		-h | --help)
			usage
			return 0
			;;
		*)
			echo "Unknown option: $1" >&2
			usage >&2
			return 1
			;;
		esac
		# Enable when more args added.
		#shift # Move to the next argument
	done
}

##################################################
# Main
##################################################

logger INFO "Starting ${SCRIPT_NAME}"

parse_args "$@" || {
	logger ERROR "Failed to parse arguments"
	exit 1
}

check_deps || {
	logger ERROR "Failed to check dependencies"
	exit 2
}

check_vars || {
	logger ERROR "Failed to check variables"
	exit 3
}

setup_temp || {
	logger ERROR "Failed to setup temporary directory"
	exit 4
}

extract_iso \
	"${LOCAL_ISO_PATH}" \
	"${NCE_TEMP}/${TEMP_ISO_EXTRACTED}" ||
	{
		logger ERROR "Failed to extract ISO"
		exit 5
	}

modify_grub_cfg || {
	logger ERROR "Failed to modify GRUB configuration"
	exit 6
}

unpack_initrd || {
	logger ERROR "Failed to unpack initrd"
	exit 7
}

copy_scripts || {
	logger ERROR "Failed to copy scripts"
	exit 8
}

repack_initrd || {
	logger ERROR "Failed to repack initrd"
	exit 9
}

rebuild_iso || {
	logger ERROR "Failed to rebuild ISO"
	exit 10
}

extract_iso \
	"${LOCAL_ISO_PATH_IPXE}" \
	"${LOCAL_ISO_EXTRACTED_DIR}/phoenix" ||
	{
		logger ERROR "Failed to extract ISO"
		exit 5
	}

# Rsync the new ISO file.
rsync_files \
	"${LOCAL_ISO_PATH_IPXE}" \
	"${IPXE_HOST}:${IPXE_HOST_ISO_DIR}/${NCE_ISO_NAME_IPXE}" ||
	{
		logger ERROR "Failed to sync files from ${LOCAL_ISO_PATH_IPXE} to ${IPXE_HOST}:${IPXE_HOST_ISO_DIR}/"
		exit 11
	}

# Rsync the new ISO file (extracted)
rsync_files \
	"${LOCAL_ISO_EXTRACTED_DIR}/phoenix/" \
	"${IPXE_HOST}:${IPXE_HOST_ISO_EXTRACTED_DIR}/" ||
	{
		logger ERROR "Failed to sync files from ${LOCAL_ISO_PATH_IPXE} to ${IPXE_HOST}:${IPXE_HOST_ISO_DIR}/"
		exit 12
	}

logger INFO "Success! iPXE ISO ready at ${LOCAL_ISO_PATH_IPXE}"
