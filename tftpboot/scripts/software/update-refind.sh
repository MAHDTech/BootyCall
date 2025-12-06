#!/usr/bin/env bash
clear

export DEBUG=false

# Variables
export WORK_DIR
WORK_DIR="$(mktemp --directory -p /tmp refind.XXX)"

export IMG_LOCAL
IMG_LOCAL="$(pwd)/tftpboot/iso/refind.img"
export IMG_REMOTE="bootycall:/mnt/hdd/tftpboot/iso/refind.img"
export IMG_SIZE="64"

export REFIND_VER="0.14.1"
export REFIND_BIN="refind_extracted/refind-bin-${REFIND_VER}"
export REFIND_THEME_ENABLED="true"
export REFIND_THEME_REPO="https://github.com/ussooraj/rEFInd-Synthwave.git"
export REFIND_THEME_DIR="refind-custom-theme"

export EFI_DIR="EFI"

mkdir -p "$WORK_DIR" || {
	echo "Failed to create a temporary working directory"
	exit 1
}

cat <<-_EOF_ >"${WORK_DIR}/run.sh"
	#!/usr/bin/env bash

	set -euo pipefail

	function cleanup {
		echo "Cleaning up..."
		popd 2>/dev/null

		if [ "${DEBUG:-false}" == "true" ]; then
			read -rp "Press Enter to begin cleanup operation on rEFInd working directory: ${WORK_DIR}..." RESPONSE
		fi

		rm -rf "${WORK_DIR}" || {
			echo "Failed to remove working directory, please cleanup ${WORK_DIR} manually."
		}
	}

	function show_header() {
		echo -e "\n########################"
		echo -e "\${1:-no message provided}"
		echo -e "########################\n"
	}

	trap cleanup EXIT

	show_header "Downloading rEFInd v${REFIND_VER}"

	pushd "${WORK_DIR}" || {
		echo "Failed to change directory"
		exit 1
	}

	curl \
		--insecure \
		--location \
		--output refind.zip \
		--url "https://sourceforge.net/projects/refind/files/${REFIND_VER}/refind-bin-${REFIND_VER}.zip/download" \
		|| {
			echo "Failed to download rEFInd"
			exit 1
		}

	unzip \
		-q \
		-d \
		refind_extracted \
		refind.zip || {
			echo "Failed to extract rEFInd"
			exit 1
		}

	show_header "Creating directory structure for EFI"

	mkdir -p "${EFI_DIR}/BOOT" || {
		echo "Failed to create directories for EFI"
		exit 1
	}

	cp "${REFIND_BIN}/refind/refind_x64.efi" "${EFI_DIR}/BOOT/BOOTX64.EFI" || {
		echo "Failed to copy rEFInd binary"
		exit 1
	}

	show_header "Creating directory structure for rEFInd common folders"

	REFIND_FOLDERS=(
		"assets"
		"fonts"
		"icons"
		"images"
		"keys"
	)

	for FOLDER in ${REFIND_FOLDERS[@]}; do
		echo "Processing rEFInd folder: ${FOLDER}"
		mkdir -p "${EFI_DIR}/refind/${FOLDER}" || {
			echo "Failed to create directory ${FOLDER} for rEFInd"
			exit 1
		}
		cp -r "${REFIND_BIN}/refind/${FOLDER}/"* "${EFI_DIR}/refind/${FOLDER}/" || {
			echo "Failed to copy rEFInd ${FOLDER}"
			exit 1
		}
	done

	show_header "Creating directory structure for rEFInd architecture folders"

	mkdir -p "${EFI_DIR}/refind/"{drivers,tools} || {
		echo "Failed to create directories for rEFInd"
		exit 1
	}

	cp "${REFIND_BIN}/refind/drivers_x64/"* "${EFI_DIR}/refind/drivers/" || {
		echo "Failed to copy rEFInd drivers"
		exit 1
	}

	cp "${REFIND_BIN}/refind/tools_x64/"* "${EFI_DIR}/refind/tools/" || {
		echo "Failed to copy rEFInd tools"
		exit 1
	}

	if [ "${REFIND_THEME_ENABLED}" = "true" ]; then
		show_header "Creating directory structure for rEFInd custom theme"

		mkdir -p "${EFI_DIR}/refind/theme/"{assets,icons} || {
			echo "Failed to create directory for rEFInd custom theme"
			exit 1
		}

		git clone "${REFIND_THEME_REPO}" "${REFIND_THEME_DIR}" || {
			echo "Failed to clone rEFInd custom theme into ${REFIND_THEME_DIR}"
			exit 1
		}

		if [ -d "${REFIND_THEME_DIR}/icons" ]; then
			echo "Copying custom rEFInd theme icons"
			cp -r "${REFIND_THEME_DIR}/icons"/* "${EFI_DIR}/refind/theme/icons/" || {
				echo "Failed to copy custom rEFInd theme icons"
				exit 1
			}
		else
			echo "Failed to find custom rEFInd theme icons directory"
			exit 1
		fi

		if [ -d "${REFIND_THEME_DIR}/assets" ]; then
			echo "Copying custom rEFInd theme assets"
			cp -r "${REFIND_THEME_DIR}/assets"/* "${EFI_DIR}/refind/theme/assets/" || {
				echo "Failed to copy custom rEFInd theme assets"
				exit 1
			}
		else
			echo "Failed to find custom rEFInd theme assets directory"
			exit 1
		fi

	else
		echo "Using default rEFInd theme"
	fi

	show_header "Creating rEFInd Config"

	# Common configuration
	cat <<-EOF > "${EFI_DIR}/refind/refind.conf"
	# Settings
	timeout                   10
	screensaver               30
	log_level                 3
	use_graphics_for          osx,linux,elilo,grub,windows
	showtools                 firmware,reboot,shutdown
	hideui                    singleuser,arrows,hints,badges,label
	default_selection         +
	scanfor                   internal,external,optical,biosexternal

	EOF
	# Theme configuration
	if "${REFIND_THEME_ENABLED:-false}" == "true"; then
		echo "Using custom rEFInd theme"
		cat <<-EOF >> "${EFI_DIR}/refind/refind.conf"
		# Resources
		icons_dir                 /EFI/refind/theme/icons
		banner                    /EFI/refind/theme/assets/background.png
		selection_big             /EFI/refind/theme/assets/selection_big_v3.png
		selection_small           /EFI/refind/theme/assets/selection_small_v3.png

		# Scale
		banner_scale              fillscreen
		big_icon_size             125
		small_icon_size           48

		EOF
	else
		echo "Using default rEFInd theme"
		cat <<-EOF >> "${EFI_DIR}/refind/refind.conf"
		# TODO: Add default theme configuration here.
		EOF
	fi

	show_header "Creating rEFInd image"

	mkdir -p "$(dirname "${IMG_LOCAL}")" || {
		echo "Failed to create image directory"
		exit 1
	}

	rm -f "${IMG_LOCAL}" || true

	dd if=/dev/zero of="${IMG_LOCAL}" bs=1M count="${IMG_SIZE}" status=none || {
		echo "Failed to create image file"
		exit 1
	}

	mkfs.vfat -n BOOTYCALL "${IMG_LOCAL}" || {
		echo "Failed to create FAT filesystem"
		exit 1
	}

	# Verify the image is accessible with mtools
	mdir -i "${IMG_LOCAL}" ::/ || {
		echo "Image is not accessible with mtools"
		exit 1
	}

	mcopy -s -i "${IMG_LOCAL}" "EFI" ::/ || {
		echo "Failed to copy rEFInd files"
		exit 1
	}

	echo "Image created -> ${IMG_LOCAL}"

_EOF_

chmod +x "${WORK_DIR}/run.sh" || {
	echo "Failed to make run.sh executable"
	exit 1
}

nix-shell -p \
	curl \
	dosfstools \
	git \
	mtools \
	openssh \
	refind \
	rsync \
	tree \
	unzip \
	--pure \
	--run "${WORK_DIR}/run.sh" || {
	echo "Failed to execute run.sh inside nix-shell!"
	exit 1
}

read -rp "Image creation has finished, do you want to upload it? " RESPONSE
if [[ $RESPONSE =~ ^[Yy]$ ]]; then
	rsync -av "${IMG_LOCAL}" "${IMG_REMOTE}" || {
		echo "Failed to upload image to remote server"
		exit 1
	}
fi

echo "Finished."
