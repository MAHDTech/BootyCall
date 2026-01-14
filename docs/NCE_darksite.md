# Nutanix Community Edition Darksite

## Overview

Instructions for setting up a Nutanix Community Edition Darksite on Linux.

This is a summary from the official docs available in the guide [Web Server Upload Method](https://portal.nutanix.com/page/documents/details?targetId=Life-Cycle-Manager-Guide-v3_3:top-web-server-based-upload-method-c.html)

## Prerequisites

Make sure you already have a running web server. In this example, we will use Caddy.

## Setup

- Download the latest LCM bundles from the [Nutanix Portal for LCM](https://portal.nutanix.com/page/downloads?product=lcm)

NOTE: Each component has their own bundle, download all components you need.

For example;

- [AHV](https://portal.nutanix.com/page/downloads?product=ahv)
- [AOS](https://portal.nutanix.com/page/downloads?product=nos)
- [File Analytics](https://portal.nutanix.com/page/downloads?product=fileanalytics)
- [Files](https://portal.nutanix.com/page/downloads?product=afs)
- [Flow Networking and Security](https://portal.nutanix.com/page/downloads?product=flowNetworkSecurity)
- [Foundation](https://portal.nutanix.com/page/downloads?product=foundation)
- [LCM](https://portal.nutanix.com/page/downloads?product=lcm)
- [License Manager](https://portal.nutanix.com/page/downloads?product=licensemanager)
- [Microservices Platform (MSP)](https://portal.nutanix.com/page/downloads?product=msp)
- [NCC](https://portal.nutanix.com/page/downloads?product=ncc)
- [Objects](https://portal.nutanix.com/page/downloads?product=objects)
- [Power Monitor](https://portal.nutanix.com/page/downloads?product=powerMonitor)
- [Prism Central](https://portal.nutanix.com/page/downloads?product=prism)
- [Security Dashboard](https://portal.nutanix.com/page/downloads?product=security)
- [Self-Service](https://portal.nutanix.com/page/downloads?product=calm)

- Transfer the LCM bundles to the web server

```bash
LCM_BUNDLES_REMOTE="bootycall:/mnt/hdd/tftpboot/nce-darksite/lcm-bundles"

rsync -avz --progress lcm_*.tar.gz "${LCM_BUNDLES_REMOTE}/"
```

- Extract the LCM bundles into the Darksite `release` directory on the remote server

```bash
ssh bootycall

LCM_BUNDLES_LOCAL="/mnt/hdd/tftpboot/nce-darksite/lcm-bundles"
LCM_BUNDLES_EXTRACTED="/mnt/hdd/tftpboot/nce-darksite/release"

mkdir -p "${LCM_BUNDLES_EXTRACTED}"

BUNDLE_COUNT=0
for BUNDLE in "${LCM_BUNDLES_LOCAL}/"lcm_*.tar.gz;
do
  if [ -f "${BUNDLE}" ]; then
    echo "Extracting ${BUNDLE} to ${LCM_BUNDLES_EXTRACTED}"
    tar -xvzf "${BUNDLE}" -C "${LCM_BUNDLES_EXTRACTED}" || {
        echo "Failed to extract ${BUNDLE}, skipping..."
        continue
    }
    ((BUNDLE_COUNT++))
  fi
done
echo "Extracted ${BUNDLE_COUNT} bundles"
```

- Ensure the ownership and permissions of the extracted files are correct

```bash
chown -R tftp:tftp "${LCM_BUNDLES_LOCAL}"
```

- Configure the Nutanix LCM to use the remote site.

```yaml
# Example
URL: http://bootycall.saltlabs.cloud/nce-darksite/release
```
