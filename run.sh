#!/usr/bin/env bash

while sleep 1; do
    if [ -f dd2-server-next ]; then
        cp dd2-server-next dd2-server
    fi
    ./dd2-server || echo "\nserver exiting\n"
done
