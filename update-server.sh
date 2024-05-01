#!/usr/bin/env bash

# config.json
# server binary
# setup script
# migrations

cargo build --release

# ssh DD2-server1 "mkdir -p ~/plugin-server"
# rsync -auv config.json DD2-server1:~/plugin-server/
rsync -auv run.sh DD2-server1:~/plugin-server/run.sh
rsync -auv target/release/dd2-server DD2-server1:~/plugin-server/ &
rsync -auv target/release/dd2-server DD2-server1:~/plugin-server/dd2-server-next &
rsync -auv -r migrations DD2-server1:~/plugin-server/ &
# rsync -auv server-setup.sh DD2-server1:~/plugin-server/ &
# scp -C -r server DD2-server1:~/plugin-server/ &
wait

ssh DD2-server1 "cd ~/plugin-server && ./server-setup.sh"
