#!/usr/bin/env bash

set -e
set -x

systemctl status dd2-server | cat
systemctl restart dd2-server
systemctl status dd2-server | cat

echo "opening logs in follow mode.."

sleep 0.25

journalctl -fu dd2-server
