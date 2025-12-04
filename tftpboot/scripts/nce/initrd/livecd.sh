#!/bin/sh

##################################################
#
# Copyright (c) 2012 Nutanix, Inc.  All Rights Reserved.
#
# Author: cui@nutanix.com
#         manish.sharma@nutanix.com (Added centos/squashfs support)
#
# Entry point to all image related scripts. Mounts livecd and dispatches to
# a different script that does the imaging.

# Log all output to file and console for debugging purpose.
##################################################

##################################################
# Hacked on by MAHDTech@saltlabs.cloud to add iPXE support.
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

# shellcheck disable=SC1091
. "$HOME/common_utils.sh"

# shellcheck disable=SC1091
. "${HOME}/ce_functions.sh" || {
	echo "Failed to source critical NCE functions!"
	exit 1
}

# ENG-197802
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
export TERM=linux

# More colours
export RED='\033[0;31m'
export GREEN='\033[0;32m'
export YELLOW='\033[0;33m'
export BLUE='\033[0;34m'
export PURPLE='\033[0;35m'
export RESET='\033[0m'

# Setting core dump file name pattern
# %e is the filename
# %s is the signal that caused the dump
# %t is the time the dump occurred
sysctl -w kernel.core_pattern=/tmp/core-%e-%s-%t
# Setting core dump file size to unlimited
# shellcheck disable=SC3045
ulimit -S -c unlimited

ce=0
if [ "${0##*/}" = "ce_installer" ]; then
	ce=1
fi

##################################################
# Functions
##################################################

redirect_logs_to_file $log_file
# TODO: rewrite a unified logging function.
#logger() {
#
#}

wait_for_devices() {
	if [ "$PEM_WORKFLOW" = "TRUE" ] || [ "$USE_CVM_CFG" = "true" ]; then
		# Devices can take longer to initialize post firmware upgrade,
		# add one minute delay since PEM/IVU workflows mount disks early.
		for try in $(seq 1 6); do
			echo "[$try/6] Waiting for devices to get initialized..."
			sleep 10
		done
	fi
}

find_squashfs_in_disks() {
	paths=${1:-"nutanix/foundation/tmp/phoenix_livecd"}
	echo "Looking for squashfs.img in the existing filesystem of this node"
	[ -d $CVM_HOME_MNT ] || mkdir -p $CVM_HOME_MNT
	parts="/dev/md* /dev/sd* /dev/nvme*"
	if ! [ "$COMPUTE_ONLY" = "TRUE" ]; then
		if ! assemble_raid; then
			# Fail early if CVM boot disk is raided.
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
				livecd=$CVM_HOME_MNT/$path/squashfs.img
				if [ -f "$livecd" ]; then
					echo "squashfs.img found in $part"
					if md5sum "$livecd" | grep -q "$IMG_MD5SUM"; then
						cp "$livecd" /mnt/local
					else
						echo "squashfs.img found in $part at $path but md5sum didn't match"
						continue
					fi

					updates_dir_path=$CVM_HOME_MNT/$path/updates
					if [ -d "$updates_dir_path" ]; then
						[ -d /root/updates ] || mkdir -p /root/updates
						cp -rf "$updates_dir_path"/* /root/updates
					fi

					if [ -d "$CVM_HOME_MARKER1" ] || [ -d "$CVM_HOME_MARKER2" ]; then
						echo "Storing CVM home partition info in $CVM_HOME_PART_INFO_PATH"
						echo "$part" >"$CVM_HOME_PART_INFO_PATH"
					fi
					umount $CVM_HOME_MNT
					return 0
				fi
			done
		fi
		umount $CVM_HOME_MNT
	done

	printf "Phoenix %sfailed%s to load squashfs.img\nfrom both the network and the existing CVM filesystem of this node\n" "$RED" "$RESET"
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
	# ENG-51262 Try CDROMs first before moving on to usb devices.
	for try in $(seq 1 15); do
		echo "[$try/15] Searching for a CDROM containing squashfs.img"
		for i in /dev/sr*; do
			[ -e "$i" ] || continue
			# Copy squashfs in RAM. Otherwise, once iso is removed, livecd environment
			# will crash. Same for USB devices below.
			if mount -t udf,iso9660 -o ro "$i" /mnt/local && [ -f /mnt/local/make_iso.sh ]; then
				if [ -f /mnt/local/squashfs.img ]; then
					# Node is booted in Centos phoenix, not Gentoo phoenix
					echo "squashfs.img found in ${i}. Copying to /root/"
					cp -rf /mnt/local/squashfs.img /root/
				fi
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
			mount "$part" /mnt/local 1>&2 2>/dev/null
			if [ -f /mnt/local/.prepared ]; then
				echo "squashfs.img found in ${part}"
				if [ -f /mnt/local/squashfs.img ]; then
					# Node is booted in Centos phoenix, not Gentoo phoenix
					cp -rf /mnt/local/squashfs.img /root/
				fi
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
	# Searches for a file/folder in the CDROM and if found, copies it
	# to the root directory.
	for retry in $(seq 1 15); do
		for i in /dev/sr*; do
			[ -e "$i" ] || continue
			echo "Mounting $i"
			if mount -t udf,iso9660 -o ro "$i" /mnt/iso && [ -f /mnt/iso/make_iso.sh ]; then
				if [ -d "/mnt/iso/$1" ]; then
					echo "Copying $1 from $i"
					cp -r "/mnt/iso/$1" /root/"$1"
					umount /mnt/iso
					return 0
				elif [ -f "/mnt/iso/$1" ]; then
					echo "Copying $1 from $i"
					cp "/mnt/iso/$1" /root/
					umount /mnt/iso
					return 0
				else
					# Iso is already mounted, either dir will be there or not. No
					# need to wait for additional 1 min.
					echo "$i doesn't contain directory /$1." \
						"Proceeding without searching for injections"
					return 1
				fi
			fi
			umount /mnt/iso
		done
		echo "[$retry/15] Searching for a CDROM containing directory /$1"
		sleep 2
	done
	echo "No CDROM containing directory /$1 was found." \
		"Proceeding without searching for injections"
	return 1
}

find_squashfs_in_iso_ce() {
	echo "Looking for device containing Phoenix ISO..."
	for retry in $(seq 1 15); do
		PHX_DEV=$(blkid | grep 'LABEL="PHOENIX"' | cut -d: -f1)
		ret=$?
		if [ "$ret" -eq 0 ] && [ "$PHX_DEV" != "" ]; then
			if mount "$PHX_DEV" /mnt/iso; then
				if [ -f /mnt/iso/squashfs.img ]; then
					printf "\nCopying squashfs.img from Phoenix ISO on %s\n" "$PHX_DEV"
					cp -rf /mnt/iso/squashfs.img /root/
					return 0
				else
					umount /mnt/iso
				fi
			fi
		fi
		printf "\r [%s/15] Waiting for Phoenix ISO to be available ..." "$retry"
		sleep 2
	done

	echo "Failed to find Phoenix ISO."
	return 1
}

# max raid rebuild speed set to 5G/sec
echo 5000000 >/proc/sys/dev/raid/speed_limit_max
# min raid rebuild speed set to 500M/sec
echo 500000 >/proc/sys/dev/raid/speed_limit_min

##################################################
# Constants
##################################################

# constants and boot_params
RAMDISK_SZ=${RAMDISK_SZ:-"64G"}
# Hash value will be populated by Makefile.phoenix.
SQUASHFS_DIGEST_x86_64=b38db887dd467372179620d1c70f9300
SQUASHFS_DIGEST_ppc64le=dfec0035fe5fce613e58d3e015d36fbb
LIVEFS_URL="$(get_boot_param LIVEFS_URL)"
PHOENIX_IP="$(get_boot_param PHOENIX_IP)"
MASK="$(get_boot_param MASK)"
FOUND_IP="$(get_boot_param FOUND_IP)"
GATEWAY="$(get_boot_param GATEWAY)"
VLAN="$(get_boot_param VLAN)"
NAMESERVER="$(get_boot_param NAMESERVER)"
# Accepts multiple NTP servers in comma separated format.
NTP_SERVERS="$(get_boot_param NTP_SERVERS)"
# Parameters for setting up bonding.
# shellcheck disable=SC2034
BOND_MODE="$(get_boot_param BOND_MODE)"
# shellcheck disable=SC2034
BOND_UPLINKS="$(get_boot_param BOND_UPLINKS)"
# shellcheck disable=SC2034
BOND_LACP_RATE="$(get_boot_param BOND_LACP_RATE)"
# This parameter tells us about boot in centos or gentoo
INIT_CMD="$(get_boot_param init)"
# Parameter to enable network configuration using config in CVM partition.
USE_CVM_CFG="$(get_boot_param USE_CVM_CFG)"
# URL to download Foundation Central settings.
FC_CONFIG_URL="$(get_boot_param FC_CONFIG_URL)"
# Parameter for PEM workflow
COMPUTE_ONLY="$(get_boot_param COMPUTE_ONLY)"
PEM_WORKFLOW="$(get_boot_param PEM_WORKFLOW)"
CVM_HOME_PART_INFO_PATH="/phoenix/.cvm_home_partition_info"
CVM_HOME_MNT=/mnt/cvm_home
CVM_HOME_MARKER1=$CVM_HOME_MNT/nutanix/data/installer
CVM_HOME_MARKER2=$CVM_HOME_MNT/data/installer
CVM_HOME_RAID_PART_UUID="$(get_boot_param CVM_HOME_RAID_PART_UUID)"
# For DOS upgrade
DISCOVERY_OS="$(get_boot_param DISCOVERY_OS)"
# Because hypervisors generally call untagged traffic "0", treat it that way
# here too.
if [ "$VLAN" = 'None' ] || [ "$VLAN" = 0 ]; then
	VLAN=""
fi
PXEBOOT="$(get_boot_param PXEBOOT)"
ifconfig lo 127.0.0.1 netmask 255.0.0.0 up

IMG_FILE=/mnt/local/squashfs.img

if (uname -m | grep -q x86_64); then
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

# ENG-389037: stop abusing overlayfs or other
# unnecessary stuff, the overlayfs layer has been removed to avoid
# modprobing issue. I didn't bother to cleanup the naming of functions
# due to laziness. TODO: maybe get rid of squashfs and simplify the entire
# implementation.
setup_overlayfs() {
	echo "Mounting squashfs"
	if [ -f /root/squashfs.img ]; then
		# ENG-178833: When we have copied the squashfs img at "/root". It will
		# make sure Centos phoenix will not throw any I/O error in case iso
		# get detached.e.g "HUA-32".
		IMG_FILE=/root/squashfs.img
	fi
	if mount -t squashfs "$IMG_FILE" /mnt/squashfs; then
		# Setting up default size of the ramdisk
		mkdir -p /overlay
		mount -t tmpfs -o size="$RAMDISK_SZ" tmpfs /overlay
		cp -af /mnt/squashfs/. /overlay/
		cp /bin/busybox /overlay/bin/
		if umount /mnt/squashfs; then
			echo "preparing new rootfs"
			cp -rf /lib/* /overlay/lib/
			cp -rf /root/.local /overlay/root
			# ENG-182413, ENG-337395: Copy override files and foundation-platforms
			# contents from /updates to /overlay
			if [ -d /root/updates ]; then
				cp -rf /root/updates /overlay/root
			fi
			# TODO: dell package maybe missing, if needed we can copy for /dell.
			for file in /*; do
				if [ -d "$file" ]; then
					if [ "${file##*/}" = "phoenix" ]; then
						cp -rf "$file" /overlay/root/
					fi
				else
					cp -rf "$file" /overlay/root/
				fi
			done

			# Copy cached networking information over.
			cp -P /active_nic /overlay/ || true
			cp /tmp/* /overlay/tmp/ || true
			# Note: better to umount the /mnt/local before switch root
			# find_squashfs_in_iso may keep the /mnt/local mounted, and it breaks disco-OS
			# squashfs will be either be copied to /root if it's found from cdrom/usb
			# (manual phoenix iso or disco-OS), or stays at /mnt/local from ramfs (foundation)
			grep -qs "/mnt/local" /proc/mounts && umount /mnt/local

			echo "switching root to new file system."
			exec switch_root -c /dev/console /overlay /sbin/init
			echo "switch root failed."
			return 1
		else
			echo "Failed to unmount squashfs ${IMG_FILE}"
			drop_to_shell_auto
		fi
		return 0
	else
		echo "Failed to mount squashfs "
		return 1
	fi
}

configure_networking_from_cvm() {
	if ! assemble_raid; then
		# Fail early if CVM boot disk is raided.
		[ -n "$CVM_HOME_RAID_PART_UUID" ] && drop_to_shell_auto
	fi
	[ -d $CVM_HOME_MNT ] || mkdir -p $CVM_HOME_MNT
	parts="/dev/md* /dev/sd* /dev/nvme*"
	if find_cvm_home_raid_part_by_uuid; then
		cvm_part=$(cat $CVM_HOME_PART_INFO_PATH)
		parts="$cvm_part $parts"
	fi
	for part in $parts; do
		[ -e "$part" ] || continue
		echo "Looking for Phoenix networking configuration in $part"
		if mount "$part" $CVM_HOME_MNT; then
			echo "$part mounted successfully"
			cvm_phx=$CVM_HOME_MNT/nutanix/tmp/phoenix/svm_cfg.json
			# Sample configuration file:
			# {
			#   "NETMASK": "255.255.252.0",
			#   "BOOTPROTO": "none",
			#   "IPADDR": "10.47.96.115",
			#   "GATEWAY": "10.47.96.1",
			#   "VLAN": "300"
			# }
			if [ -f $cvm_phx ]; then
				echo "cvm network configuration found on $part"
				DHCP=$(grep -i '^ *"BOOTPROTO":' -m 1 "${cvm_phx}" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
				if [ "$DHCP" = "none" ]; then
					PHOENIX_IP=$(grep -i '^ *"IPADDR":' -m 1 "${cvm_phx}" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					MASK=$(grep -i '^ *"NETMASK":' -m 1 "${cvm_phx}" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					GATEWAY=$(grep -i '^ *"GATEWAY":' -m 1 "${cvm_phx}" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					FOUND_IP=$(grep -i '^ *"FOUND_IP":' -m 1 "${cvm_phx}" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					VID=$(grep -i '^ *"VLAN":' -m 1 "${cvm_phx}" 2>/dev/null | cut -d':' -f2- | tr -d '[:space:]",')
					if [ "$VID" != "none" ]; then
						VLAN=$VID
					fi
					#rm -f $cvm_phx
					echo "Extracted network configuration from CVM:"
					echo "IP [$PHOENIX_IP]"
					echo "NETMASK [$MASK]"
					echo "GATEWAY [$GATEWAY]"
					echo "VLAN [$VLAN]"
					echo "FOUND_IP [$FOUND_IP]"
					configure_uplink_to_foundation
					echo "Storing CVM home partition info in $CVM_HOME_PART_INFO_PATH"
					echo "$part" >"$CVM_HOME_PART_INFO_PATH"
					umount $CVM_HOME_MNT
					return 0
				fi
				for i in 1 2; do
					umount $CVM_HOME_MNT
					sleep 5
				done
				return 1
			fi
		fi
		for i in 1 2; do
			umount $CVM_HOME_MNT
			sleep 5
		done
	done
	return 1
}

setup_nw() {
	if [ "$IPV6" = "true" ] || [ -n "$PHOENIX_IP" ]; then
		if [ "$IPV6" != "true" ] && [ -n "$NAMESERVER" ]; then
			echo "Setting $NAMESERVER as DNS server"
			rm -f /etc/resolv.conf
			for x in $(echo "$NAMESERVER" | sed "s/,/ /g"); do
				echo "nameserver $x" >>/etc/resolv.conf
			done
		fi
		if [ "$IPV6" != "true" ] && [ -n "$NTP_SERVERS" ] && [ "$OS_TYPE" = "Centos" ]; then
			echo "Setting $NTP_SERVERS as NTP server(s)"
			for x in $(echo "$NTP_SERVERS" | sed "s/,/ /g"); do
				echo "server $x" >>/etc/chrony.conf
			done
			systemctl restart chronyd
		fi
		configure_uplink_to_foundation
		if setup_network_static "$NETWORK" "$PREFIX_LEN" "$GW" "$IFACE" "$VLAN"; then
			return 0
		fi
		echo "Could not establish a connection to Foundation"
		return 1
	elif [ "$IS_INTERSIGHT" = "true" ]; then
		setup_nw_for_intersight_node
		if ! configure_ipv6_only "$IFACE" "$VLAN" "$DHCP"; then
			drop_to_shell_auto
		fi
	else
		if [ "$USE_CVM_CFG" = "true" ]; then
			configure_networking_from_cvm
			if true; then
				echo "Found valid network configuration on cvm"
				return 0
			fi
		fi
		if [ "$OS_TYPE" = "Gentoo" ] && [ "$IS_CISCO" = "true" ]; then
			# Skip DHCP setup for cisco nodes when in gentoo and setup once squashfs
			# is loaded. This is needed for intersight nodes to avoid
			# unconfiguring the DHCP ip once in centos and configure again using the
			# config fetched from CIMC when imaging via FC.
			# If the intersight config is not present on CIMC, DHCP configuration if
			# required is done once squashfs is loaded.
			# All the current workflows where phoenix is used, doesn't rely on DHCP
			# to fetch squashfs img.
			return 0
		fi
		# Use DHCP if a static IP is not provided. Iterate through each
		# interface until you find a working one.
		echo "Getting DHCP address for phoenix"
		for x in /sys/class/net/*; do
			x=${x##*/}
			[ "$x" != "lo" ] || continue
			ifconfig "$x" up
			if [ "$OS_TYPE" = "Gentoo" ]; then
				echo "Getting DHCP address for $x..."
				udhcpc -b -q -i "$x" -s /dhcp.sh
			fi
		done
		if [ "$OS_TYPE" = "Gentoo" ]; then
			if [ ! -f /.dhcp_lease ]; then
				echo "Waiting for DHCP lease"
				sleep 2
			fi
		else
			if [ -n "$FC_CONFIG_URL" ]; then
				# Assign DHCP ip to only one interface for FC workflows.
				bash -c ". /root/dhcp_network.sh; setup_dhcp_network"
			else
				dhclient -v
			fi
		fi
	fi
	# Give the OS time to setup DNS
	sleep 2
	return 0
}

download_squashfs_from_livefs_url() {
	# Allow destination to be specified
	DESTINATION=$1

	echo "Downloading squashfs.img from $LIVEFS_URL to $DESTINATION"
	# In the case of VLAN environments, ping works but wget
	# takes a while to "stabilize". Try for a few times.
	total_tries=5
	for i in $(seq $total_tries); do
		wget "$LIVEFS_URL" -t1 -T30 -O- >"$DESTINATION"
		# verify md5sum of squashfs, delete the IMG_FILE if md5sum does not
		# match, so that we can retry or check for backup on cvm.
		if ! md5sum "$DESTINATION" | grep -q "$IMG_MD5SUM"; then
			echo "md5 checksum does not match"
			rm "$DESTINATION"
		fi
		if [ -e "$DESTINATION" ]; then
			break
		else
			echo "[$i/$total_tries] wget failed, sleeping for 5 seconds before trying again"
			sleep 5
		fi
	done
	# if wget fails, try to mount CVM and look for squashfs.img in it.
	if [ ! -e "$DESTINATION" ]; then
		echo "Failed to download squashfs.img via wget"
		find_squashfs_in_disks
	fi
}

wait_for_dns() {
	TIMEOUT=60
	ELAPSED=0
	DOMAIN=$(echo "$LIVEFS_URL" | sed -e 's|https\?://||' -e 's|/.*||')

	echo "Waiting for DNS resolution to be ready..."
	while ! nslookup "${DOMAIN}" >/dev/null 2>&1; do
		if [ $ELAPSED -ge $TIMEOUT ]; then
			echo "DNS timeout after ${TIMEOUT}s"
			return 1
		fi
		sleep 1
		ELAPSED=$((ELAPSED + 1))
	done
	echo "DNS ready"
}

##################################################
# Main
##################################################

# Identifying the OS type
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
	if dmesg | grep -q -i "dmi: cisco"; then
		IS_CISCO=true
	fi
	wait_for_devices
	setup_nw
	setup_nw_result=$?
	if [ $ce -ne 0 ]; then
		if [ -n "$(get_boot_param CE_IPXE)" ]; then
			echo "CE iPXE mode enabled"
			echo "Waiting for network..."
			sleep 15
			wait_for_dns || {
				echo "Warning, DNS resolution has failed, attempting to continue with download"
			}
			download_squashfs_from_livefs_url /root/squashfs.img
			[ -f /root/squashfs.img ] || drop_to_shell_auto
		else
			echo "CE USB mode enabled"
			find_squashfs_in_iso_ce
		fi
	elif [ -n "$LIVEFS_URL" ]; then
		if [ $setup_nw_result -ne 0 ]; then
			find_squashfs_in_disks
		else
			download_squashfs_from_livefs_url $IMG_FILE
			[ -f "$IMG_FILE" ] || drop_to_shell_auto
		fi
	elif [ "$PEM_WORKFLOW" = "TRUE" ]; then
		if [ "$COMPUTE_ONLY" = "TRUE" ]; then
			# For FND >= 5.1, PEM will be staging squashfs.img and other
			# files in /boot partition. This is required for AHV on LVM
			# where we will only have access to boot partition.
			find_squashfs_in_disks "tmp_phoenix boot/tmp_phoenix root/tmp_phoenix"
		else
			find_squashfs_in_disks nutanix/tmp_phoenix
		fi
	elif [ $ce -eq 0 ]; then
		echo "Boot parameter LIVEFS_URL was not provided." \
			"We will not try to download squashfs.img from network"
		retry=1
		if [ "$DISCOVERY_OS" = "true" ] || [ "$DISCOVERY_OS" = "TRUE" ]; then
			find_squashfs_in_disks disc_os
			retry=$?
		fi
		if [ $retry -eq 1 ]; then
			find_squashfs_in_iso # Note: This drops to shell if squashfs.img isn't found
		fi
	fi
else
	# in case of installer mode, we need to mount iso to get nos and hypervisor
	if [ "$(basename "$INIT_CMD")" = "installer" ]; then
		echo 'Since the boot parameter INIT_CMD is "installer",' \
			"we need to search CDROMs and USB devices for AOS and hypervisor files"
		find_squashfs_in_iso # Note: This drops to shell if squashfs.img isn't found
	fi
fi

if [ -z "$PXEBOOT" ] && [ "$ce" -eq 0 ]; then
	echo "Checking if any CDROM contains injections into Phoenix"
	# Copy contents used by phoenix from cdrom iso image.
	# This makes it easier to debug as the following actions
	# don't rely on the cdrom being available.
	# The updates are for injected content like layout file and HCL
	# updates. Later they could be used for installation hooks.
	if copy_contents "updates"; then
		# Components include tartarus, aurora, updater, etc
		# which needs to be installed in phoenix for flex
		copy_contents "components"
	fi
fi

if [ -n "$FC_CONFIG_URL" ]; then
	# FC workflows require drivers to be present. Copy it out.
	copy_contents "images"
fi

# TODO: copy_contents need not execute both in gentoo and centos,
# fix in other places where ever it's done.
if [ "$OS_TYPE" = "Centos" ]; then
	# For cisco nodes, try to fetch the intersight config from cimc,
	# if it succeeds, fetch the drivers from phoenix for imaging via FC.
	if dmidecode -t 1 | grep -q -i cisco; then
		IS_CISCO="true"
	fi
	# TODO: Make use of phoenix/intersight_options.py
	if [ "$IS_CISCO" = "true" ] && [ -f "$CISCO_IPMITOOL" ]; then
		for _ in $(seq 3); do
			if $CISCO_IPMITOOL read_file "$INTERSIGHT_CONFIG_SRC" "$INTERSIGHT_CONFIG" && [ -f "$INTERSIGHT_CONFIG" ]; then
				IS_INTERSIGHT="true"
				copy_contents "images"
				break
			fi
		done
		if [ "$IS_INTERSIGHT" = "false" ]; then
			echo "Couldn't find the intersight config for the cisco node"
		fi
	fi
fi

cp /proc/mounts /etc/mtab 1>/dev/null 2>&1
# Bifurcation for both OS_TYPES
# Gentoo is more of initrd for centos
if [ "$OS_TYPE" = "Gentoo" ]; then
	if [ ! -e "$IMG_FILE" ] && [ ! -e /root/squashfs.img ]; then
		echo "livecd files not found."
		drop_to_shell_auto
	fi

	if [ ! -f /.overlayfs_setup_done ]; then
		setup_overlayfs
		if ! setup_overlayfs; then
			echo "Unable to create overlayfs"
		fi
		touch /.overlayfs_setup_done
		if [ $ce -ne 0 ]; then
			echo
			echo
		fi
	fi

	# Fake sudo command for compatibility, in svm_rescue some command start with
	# sudo, to run them properly we need this.
	cat >/bin/sudo <<'EOF'
#!/bin/sh
exec "$@"
EOF
	chmod +x /bin/sudo

	# Resize rootfs to 64GB.
	# Imaging will unpack NOS and hypervisor iso to ramfs, 16G is not quite enough
	# for a 5.9G Hyperv and 2.9G NOS. Increase this number to 64G to provide
	# larger installer images.
	mount -o remount,size="$RAMDISK_SZ" /

	script=${0##*/}

else
	# centos
	setup_nw
	if ! setup_nw; then
		echo "Unable to setup networking"
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
