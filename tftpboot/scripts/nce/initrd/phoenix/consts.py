#
# Copyright (c) 2014 Nutanix Inc. All rights reserved.
#
# Author: thomas@nutanix.com
#
# This module contains shared constants.
#

import os
import re

from dell_factory_filter import DELL_DIR, has_dell_content

ISO_DIR = "/mnt/local"
if "COMMUNITY_EDITION" in os.environ:
  ISO_DIR = "/mnt/iso"
PREP_FLAG = "%s/.prepared" % ISO_DIR
IMAGES_DIR = "%s/images" % ISO_DIR
PHOENIX_DIR = "/phoenix"
HYP_IMAGES = "%s/hypervisor" % IMAGES_DIR
SVM_VERSIONS = "%s/svm" % IMAGES_DIR
SVM_INSTALL_BASE = "/mnt/svm_installer"
NOS_FROM_SVM = "/mnt/nos_from_svm"
SVM_INSTALL_SSH_KEYS_DIR = "%s/install/ssh_keys" % SVM_INSTALL_BASE
SSH_KEYS_DIR = "%s/common/ssh_keys" % PHOENIX_DIR
HCL_JSON = "/phoenix/hcl.json"
IPMICFG = "/usr/bin/ipmicfg-linux.x86_64"
SMARTCTL = "/usr/sbin/smartctl"
SATADOM_TOOLS_PATH = "/usr/local/satadom-firmware/S170119"
ROOT = "/root"
UPDATES_PATH = os.path.join(ROOT, "updates")
FEATURES_JSON_UPDATE = os.path.join(UPDATES_PATH, "features.json")
FEATURES_JSON = "/phoenix/features.json"
CVM_HOME_PART_INFO_PATH = "/phoenix/.cvm_home_partition_info"
DRIVERS_DIR = "/tmp/drivers"
DRIVER_PACKAGE_NAME = "driver_package.tar.gz"
NOS_TAR = "nos.tar"
NOS_TAR_GZ = "nos.tar.gz"

# Community edition Constants for disk selection
MAX_DEV = 16
MAX_DISK_SERIAL = 16
MAX_MODEL = 24
MAX_SZ = 7
MAX_TYPE = 7

# Constants for firmware upgrade.
FIRMWARE_MODULES_PATH = "/phoenix/firmware_modules"
FIRMWARE_UPGRADE_FLOCK_PATH = "/tmp/firmware_install_lock"
SUM16_PATH = "/usr/local/bin/sum1.6"
SUM_PATH = "/usr/local/bin/sum"
FIRMWARE_CONFIG_PATH = "/etc/nutanix/firmware_config.json"
HARDWARE_CONFIG_PATH = "/etc/nutanix/hardware_config.json"
FIRMWARE_UPGRADE_DONE_MARKER = "/tmp/.firmware_upgrade_finished"

# Additional config to be read for factory.
# dell_factory.json/factory.json triggers factory workflow.
# This file doesn't and is meant to allow factory to specify parameters for
# phoenix which can be used in field and in factory.
# e.g. factory_hypervisors_location, factory_drivers_location.
FACTORY_PHOENIX_CONFIG = "factory_phoenix_config.json"

# Factory exchange directory.
FACTORY_EXCHANGE_DIR = "/mnt/local/factory"

KVM_VERSION_REGEX = re.compile(r"el(\d+)\.nutanix\.(\S+)")

# Marker for CVM only imaging
CVM_ONLY_MARKER = "/tmp/cvm_only_imaging"

# AHV and ESXi are the only supported hypervisor types for Dell/UEFI nodes.
# Add to EFI boot labels list when other hypervisor support is added.
UEFI_HYP_LABELS = ["Nutanix AHV", "ESXi"]

# LOGS
REBOOT_TO_HOST_LOG = "/tmp/reboot_to_host.log"
MDADM_LOG = "/tmp/mdadm_logs.out"
DMESG_LOG = "/tmp/dmesg.out"
JOURNALCTL_LOG = "/tmp/journalctl_all.out"
FOUNDATION_CENTRAL_LOG = "/tmp/foundation_central.log"

def factory_phoenix_config():
  dell_phoenix_config_path = os.path.join(DELL_DIR, FACTORY_PHOENIX_CONFIG)
  factory_phoenix_config_path = os.path.join(FACTORY_EXCHANGE_DIR,
                                             FACTORY_PHOENIX_CONFIG)
  if os.path.exists(dell_phoenix_config_path):
    return dell_phoenix_config_path
  elif os.path.exists(factory_phoenix_config_path):
    return factory_phoenix_config_path
  else:
    return None


def factory_exchange_dir():
  if has_dell_content():
    return DELL_DIR
  else:
    return FACTORY_EXCHANGE_DIR


class ValidationError(Exception):
  pass

# This version is to be populated by $TOP/Makefile.media
PHOENIX_VERSION = "phoenix-5.6.1_8c4d61fc"

# NTP configuration
NTP_SERVERS = ["0.north-america.pool.ntp.org",
               "1.north-america.pool.ntp.org",
               "2.north-america.pool.ntp.org",
               "3.north-america.pool.ntp.org"]

MIN_NOS_VERSION_FOR_NS = "5.5"
MIN_NOS_VERSION_FOR_UEFI = "5.16"
MIN_NOS_FOR_PMEM = "6.7"
MIN_ESX_FOR_PMEM = "6.7"

# Constants to represent state of the system.
NOT_INSTALLED = "not_installed"
INSTALLED = "installed"
CUSTOMIZED = "customized"
FIRSTBOOT_SUCCESS = "firstboot_success"
FIRSTBOOT_FAILED = "firstboot_failed"

# Constants to represent state of the system.
ONE_NODE_INSTALL_SUCCESS = "one_node_install_success"
ONE_NODE_RF1 = "one_node_rf1"

PPC_SVM_THREADS = 4

#archs
ARCH_PPC = "ppc64le"
ARCH_X86 = "x86_64"

# hook scripts
HOOK_PRE_PHOENIX    = "pre_phoenix"
HOOK_POST_PHOENIX   = "post_phoenix"
HOOK_PRE_FIRSTBOOT  = "pre_firstboot"
HOOK_POST_FIRSTBOOT = "post_firstboot"

CVM_BOOT_MARKER = ".nutanix_active_svm_partition"

DEFAULT_IPV6_INTERFACE = "eth0"

# Marker to stop fc workflow if foundation workflow is going on.
FC_STOP_MARKER = "/tmp/.fc_stop_marker"
# https://docs.microsoft.com/en-us/windows-server/get-started/windows-server-release-info
HYPERV2O16_BUILD_VER = "10.0.14393"
HYPERV2O19_BUILD_VER = "10.0.17763"
HYPERV2O22_BUILD_VER = "10.0.20348"
HYPERV_BUILD_VERSION_MAP = {
  # build version: hyperv version
  HYPERV2O16_BUILD_VER: "2016",
  HYPERV2O19_BUILD_VER: "2019",
  HYPERV2O22_BUILD_VER: "2022"
}

# AHV LVM consts
DEV_MAPPER_PREFIX = "/dev/mapper/"
LVM_VG="ahv"
DEV_VG_PREFIX = DEV_MAPPER_PREFIX + LVM_VG
LVM_ROOT = DEV_VG_PREFIX + "-root"
LVM_VAR = DEV_VG_PREFIX + "-var"
LVM_VAR_LOG = DEV_VG_PREFIX + "-varlog"
LVM_VAR_LOG_AUDIT = DEV_VG_PREFIX + "-varlogaudit"
LVM_HOME = DEV_VG_PREFIX + "-home"
LVM_TMP = DEV_VG_PREFIX  + "-tmp"
LVM_BOOT = "/dev/disk/by-label/boot"

UEFI_DEV = "/dev/disk/by-label/EFI"

# The order matters when we iterate over it while mounting
# thus keeping it in a list
ahv_lvm_dirs = [ ("/", LVM_ROOT),
                 ("/boot", LVM_BOOT),
                 ("/var", LVM_VAR),
                 ("/var/log", LVM_VAR_LOG),
                 ("/var/log/audit", LVM_VAR_LOG_AUDIT),
                 ("/home", LVM_HOME),
                 ("/tmp", LVM_TMP)
                ]

# HyperV unsupported nics.
UNSUPPORTED_NICS_HYPERV = {
    # (modelname, vendor_id, device_id, subdevice_id, subvendor_id)
    "2022": {("8086", "1528", "088a", "15d9"): "Intel X540",
             ("8086", "1528", "085f", "15d9"): "Intel X540",
             ("8086", "1528", "085d", "15d9"): "Intel X540",
             ("8086", "1528", "1528", "8086"): "Intel X540",
             ("8086", "10fb", "000c", "8086"): "Intel 82599",
             ("8086", "10fb", "0611", "15d9"): "Intel 82599",
             ("8086", "1521", "1521", "15d9"): "Intel i350"}}

# Driver list of non ethernet network device.
NON_ETHERNET_DEV_DRIVERS = ["cdc_ether", "rndis_host", "cdc_eem", "cdce"]
COLD_REBOOT_MODELS = ["NX-8155-G7", "NX-8155-G6"]
SVM_MARKER = "/tmp/svm_marker"

#LUKS support
ENABLE_LUKS_AOS = "--enable_luks_aos"
SVMBOOT_UPDATED = "/tmp/svm_install_chroot/svmboot_new.iso"

INTERSIGHT_CONFIG = "/tmp/cisco_intersight_fc_metadata.json"
