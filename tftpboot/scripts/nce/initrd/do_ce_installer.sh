#!/bin/sh

##################################################
# NCE Installer
##################################################

export COMMUNITY_EDITION=1

CE_NET_INFO=""
if [ -f /mnt/stage/root/.host_net_info ]; then
	temp="$(cat /mnt/stage/root/.host_net_info)"
	CE_NET_INFO="$temp"
fi
if [ -f /mnt/stage/root/.cvm_net_info ]; then
	CE_NET_INFO="$CE_NET_INFO "
	temp="$(cat /mnt/stage/root/.cvm_net_info)"
	CE_NET_INFO="$CE_NET_INFO$temp"
fi
if [ -n "$CE_NET_INFO" ]; then
	CE_INSTALLED="$CE_NET_INFO"
	export CE_INSTALLED
fi

# shellcheck disable=SC1091
. "${HOME}/ce_functions.sh" || {
	echo "Failed to source required CE functions"
	exit 1
}

# If loaded over iPXE,download the ISO contents into /mnt/iso
if [ "$(extract_boot_param CE_IPXE)" = "1" ]; then
	PHOENIX_BASE="$(extract_boot_param PHOENIX_BASE)"

	# The default ISO mount location.
	ISO_MOUNT="/mnt/local"
	# Community Edition has different logic.
	ISO_MOUNT_CE="/mnt/iso"

	if [ -z "$PHOENIX_BASE" ]; then
		logger ERROR "Unable to download ISO contents: PHOENIX_BASE parameter was not found"
		exit 1
	fi

	logger INFO "CE iPXE: Downloading ISO contents from $PHOENIX_BASE to $ISO_MOUNT_CE"
	logger WARN "CE iPXE: This part takes a while..."

	mkdir -p "$ISO_MOUNT" || {
		logger ERROR "Failed to create $ISO_MOUNT directory"
		exit 1
	}

	mkdir -p "$ISO_MOUNT_CE" || {
		logger ERROR "Failed to create $ISO_MOUNT_CE directory"
		exit 1
	}

	download_path_into_ce "${PHOENIX_BASE}" "$ISO_MOUNT_CE" || {
		logger ERROR "Failed to download ISO contents"
		exit 1
	}

	# Create the required flag files in all locations.
	FLAG_FILES="$ISO_MOUNT/.prepared $ISO_MOUNT_CE/.prepared /tmp/phoenix_iso_marker"

	for flag in $FLAG_FILES; do
		touch "$flag" || {
			logger ERROR "Failed to create the required prepared file flag at $flag"
			exit 1
		}
	done

	logger INFO "Directory ${ISO_MOUNT} size: ($(du -sh ${ISO_MOUNT}))"
	logger INFO "Directory ${ISO_MOUNT_CE} size: ($(du -sh ${ISO_MOUNT_CE}))"

fi

rm -f /mnt/stage/root/.ce_install_success
sh /root/do_installer.sh
