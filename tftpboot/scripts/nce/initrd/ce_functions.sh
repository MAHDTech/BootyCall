#!/bin/sh

##################################################
# Name: ce_functions.sh
# Description: A collection of custom functions for hacking on NCE.
##################################################

# Common log for all the 'hacks'
NCE_HACKS_LOG=/tmp/nce_hacks.log

logger() {
	LOGGER_LEVEL="$1"
	LOGGER_MESSAGE="$2"
	LOGGER_TIME=$(date +"%Y-%m-%d %H:%M:%S")

	echo "${LOGGER_TIME} ${LOGGER_LEVEL} ${LOGGER_MESSAGE}" >>"${NCE_HACKS_LOG}" 2>&1
	echo "${LOGGER_TIME} ${LOGGER_LEVEL} ${LOGGER_MESSAGE}"
	return 0

}
logger "Loading Nutanix Community Edition shell functions..."

# Auto-detect stage: initrd (pre-overlay) vs full root (post-squashfs)
drop_to_shell_auto() {
	is_initrd=true

	if [ -f /etc/redhat-release ] || grep -q overlay /proc/mounts || [ -d /overlay ]; then
		is_initrd=false
	fi

	if [ "$is_initrd" = true ]; then
		echo "Detected INITRD stage - using safe drop_to_shell_initrd"
		drop_to_shell_initrd
	else
		echo "Detected FULL ROOT stage - using standard drop_to_shell"
		drop_to_shell
	fi
}

drop_to_shell_initrd() {
	echo "=== INITRD DEBUG SHELL ==="
	echo "Context: $(cat /proc/cmdline)"
	echo "Mounts: $(mount | cat)"
	echo "Available: busybox ls cat mount umount wget md5sum blkid losetup"
	echo "Resume: exec /init (or exit)"
	echo "=========================="

	# Temp tmpfs for symlinks/tools
	mkdir -p /debug
	mount -t tmpfs -o size=256M tmpfs /debug || echo "Tmpfs failed; using /tmp"
	cp /bin/busybox /debug/bin/ 2>/dev/null || true

	# Busybox applets as standalone
	for cmd in ls cat mount umount wget md5sum blkid losetup find wc; do
		ln -sf /bin/busybox /debug/bin/$cmd 2>/dev/null
	done

	export PATH="/debug/bin:$PATH"
	export PS1="INITRD: "

	# Exec sh in current namespace
	exec /bin/sh -c 'export PS1="INITRD: "; echo "Shell ready."; exec /bin/sh'
}

extract_boot_param() {
	PARAM_NAME="$1"
	VALUE=$(grep -oE "$PARAM_NAME=\S*" /proc/cmdline | head -n 1 | sed "s/^$PARAM_NAME=//")
	VALUE_TRIMMED=$(echo "$VALUE" | sed "s/^'//" | sed "s/'$//")
	echo "${VALUE_TRIMMED:-}"
}

download_path_into_ce() {
	SOURCE="$1" # e.g. http://192.168.1.100/iso/
	DEST="$2"   # local directory, must already exist

	[ -z "$SOURCE" ] && {
		logger ERROR "download_path_into_ce: SOURCE URL missing"
		return 1
	}
	[ -z "$DEST" ] && {
		logger ERROR "download_path_into_ce: DEST directory missing"
		return 1
	}
	[ -d "$DEST" ] || {
		logger ERROR "download_path_into_ce: DEST $DEST does not exist"
		return 1
	}

	TRY=1
	TOTAL_TRIES=10
	TOTAL_TRIES_PER_FILE=3
	TIME_BETWEEN=30
	TIMEOUT=3600
	TIMEOUT_READ_FILE=3600
	BANDWIDTH_LIMIT=100m

	logger INFO "Recursively downloading ISO contents from $SOURCE into $DEST"

	while [ "$TRY" -le "$TOTAL_TRIES" ]; do
		logger INFO "Recursive download attempt $TRY of $TOTAL_TRIES"

		if wget \
			--continue \
			--cut-dirs=2 \
			--directory-prefix="$DEST" \
			--level=inf \
			--limit-rate=$BANDWIDTH_LIMIT \
			--no-host-directories \
			--no-parent \
			--no-remove-listing \
			--quiet \
			--read-timeout=$TIMEOUT_READ_FILE \
			--recursive \
			--reject="index.html*,?.*" \
			--accept-regex='.*' \
			--show-progress \
			--timeout=${TIMEOUT} \
			--tries=${TOTAL_TRIES_PER_FILE} \
			--user-agent="Mozilla/5.0 (compatible; wget)" \
			--waitretry=${TIME_BETWEEN} \
			"${SOURCE}/" 2>&1 | tee /tmp/wget-recursive.log; then
			logger INFO "Recursive download completed successfully"
			return 0
		else
			logger WARN "wget recursive failed attempt $TRY/$TOTAL_TRIES"
			if [ "$TRY" -lt "$TOTAL_TRIES" ]; then
				logger WARN "Retrying in $TIME_BETWEEN seconds..."
				# Nuke all files between partial retries.
				#rm -rf "${DEST}"/*
				sleep "$TIME_BETWEEN"
			fi
		fi
		TRY=$((TRY + 1))
	done

	logger ERROR "Failed to recursively download $SOURCE after $TOTAL_TRIES attempts"
	cat /tmp/wget-recursive.log >>${NCE_HACKS_LOG} 2>/dev/null || true
	return 1
}

download_file_into_ce() {
	SOURCE="$1"
	DEST="$2"
	FILENAME=$(basename "$SOURCE")

	TRY=1
	TOTAL_TRIES=10
	TOTAL_TRIES_PER_FILE=3
	TIME_BETWEEN=30
	TIMEOUT=900

	logger INFO "Downloading ${SOURCE} to ${DEST}..."

	if ! command -v wget >/dev/null 2>&1; then
		logger ERROR "wget is not installed!"
		return 1
	fi

	while [ "$TRY" -le "$TOTAL_TRIES" ]; do
		logger INFO "Downloading ${FILENAME} attempt $TRY of $TOTAL_TRIES..."
		if wget \
			--continue \
			--quiet \
			--timeout="$TIMEOUT" \
			--tries=$TOTAL_TRIES_PER_FILE \
			--output-document=- "$SOURCE" >"$DEST"; then
			if [ -s "$DEST" ]; then
				logger INFO "Successfully downloaded ${SOURCE} to ${DEST}"
				return 0
			else
				logger WARN "Downloaded file is empty, will retry download."
				rm -f "$DEST"
			fi
		fi
		if [ "$TRY" -lt "$TOTAL_TRIES" ]; then
			logger WARN "Download failed, retrying in ${TIME_BETWEEN} seconds..."
			sleep "$TIME_BETWEEN"
		fi
		TRY=$((TRY + 1))
	done

	logger ERROR "Failed to download ${SOURCE} after ${TOTAL_TRIES} attempts"
	rm -f "$DEST"
	return 1
}
