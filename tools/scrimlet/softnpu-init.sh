#!/bin/bash

set -e
set -x

# Gateway ip is automatically configured based on the default route on your development machine
# Can be overridden by setting GATEWAY_IP
GATEWAY_IP=${GATEWAY_IP:=$(netstat -rn -f inet | grep default | awk -F ' ' '{print $2}')}
echo "Using $GATEWAY_IP as gateway ip"

# Gateway mac is determined automatically by inspecting the arp table on the development machine
# Can be overridden by setting GATEWAY_MAC
GATEWAY_MAC=${GATEWAY_MAC:=$(arp "$GATEWAY_IP" | awk -F ' ' '{print $4}')}
echo "Using $GATEWAY_MAC as gateway mac"

z_swadm () {
    pfexec zlogin oxz_switch /opt/oxide/dendrite/bin/swadm $@
}


# Add front facing port
z_swadm port create 1:0 100G RS
z_swadm port create 2:0 100G RS

# Configure sidecar local ipv6 addresses
z_swadm addr add rear0/0 fe80::aae1:deff:fe01:701c
z_swadm addr add qsfp0/0 fe80::aae1:deff:fe01:701d
z_swadm addr add rear0/0 fd00:99::1

# Configure route to the "sled"
z_swadm route add fd00:1122:3344:0101::/64 rear0/0 fe80::aae1:deff:fe00:1
# Create NDP entry for the "sled"
z_swadm arp add fe80::aae1:deff:fe00:1 a8:e1:de:00:00:01

# Configure upstream network gateway ARP entry
z_swadm arp add "$GATEWAY_IP" "$GATEWAY_MAC"
# Configure route to upstream gateway
z_swadm route add 0.0.0.0/0 qsfp0/0 "$GATEWAY_IP"

z_swadm port list
z_swadm addr list
z_swadm route list
z_swadm arp list
