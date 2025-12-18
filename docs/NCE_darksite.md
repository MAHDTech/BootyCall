# Nutanix Community Edition Darksite

## Overview

Instructions for setting up a Nutanix Community Edition Darksite on Linux.

## Prerequisites

Make sure you already have a running web server. In this example, we will use Caddy.

## Setup

- Download the latest LCM bundle from the [Nutanix Portal for LCM](https://portal.nutanix.com/page/downloads?product=lcm)

- Transfer the LCM bundle to the web server

```bash
LCM_BUNDLE="lcm_dark_site_bundle_3.3.74044.tar.gz"
REMOTE_DIR="bootycall:/mnt/hdd/tftpboot/nce-darksite"

rsync -avz --progress "${LCM_BUNDLE}" "${REMOTE_DIR}/"
```

- Extract the LCM bundle into the Darksite directory on the web server

```bash
LCM_BUNDLE="lcm_dark_site_bundle_3.3.74044.tar.gz"
LOCAL_DIR="/mnt/hdd/tftpboot/nce-darksite"

mkdir -p "${LOCAL_DIR}/lcm"

tar -xvzf "${LOCAL_DIR}/${LCM_BUNDLE}" -C "${LOCAL_DIR}/lcm"
```

- Ensure the ownership and permissions of the extracted files are correct

```bash
chown -R tftp:tftp "${LOCAL_DIR}"
```

- Configure the Nutanix LCM to use the remote site.

```yaml
# Example
URL: http://bootycall.saltlabs.cloud/nce-darksite/lcm
```

- Now you can download and transfer LCM product downloads to the remote server from your local machine.

```bash
REMOTE_DIR="bootycall:/mnt/hdd/tftpboot/nce-darksite/lcm"

# Example
rsync -avz --progress "${HOME}/Downloads/"lcm_*.tar.gz "${REMOTE_DIR}/"
```
