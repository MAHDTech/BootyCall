export COMMUNITY_EDITION=1

CE_NET_INFO=""
if [ -f /mnt/stage/root/.host_net_info ]; then
	CE_NET_INFO=$(cat /mnt/stage/root/.host_net_info)
fi
if [ -f /mnt/stage/root/.cvm_net_info ]; then
	CE_NET_INFO=$CE_NET_INFO" "
	CE_NET_INFO=$CE_NET_INFO$(cat /mnt/stage/root/.cvm_net_info)
fi
if [ ! -z "$CE_NET_INFO" ]; then
	export CE_INSTALLED=$(echo $CE_NET_INFO)
fi

##################################################
# HACK: Modification for CE with iPXE.
##################################################

# shellcheck disable=SC1091
. "${HOME}/ce_functions.sh" || {
	echo "Failed to source required CE functions"
	exit 1
}

# Download the required images into the overlayfs before running the installer.
download_images_into_ce || {
	echo "Failed to download required images"
	exit 1
}
echo "Required images have been downloaded successfully"
##################################################

rm -f /mnt/stage/root/.ce_install_success
sh /root/do_installer.sh
