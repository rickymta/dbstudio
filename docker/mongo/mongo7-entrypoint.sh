#!/bin/sh
# mongo7 test service: single-node replica set rs0 with keyFile auth and preferTLS.
# Wraps the official entrypoint: prepares the (non-secret) keyFile, then starts mongod.
set -eu
printf '%s\n' 'ZGJzdHVkaW8tdGVzdC1tb25nby1rZXlmaWxlLW5vdC1hLXNlY3JldC0wMTIzNDU2Nzg5' > /etc/mongo-keyfile
chown 999:999 /etc/mongo-keyfile
chmod 400 /etc/mongo-keyfile
exec docker-entrypoint.sh mongod --replSet rs0 --keyFile /etc/mongo-keyfile --bind_ip_all \
  --tlsMode preferTLS --tlsCertificateKeyFile /certs/mongo.pem --tlsCAFile /certs/ca.crt \
  --tlsAllowConnectionsWithoutCertificates
