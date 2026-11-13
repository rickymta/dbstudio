#!/bin/sh
# Enable TCP forwarding in the linuxserver sshd (disabled by default) so tunnels can reach
# pg16 / mysql8 / mssql2022 ... by compose service name. Runs from /custom-cont-init.d.
f=/config/sshd/sshd_config
[ -f "$f" ] || exit 0
sed -i '/^#\?AllowTcpForwarding/d;/^#\?GatewayPorts/d' "$f"
printf 'AllowTcpForwarding yes\nGatewayPorts no\n' >> "$f"
