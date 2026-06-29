#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Install the uartfs console front-end as the owner of a serial line, with serial-getty as the
# fallback. Run on-device as root. TTY defaults to ttySAC0 (felix); override with $1.
#
# Bootstrap note: the very first install is done over the *agentless floor* — `uart run` pushes
# this binary and runs this script — because there's no front-end yet to push it framed.

set -eu
TTY="${1:-ttySAC0}"

if [ ! -x /usr/local/bin/uartfs-frontend ]; then
    echo "error: /usr/local/bin/uartfs-frontend not found (push it first)" >&2
    exit 1
fi

install -m 0644 "$(dirname "$0")/uartfs-console@.service" \
    /etc/systemd/system/uartfs-console@.service

systemctl daemon-reload
# The front-end and a getty can't share the line; disabling the getty hands it over.
systemctl disable --now "serial-getty@${TTY}.service" 2>/dev/null || true
systemctl enable --now "uartfs-console@${TTY}.service"

echo "uartfs console front-end now owns /dev/${TTY}"
echo "fallback: on repeated failure it hands the line back to serial-getty@${TTY}"
