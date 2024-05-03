#!/usr/bin/env bash

# config.json
# server binary
# setup script
# migrations

cargo build --release || exit 1

SERVERNAME="DipsPP-server1"

# ssh $SERVERNAME "mkdir -p ~/plugin-server"
# rsync -avz config.json $SERVERNAME:~/plugin-server/
# rsync -avz run.sh $SERVERNAME:~/plugin-server/run.sh
rsync -avz target/release/dd2-server $SERVERNAME:~/plugin-server/ &
rsync -avz target/release/dd2-server $SERVERNAME:~/plugin-server/dd2-server-next &
rsync -avz -r migrations $SERVERNAME:~/plugin-server/ &
# rsync -avz server-setup.sh $SERVERNAME:~/plugin-server/ &
# scp -C -r server $SERVERNAME:~/plugin-server/ &
wait

# ssh $SERVERNAME "cd ~/plugin-server && ./server-setup.sh"
