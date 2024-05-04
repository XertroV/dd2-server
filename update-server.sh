#!/usr/bin/env bash

# config.json
# server binary
# setup script
# migrations

cargo build --release || exit 1


function sync_server() {
    SERVERNAME=$1
    # ssh $SERVERNAME "mkdir -p ~/plugin-server"
    # rsync -avz config.json $SERVERNAME:~/plugin-server/ &
    rsync -avz update.sh $SERVERNAME:~/plugin-server/update.sh &
    rsync -avz logs.sh $SERVERNAME:~/plugin-server/logs.sh &
    rsync -avz run.sh $SERVERNAME:~/plugin-server/run.sh &
    rsync -avz target/release/dd2-server $SERVERNAME:~/plugin-server/ &
    rsync -avz target/release/dd2-server $SERVERNAME:~/plugin-server/dd2-server-next &
    rsync -avz -r migrations $SERVERNAME:~/plugin-server/ &
    # scp -C -r ./ssl/* $SERVERNAME:~/plugin-server/ &
    # rsync -avz server-setup.sh $SERVERNAME:~/plugin-server/ &
    # scp -C -r server $SERVERNAME:~/plugin-server/ &
    wait
}

sync_server "DipsPP-server1"
sync_server "dpp-02"
sync_server "dpp-03"
sync_server "dpp-04"
# ssh $SERVERNAME "cd ~/plugin-server && ./server-setup.sh"
