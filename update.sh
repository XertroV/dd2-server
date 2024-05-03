#!/usr/bin/env bash

set -e
set -x

systemctl status dd2-server | cat
systemctl restart dd2-server
systemctl status dd2-server | cat

sleep 1

echo "opening logs in follow mode.."

sleep 1

journalctl -fu dd2-server
