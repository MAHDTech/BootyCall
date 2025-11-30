#!/bin/sh

##################################################
#
# Copyright (c) 2012 Nutanix, Inc. All Rights Reserved.
#
# Author: cui@nutanix.com
# manish.sharma@nutanix.com (Added centos/squashfs support)
#
# Entry point to all image related scripts. Mounts livecd and dispatches to
# a different script that does the imaging.
# Log all output to file and console for debugging purpose.
##################################################

##################################################
# Hacked on by MAHDTech@saltlabs.dev to add iPXE support to NCE.
##################################################

##################################################
# Variables
##################################################

log_file="/tmp/phoenix.log"

# Identifying the OS type
if [ -e '/etc/redhat-release' ]; then
	OS_TYPE="Centos"
	HOME="/root"
else
	OS_TYPE="Gentoo"
	HOME="/"
fi

# Source common functions like get_boot_param
# shellcheck disable=SC1091
. "$HOME/common_utils.sh"

# shellcheck disable=SC1091
. "${HOME}/ce_functions.sh" || {
	echo "Failed to source critical NCE functions!"
	exit 1
}

redirect_logs_to_file $log_file

# ENG-197802
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
export TERM=linux
export RED='\033[0;31m'
export RESET='\033[0m'

# Setting core dump file name pattern
sysctl -w kernel.core_pattern=/tmp/core-%e-%s-%t

# Setting core dump file size to unlimited
# shellcheck disable=SC3045
ulimit -c unlimited

ce=0
if [ "${0##*/}" = "ce_installer" ]; then
	ce=1
fi

# max raid rebuild speed set to 5G/sec
echo 5000000 >/proc/sys/dev/raid/speed_limit_max
# min raid rebuild speed set to 500M/sec
echo 500000 >/proc/sys/dev/raid/speed_limit_min

##################################################
# Constants
##################################################

RAMDISK_SZ=${RAMDISK_SZ:-"64G"}
SQUASHFS_DIGEST_x86_64=b38db887dd467372179620d1c70f9300
SQUASHFS_DIGEST_ppc64le=dfec0035fe5fce613e58d3e015d36fbb
LIVEFS_URL="$(get_boot_param LIVEFS_URL)"
PHOENIX_IP="$(get_boot_param PHOENIX_IP)"
MASK="$(get_boot_param MASK)"
FOUND_IP="$(get_boot_param FOUND_IP)"
GATEWAY="$(get_boot_param GATEWAY)"
VLAN="$(get_boot_param VLAN)"
NAMESERVER="$(get_boot_param NAMESERVER)"
NTP_SERVERS="$(get_boot_param NTP_SERVERS)"
# shellcheck disable=SC2034
BOND_MODE="$(get_boot_param BOND_MODE)"
# shellcheck disable=SC2034
BOND_UPLINKS="$(get_boot_param BOND_UPLINKS)"
# shellcheck disable=SC2034
BOND_LACP_RATE="$(get_boot_param BOND_LACP_RATE)"
# shellcheck disable=SC2034
IMG="$(get_boot_param IMG)"
INIT_CMD="$(get_boot_param init)"
USE_CVM_CFG="$(get_boot_param USE_CVM_CFG)"
FC_CONFIG_URL="$(get_boot_param FC_CONFIG_URL)"
COMPUTE_ONLY="$(get_boot_param COMPUTE_ONLY)"
PEM_WORKFLOW="$(get_boot_param PEM_WORKFLOW)"
CVM_HOME_PART_INFO_PATH="/phoenix/.cvm_home_partition_info"
CVM_HOME_MNT=/mnt/cvm_home
CVM_HOME_MARKER1="$CVM_HOME_MNT/nutanix/data/installer"
CVM_HOME_MARKER2="$CVM_HOME_MNT/data/installer"
CVM_HOME_RAID_PART_UUID="$(get_boot_param CVM_HOME_RAID_PART_UUID)"
DISCOVERY_OS="$(get_boot_param DISCOVERY_OS)"
if [ "$VLAN" = 'None' ] || [ "$VLAN" = 0 ]; then
	VLAN=""
fi
PXEBOOT="$(get_boot_param PXEBOOT)"
ifconfig lo 127.0.0.1 netmask 255.0.0.0 up
IMG_FILE=/mnt/local/squashfs.img
if uname -m | grep -q x86_64; then
	IMG_MD5SUM="$SQUASHFS_DIGEST_x86_64"
else
	IMG_MD5SUM="$SQUASHFS_DIGEST_ppc64le"
fi
ipv6_first_hexa=$(echo "$FOUND_IP" | cut -f 1 -d ":")
IPV6=false
if [ "$ipv6_first_hexa" != "$FOUND_IP" ]; then
	IPV6=true
fi
IS_INTERSIGHT="false"
IS_CISCO="false"
CISCO_IPMITOOL="/opt/cisco/ipmitool"
INTERSIGHT_CONFIG="/tmp/cisco_intersight_fc_metadata.json"
INTERSIGHT_CONFIG_SRC="host-init.json"

##################################################
# Functions
##################################################

wait_for_devices() {
	if [ "$PEM_WORKFLOW" = "TRUE" ] || [ "$USE_CVM_CFG" = "true" ]; then
		for try in $(seq 1 6); do
			echo "[$try/6] Waiting for devices to get initialized..."
			sleep 10
		done
	fi
}

find_squashfs_in_disks() {
	paths=${1:-"nutanix/foundation/tmp/phoenix_livecd"}
	echo "Looking for squashfs.img in the existing filesystem of this node"
	[ -d "$CVM_HOME_MNT" ] || mkdir -p "$CVM_HOME_MNT"
	parts="/dev/md* /dev/sd* /dev/nvme*"
	if [ "$COMPUTE_ONLY" != "TRUE" ]; then
		if ! assemble_raid; then
			[ -n "$CVM_HOME_RAID_PART_UUID" ] && drop_to_shell_auto
		fi
		if find_cvm_home_raid_part_by_uuid; then
			cvm_part=$(cat "$CVM_HOME_PART_INFO_PATH")
			parts="$cvm_part $parts"
		fi
	fi
	for part in $parts; do
		[ -e "$part" ] || continue
		if mount "$part" "$CVM_HOME_MNT"; then
			for path in $paths; do
				livecd="$CVM_HOME_MNT/$path/squashfs.img"
				if [ -f "$livecd" ]; then
					echo "squashfs.img found in $part"
					if md5sum "$livecd" | grep -q "$IMG_MD5SUM"; then
						cp "$livecd" /mnt/local
					else
						echo "squashfs.img found in $part at $path but md5sum didn't match"
						continue
					fi
					updates_dir_path="$CVM_HOME_MNT/$path/updates"
					if [ -d "$updates_dir_path" ]; then
						[ -d /root/updates ] || mkdir -p /root/updates
						cp -rf "$updates_dir_path"/* /root/updates
					fi
					if [ -d "$CVM_HOME_MARKER1" ] || [ -d "$CVM_HOME_MARKER2" ]; then
						echo "Storing CVM home partition info in $CVM_HOME_PART_INFO_PATH"
						echo "$part" >"$CVM_HOME_PART_INFO_PATH"
					fi
					umount "$CVM_HOME_MNT"
					return 0
				fi
			done
			umount "$CVM_HOME_MNT"
		fi
	done
	printf 'Phoenix %sfailed%s to load squashfs.img from both the network and the existing CVM filesystem of this node\n' "$RED" "$RESET"
	if [ "$IPV6" != "true" ]; then
		if [ -n "$PHOENIX_IP" ]; then
			echo "The network parameters provided to Phoenix were:"
			echo " > IP: $PHOENIX_IP / Netmask: $MASK / Gateway: $GATEWAY / VLAN: $VLAN"
		fi
	else
		echo "The network parameters provided to Phoenix in IPv6 mode were:"
		echo " > VLAN: $VLAN"
	fi
	drop_to_shell_auto
	return 1
}

find_squashfs_in_iso() {
	echo "Searching for squashfs.img in CDROMs first, then USB devices"
	for try in $(seq 1 15); do
		echo "[$try/15] Searching for a CDROM containing squashfs.img"
		for i in /dev/sr*; do
			[ -e "$i" ] || continue
			if mount -t udf,iso9660 -o ro "$i" /mnt/local && [ -f /mnt/local/make_iso.sh ]; then
				if [ -f /mnt/local/squashfs.img ]; then
					echo "squashfs.img found in ${i}. Copying to /root/"
					cp -rf /mnt/local/squashfs.img /root/
				fi
				umount /mnt/local
				return 0
			else
				umount /mnt/local 1>&2 2>/dev/null
			fi
		done
		sleep 2
	done
	echo "Could not find a CDROM containing squashfs.img. Searching USB devices now"
	for try in $(seq 1 15); do
		echo "[$try/15] Searching for a USB device containing squashfs.img"
		for part in /dev/sd*[1-2] /dev/nvme*p[1-2]; do
			[ -e "$part" ] || continue
			if mount "$part" /mnt/local 1>&2 2>/dev/null && [ -f /mnt/local/.prepared ]; then
				echo "squashfs.img found in ${part}"
				if [ -f /mnt/local/squashfs.img ]; then
					cp -rf /mnt/local/squashfs.img /root/
				fi
				umount /mnt/local
				return 0
			else
				umount /mnt/local 1>&2 2>/dev/null
			fi
		done
		sleep 2
	done
	echo "Could not find a CDROM or a USB device containing squashfs.img"
	drop_to_shell_auto
}

copy_contents() {
	for retry in $(seq 1 15); do
		for i in /dev/sr*; do
			[ -e "$i" ] || continue
			echo "Mounting $i"
			if mount -t udf,iso9660 -o ro "$i" /mnt/iso && [ -f /mnt/iso/make_iso.sh ]; then
				if [ -d /mnt/iso/"$1" ]; then
					echo "Copying $1 from $i"
					cp -r /mnt/iso/"$1" /root/"$1"
					umount /mnt/iso
					return 0
				elif [ -f /mnt/iso/"$1" ]; then
					echo "Copying $1 from $i"
					cp /mnt/iso/"$1" /root/
					umount /mnt/iso
					return 0
				else
					echo "$i doesn't contain directory /$1. Proceeding without searching for injections"
					return 1
				fi
			fi
			umount /mnt/iso 2>/dev/null || true
		done
		echo "[$retry/15] Searching for a CDROM containing directory /$1"
		sleep 2
	done
	echo "No CDROM containing directory /$1 was found. Proceeding without searching for injections"
	return 1
}

find_squashfs_in_iso_ce() {
	echo "Looking for device containing Phoenix ISO..."
	for retry in $(seq 1 15); do
		PHX_DEV=$(blkid | grep 'LABEL="PHOENIX"' | cut -d: -f1)
		ret=$?
		if [ $ret -eq 0 ] && [ -n "$PHX_DEV" ]; then
			if mount "$PHX_DEV" /mnt/iso; then
				if [ -f /mnt/iso/squashfs.img ]; then
					printf '\nCopying squashfs.img from Phoenix ISO on %s\n' "$PHX_DEV"
					cp -rf /mnt/iso/squashfs.img /root/
					umount /mnt/iso
					return 0
				else
					umount /mnt/iso
				fi
			fi
		fi
		printf '\r [%d/15] Waiting for Phoenix ISO to be available ...\n' "$retry"
		sleep 2
	done
	echo "Failed to find Phoenix ISO."
	return 1
}

setup_overlayfs() {
	echo "Mounting squashfs"

	if [ -f /root/squashfs.img ]; then
		IMG_FILE=/root/squashfs.img
	fi

	echo "INFO: Setting up overlayfs using squashfs from ${IMG_FILE}"

	if mount -t squashfs "$IMG_FILE" /mnt/squashfs; then

		if [ ! -f /mnt/squashfs/usr/bin/bash ] || [ ! -s /mnt/squashfs/usr/bin/bash ] || [ "$(busybox find /mnt/squashfs -type f | busybox wc -l)" -lt 1000 ]; then
			echo "ERROR: Squashfs empty/corrupt—check MD5."
			busybox ls -la /mnt/squashfs/usr/bin/bash && echo "Found bash in the squashfs"
			umount /mnt/squashfs
			drop_to_shell_auto
			return 1
		fi

		echo "Squashfs OK: $(busybox find /mnt/squashfs -type f | busybox wc -l) files (full rootfs)."

		mkdir -p /overlay
		mount -t tmpfs -o size="$RAMDISK_SZ" tmpfs /overlay
		cp -af /mnt/squashfs/. /overlay/
		cp /bin/busybox /overlay/bin/
		umount /mnt/squashfs

		if [ "$(find /mnt/squashfs -type f | wc -l)" -gt 0 ]; then
			echo "preparing new rootfs"
			cp -rf /lib/* /overlay/lib/ 2>/dev/null || true
			cp -rf /root/.local /overlay/root 2>/dev/null || true
			if [ -d /root/updates ]; then
				cp -rf /root/updates /overlay/root
			fi
			for file in /*; do
				if [ -d "$file" ]; then
					if [ "${file##*/}" = "phoenix" ]; then
						cp -rf "$file" /overlay/root/
					fi
				else
					cp -rf "$file" /overlay/root/ 2>/dev/null || true
				fi
			done
			cp -P /active_nic /overlay/ 2>/dev/null || true
			cp /tmp/* /overlay/tmp/ 2>/dev/null || true
			if grep -qs "/mnt/local" /proc/mounts; then umount /mnt/local 2>/dev/null; fi
			echo "switching root to new file system."
			exec switch_root -c /dev/console /overlay /sbin/init
			echo "switch root failed."
			return 1
		fi
		return 0

	else

		echo "Failed to mount squashfs"
		return 1

	fi

}

configure_networking_from_cvm() {
	if ! assemble_raid; then
		[ -n "$CVM_HOME_RAID_PART_UUID" ] && drop_to_shell_auto
	fi
	[ -d "$CVM_HOME_MNT" ] || mkdir -p "$CVM_HOME_MNT"
	parts="/dev/md* /dev/sd* /dev/nvme*"
	if find_cvm_home_raid_part_by_uuid; then
		cvm_part=$(cat "$CVM_HOME_PART_INFO_PATH")
		parts="$cvm_part $parts"
	fi
	for part in $parts; do
		[ -e "$part" ] || continue
		echo "Looking for Phoenix networking configuration in $part"
		if mount "$part" "$CVM_HOME_MNT"; then
			echo "$part mounted successfully"
			cvm_phx="$CVM_HOME_MNT/nutanix/tmp/phoenix/svm_cfg.json"
			if [ -f "$cvm_phx" ]; then
				echo "cvm network configuration found on $part"
				DHCP=$(grep -i '^ *"BOOTPROTO":' -m 1 "$cvm_phx" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
				if [ "$DHCP" = "none" ]; then
					PHOENIX_IP=$(grep -i '^ *"IPADDR":' -m 1 "$cvm_phx" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					MASK=$(grep -i '^ *"NETMASK":' -m 1 "$cvm_phx" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					GATEWAY=$(grep -i '^ *"GATEWAY":' -m 1 "$cvm_phx" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					FOUND_IP=$(grep -i '^ *"FOUND_IP":' -m 1 "$cvm_phx" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					VID=$(grep -i '^ *"VLAN":' -m 1 "$cvm_phx" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					[ "$VID" != "none" ] && VLAN="$VID"
					echo "Extracted network configuration from CVM:"
					echo "IP [$PHOENIX_IP]"
					echo "NETMASK [$MASK]"
					echo "GATEWAY [$GATEWAY]"
					echo "VLAN [$VLAN]"
					echo "FOUND_IP [$FOUND_IP]"
					configure_uplink_to_foundation
					echo "$part" >"$CVM_HOME_PART_INFO_PATH"
					umount "$CVM_HOME_MNT"
					return 0
				fi
			fi
			for i in 1 2; do
				umount "$CVM_HOME_MNT" 2>/dev/null || true
				sleep 5
			done
			return 1
		fi
	done
	return 1
}

setup_nw() {
	if [ "$IPV6" = "true" ] || [ -n "$PHOENIX_IP" ]; then
		if [ "$IPV6" != "true" ] && [ -n "$NAMESERVER" ]; then
			echo "Setting $NAMESERVER as DNS server"
			rm -f /etc/resolv.conf
			for x in $(echo "$NAMESERVER" | sed "s/,/ /g"); do
				echo nameserver "$x" >>/etc/resolv.conf
			done
		fi
		if [ "$IPV6" != "true" ] && [ -n "$NTP_SERVERS" ] && [ "$OS_TYPE" = "Centos" ]; then
			echo "Setting $NTP_SERVERS as NTP server(s)"
			for x in $(echo "$NTP_SERVERS" | sed "s/,/ /g"); do
				echo server "$x" >>/etc/chrony.conf
			done
			systemctl restart chronyd
		fi
		configure_uplink_to_foundation
		setup_uplink_to_foundation && return 0
		echo "Could not establish a connection to Foundation"
		return 1
	elif [ "$IS_INTERSIGHT" = "true" ]; then
		setup_nw_for_intersight_node
		if ! setup_uplink_to_foundation; then drop_to_shell_auto; fi
	else
		if [ "$USE_CVM_CFG" = "true" ]; then
			configure_networking_from_cvm
			setup_uplink_to_foundation && return 0
		fi
		if [ "$OS_TYPE" = "Gentoo" ] && [ "$IS_CISCO" = "true" ]; then
			return 0
		fi
		echo "Getting DHCP address for phoenix"
		for x in /sys/class/net/*; do
			x="${x##*/}"
			[ "$x" = "lo" ] && continue
			ifconfig "$x" up
			if [ "$OS_TYPE" = "Gentoo" ]; then
				echo "Getting DHCP address for $x..."
				udhcpc -b -q -i "$x" -s /dhcp.sh
			fi
		done
		if [ "$OS_TYPE" = "Gentoo" ]; then
			[ ! -f /.dhcp_lease ] && echo "Waiting for DHCP lease" && sleep 2
		else
			if [ -n "$FC_CONFIG_URL" ]; then
				bash -c ". /root/dhcp_network.sh; setup_dhcp_network"
			else
				dhclient -v
			fi
		fi
	fi
	sleep 2
	return 0
}

##################################################
# Main
##################################################

if [ -e '/etc/redhat-release' ]; then
	OS_TYPE="Centos"
	HOME="/root"
else
	OS_TYPE="Gentoo"
	HOME="/"
fi

dmesg >/tmp/dmesg_out

mkdir -p /mnt/local /mnt/squashfs /mnt/disk /mnt/data /mnt/usb /mnt/tmp \
	/mnt/bootbank /mnt/altbootbank /mnt/stage /mnt/scratch \
	/mnt/svm_installer /mnt/iso /mnt/efi

echo "Loading drivers"

# shellcheck disable=SC1091
. "$HOME/modules.sh"

# shellcheck disable=SC1091
. "$HOME/net_utils.sh"

# shellcheck disable=SC1091
. "$HOME/raid_utils.sh"

# check for livecd only in case of Gentoo
if [ "$OS_TYPE" = "Gentoo" ]; then
	dmesg | grep -i "dmi: cisco" >/dev/null
	dmesg | grep -i "dmi: cisco" && IS_CISCO=true
	wait_for_devices
	setup_nw
	setup_nw_result=$?
	# HACK: iPXE/CE cascade if netboot params
	if [ -n "$LIVEFS_URL" ] || [ -n "$PHOENIX_BASE" ]; then

		NCE_HACKS_SUCCESSFUL="false"
		echo "Waiting for Network to be ready (Gentoo)..."
		sleep 15
		load_loop_module || echo "Loop module load failed—skipping losetup"

		# Opt1: Probe sanboot ISO
		mount_iso_for_ce && NCE_HACKS_SUCCESSFUL="true"

		# Opt2: Full ISO wget+mount
		[ "$NCE_HACKS_SUCCESSFUL" != "true" ] && download_and_mount_iso_for_ce && NCE_HACKS_SUCCESSFUL="true"

		# Opt3: Squashfs solo
		[ "$NCE_HACKS_SUCCESSFUL" != "true" ] && download_squashfs_into_ce && NCE_HACKS_SUCCESSFUL="true"

		if [ "$NCE_HACKS_SUCCESSFUL" != "true" ]; then
			echo "All netboot fallbacks failed—dropping to shell."
			drop_to_shell_auto
		fi

		echo "Netboot hack completed successfully"

	fi
else
	# Non-Gentoo fallbacks
	if [ -n "$LIVEFS_URL" ]; then
		if [ "$setup_nw_result" -ne 0 ]; then
			find_squashfs_in_disks
		else
			echo "Downloading squashfs.img"
			total_tries=5
			for i in $(seq $total_tries); do
				wget "$LIVEFS_URL" -t1 -T30 -O- >"$IMG_FILE"
				if md5sum "$IMG_FILE" | grep -q "$IMG_MD5SUM"; then
					break
				else
					echo "md5 checksum does not match"
					rm "$IMG_FILE"
				fi
				[ -e "$IMG_FILE" ] || {
					echo "[$i/$total_tries] wget failed, sleeping for 5 seconds before trying again"
					sleep 5
				}
			done
			[ ! -e "$IMG_FILE" ] && echo "Failed to download squashfs.img via wget" && find_squashfs_in_disks
		fi
	elif [ "$PEM_WORKFLOW" = "TRUE" ]; then
		if [ "$COMPUTE_ONLY" = "TRUE" ]; then
			find_squashfs_in_disks "tmp_phoenix boot/tmp_phoenix root/tmp_phoenix"
		else
			find_squashfs_in_disks "nutanix/tmp_phoenix"
		fi
	elif [ $ce -eq 0 ]; then
		echo "Boot parameter LIVEFS_URL was not provided. We will not try to download squashfs.img from network"
		retry=1
		if [ "$DISCOVERY_OS" = "true" ] || [ "$DISCOVERY_OS" = "TRUE" ]; then
			find_squashfs_in_disks "disc_os"
			retry=$?
		fi
		[ $retry -eq 1 ] && find_squashfs_in_iso
	else
		if [ "$(basename "$INIT_CMD")" = "installer" ]; then
			echo "Since the boot parameter INIT_CMD is \"installer\", we need to search CDROMs and USB devices for AOS and hypervisor files"
			find_squashfs_in_iso
		fi
	fi
fi

if [ -z "$PXEBOOT" ] && [ $ce -eq 0 ]; then
	echo "Checking if any CDROM contains injections into Phoenix"
	copy_contents "updates"
	copy_contents "components"
fi

if [ -n "$FC_CONFIG_URL" ]; then
	copy_contents "images"
fi

if [ "$OS_TYPE" = "Centos" ]; then
	dmidecode -t 1 | grep -i cisco
	dmidecode -t 1 | grep -i cisco && IS_CISCO="true"
	if [ "$IS_CISCO" = "true" ] && [ -f "$CISCO_IPMITOOL" ]; then
		for _ in $(seq 3); do
			if $CISCO_IPMITOOL read_file "$INTERSIGHT_CONFIG_SRC" "$INTERSIGHT_CONFIG" && [ -f "$INTERSIGHT_CONFIG" ]; then
				IS_INTERSIGHT="true"
				copy_contents "images"
				break
			fi
		done
		[ "$IS_INTERSIGHT" = "false" ] && echo "Couldn't find the intersight config for the cisco node"
	fi
fi

cp /proc/mounts /etc/mtab 1>/dev/null 2>&1

if [ "$OS_TYPE" = "Gentoo" ]; then
	#########################
	# Gentoo
	#########################

	if [ ! -e "$IMG_FILE" ] && [ ! -e /root/squashfs.img ]; then
		echo "livecd files not found."
		drop_to_shell_auto
	fi

	if [ ! -f /.overlayfs_setup_done ]; then
		setup_overlayfs
		if ! setup_overlayfs; then echo "Unable to create overlayfs"; fi
		touch /.overlayfs_setup_done
		[ $ce -ne 0 ] && echo && echo
	fi

	cat >/bin/sudo <<-'EOF'
		#!/bin/sh
		exec "$@"
	EOF
	chmod +x /bin/sudo

	mount -o remount,size="$RAMDISK_SZ" /
	script="${0##*/}"

else
	#########################
	# Centos
	#########################

	setup_nw
	if ! setup_nw; then echo "Unable to setup networking"; fi

	if [ -d /overlay/mnt/iso ] && ! mountpoint -q /mnt/iso; then
		echo "Mounting ISO from overlay into /mnt/iso"
		mount --bind /overlay/mnt/iso /mnt/iso
	elif [ -n "$(extract_boot_param PHOENIX_BASE)" ] && [ ! -d /mnt/iso/images ]; then
		# shellcheck disable=SC1091
		. /root/ce_functions.sh
		download_images_into_ce || echo "Piecemeal images fallback failed—check /tmp/nce_hacks.log"
	fi

	echo "Running $INIT_CMD"
	script=$(basename "$INIT_CMD")

fi

if [ -e "$HOME/do_${script}.sh" ]; then
	# shellcheck disable=SC1090
	. "$HOME/do_${script}.sh"
else
	echo "ERROR: $HOME/do_${script}.sh not found."
	drop_to_shell_auto
fi
