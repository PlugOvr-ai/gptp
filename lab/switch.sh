#!/bin/sh
# Lab "automotive switch": bridges all of the container's ports at layer 2
# and forwards the gPTP multicast (01:80:C2:00:00:0E), which Linux bridges
# drop by default. Remembers which port faces which lab network so faults
# (tc netem) can be injected per port: /run/lab/port-<subnet third octet>.
set -eu

mkdir -p /run/lab
ip link add br0 type bridge group_fwd_mask 0x4000
ports=
for path in /sys/class/net/eth*; do
    port=${path##*/}
    ports="$ports $port"
    octet=$(ip -4 -o addr show dev "$port" | sed -n 's/.* [0-9]*\.[0-9]*\.\([0-9]*\)\.[0-9]*\/.*/\1/p')
    [ -n "$octet" ] && echo "$port" > "/run/lab/port-$octet"
    ip addr flush dev "$port"
    ip link set "$port" master br0
done
ip link set br0 up
touch /run/lab/ready
echo "switch up, ports:$ports"
exec sleep infinity
