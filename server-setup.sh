#!/usr/bin/env bash

# docker and postgres


# Add Docker's official GPG key:
sudo apt-get update
sudo apt-get install ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc

# Add the repository to Apt sources:
echo \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu \
  $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | \
  sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt-get update

sudo apt-get install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin -y

sudo apt-get install dotnet-runtime-8.0 postgresql-client openssl -y


# firewall


ufw allow 22
ufw allow 80/tcp
ufw allow 443/tcp
ufw allow 17677/tcp
ufw allow 27677/tcp
ufw allow 19796/tcp
ufw enable || ufw reload | tee /dev/null

PG_PASSWORD="$(openssl rand -hex 20)"
# check for .pg_password
if [ -f .pg_password ]; then
    PG_PASSWORD="$(cat .pg_password)"
else
    echo "$PG_PASSWORD" > .pg_password
    chmod 600 .pg_password
    rm -rf server/postgres-data
fi

# postgres

(
    cd ./server &&
    (cat docker-compose.yml | sed "s/examplePgPassword1234/$PG_PASSWORD/g" > docker-compose.yml2) &&
    cp docker-compose.yml2 docker-compose.yml &&
    ./restart_quiet.sh
)

echo "DATABASE_URL=postgresql://postgres:$PG_PASSWORD@localhost:5432/postgres" > .env
chmod 600 .env

echo "127.0.0.1:5432::postgres:$PG_PASSWORD" > ~/.pgpass
chmod 600 ~/.pgpass


psql -U postgres -h 127.0.0.1 -c "CREATE DATABASE deepdip2;"
