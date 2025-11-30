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

# Download the required images into the overlayfs before running the installer.
download_images_into_ce || {
	logger ERROR "Failed to download required images"
	exit 1
}
echo "Required images have been downloaded successfully"

rm -f /mnt/stage/root/.ce_install_success
sh /root/do_installer.sh
