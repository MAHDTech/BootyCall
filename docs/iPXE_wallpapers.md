# iPXE Wallpapers

## Overview

Taking iPXE wallpapers up a notch.

## Part 1: Generating Wallpapers

Give [Grok Imagine](https://grok.com) a prompt to generate some wallpapers.

> Example prompt
>
> Create an image suitable for a background that meets the following criteria
>
> - 1024x768 resolution
> - cyberpunk / synthwave inspired colours and theme
> - in the bottom right hand corner has an anime japanese girl with blonde wearing a maids dress answering an old school black telephone, she is "answering the call" and should be looking towards the top left of the image, looking into the sky
> - the image should be wide, and contain no text
> - text will be overlayed on top of this image so keep is clutter free and dark to allow light text to be places on top.

Download the generated images.

## Part 2: Optimizing Wallpapers

Convert and optimize the generated wallpapers making them suitable for iPXE.

- Launch a nix-shell with the right tools for the job.

```bash
nix-shell -p imagemagick pngquant optipng
```

- Convert the image from JPG into PNG format and optimize the file size.

```bash
IMAGES_SOURCE="${HOME}/Downloads"
IMAGES_DEST="$(pwd)/tftpboot/images/wallpapers"

# Reset the counter
COUNTER=0

# Use file descriptor 3 for the find output to avoid conflicting with stdin for user input
while read -u 3 -r IMAGE_SOURCE;
do
	# Each image will be named $COUNTER.png
	COUNTER=$((COUNTER + 1))

	IMAGE_NAME=$(basename "$IMAGE_SOURCE")
	IMAGE_DEST="${IMAGES_DEST}/${COUNTER}.png"

	# Warn if the destination already exists.
	if [ -f "${IMAGE_DEST}" ];
	then
		# Read from /dev/tty to get user input instead of from the pipe
		read -p "The destination wallpaper ${IMAGE_DEST} already exists, do you want to overwrite it? [y/N] " -r < /dev/tty RESPONSE

		if [[ "$RESPONSE" =~ ^([yY][eE][sS]|[yY])$ ]];
		then
			echo "Overwriting ${IMAGE_DEST}"
			rm -f "${IMAGE_DEST}" || {
				echo "Failed to remove ${IMAGE_DEST}, please remove manually!"
				break
			}
		else
			echo "Destination file ${IMAGE_DEST} already exists. Skipping..."
			continue
		fi
	fi

	echo -e "\nConverting ${IMAGE_NAME} to PNG format"
	sleep 1

	magick "${IMAGE_SOURCE}" \
		-strip \
		-alpha off \
		-interlace none \
		-define png:format=png24 \
		-resize 1024x768\! \
		"${IMAGE_DEST}" || {
			echo "Failed to convert ${IMAGE_NAME} to PNG format"
			break
		}

	echo -e "\nOptimizing ${IMAGE_DEST}"
	sleep 1

	optipng -o7 -strip all "${IMAGE_DEST}" || {
		echo "Failed to optimize ${IMAGE_DEST}"
		break
	}

	echo -e "\nQuantizing ${IMAGE_DEST}"
	sleep 1

	pngquant \
		--verbose \
		--skip-if-larger \
		--force \
		--quality=90-98 \
		--speed=1 \
		--strip \
		--output "${IMAGE_DEST}" "${IMAGE_DEST}" || {
		echo "Failed to quantize ${IMAGE_DEST} or was skipped"
	}

	echo "Finished processing ${IMAGE_SOURCE} to ${IMAGE_DEST}"

done 3< <(find "$IMAGES_SOURCE" -type f -name "grok_image*.jpg")
```

## Part 3: Making iPXE randomize wallpapers

Caddy templates ftw!
