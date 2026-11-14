#!/bin/sh
# Generates a throw-away CA and certificates used by the TLS-enabled test databases in
# compose.test.yml. Test use only. Idempotent: every file is generated only when missing, and the
# existing CA is never regenerated once ca.crt/ca.key exist.
#
#   server.crt/.key      pg16, mysql8 (localhost / 127.0.0.1)
#   other-ca.crt/.key    unrelated CA: verification against it must fail
#   mongo.pem            mongo7 server cert + key combined (+ ca.crt as the CA file);
#                        EKU serverAuth,clientAuth (re-issued when it lacks clientAuth)
#   mongo-client.pem     client cert + key combined (Mongo X.509-style tests, CN=dbstudio-client)
#   redis.crt/.key       redis7 server cert + key (ca.crt is the CA)
#   scylla.crt/.key      scylla (SAN localhost, 127.0.0.1, scylla)
#   clickhouse.crt/.key  clickhouse26 HTTPS (SAN localhost, 127.0.0.1, clickhouse26)
#   cockroach/           --certs-dir of the cockroach service: ca.crt, node.crt/key (CN=node),
#                        client.root.crt/key (CN=root)
set -eu
dir="$(cd "$(dirname "$0")" && pwd)/certs"
mkdir -p "$dir"
cd "$dir"

# issue <name> <ext-file-content>: key + CSR + cert signed by the test CA.
issue() {
  name="$1"; cn="$2"; ext="$3"
  openssl genrsa -out "$name.key" 2048 2>/dev/null
  openssl req -new -key "$name.key" -subj "/CN=$cn" -out "$name.csr"
  printf '%s\n' "$ext" > "$name.ext"
  openssl x509 -req -in "$name.csr" -CA ca.crt -CAkey ca.key -CAcreateserial -days 3650 -sha256 \
    -extfile "$name.ext" -out "$name.crt" 2>/dev/null
  rm -f "$name.csr" "$name.ext"
}

# Subject alternative names shared by the service certificates (host + compose service names).
san='subjectAltName=DNS:localhost,IP:127.0.0.1,DNS:mongo44,DNS:mongo7,DNS:redis6,DNS:redis7,DNS:valkey8'
srv_ext="$san
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth"

if [ ! -f ca.crt ] || [ ! -f ca.key ]; then
  openssl genrsa -out ca.key 2048 2>/dev/null
  openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj "/CN=DBStudio Test CA" -out ca.crt
  echo "generated CA"
fi

if [ ! -f server.crt ] || [ ! -f server.key ]; then
  issue server localhost 'subjectAltName=DNS:localhost,IP:127.0.0.1
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth'
  echo "generated server.crt"
fi

# A second, unrelated CA: connecting with it must fail verification.
if [ ! -f other-ca.crt ] || [ ! -f other-ca.key ]; then
  openssl genrsa -out other-ca.key 2048 2>/dev/null
  openssl req -x509 -new -key other-ca.key -sha256 -days 3650 -subj "/CN=DBStudio Other CA" -out other-ca.crt
  echo "generated other-ca.crt"
fi

if [ ! -f redis.crt ] || [ ! -f redis.key ]; then
  issue redis localhost "$srv_ext"
  echo "generated redis.crt"
fi

# The Mongo server certificate doubles as the replica-set member certificate, so it needs both
# serverAuth and clientAuth. A mongo.pem generated earlier without clientAuth is re-issued (the CA
# is untouched). To force a new one: rm docker/certs/mongo.pem && docker/gen-certs.sh, then
# recreate the mongo containers (the mongod keyFile / TLS files are read at start-up).
mongo_ext="$san
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth,clientAuth"
if [ -f mongo.pem ] && ! openssl x509 -in mongo.pem -noout -text 2>/dev/null | grep -q "Client Authentication"; then
  echo "mongo.pem has no clientAuth extended key usage: re-issuing it"
  rm -f mongo.pem
fi
if [ ! -f mongo.pem ]; then
  issue mongo-srv localhost "$mongo_ext"
  cat mongo-srv.crt mongo-srv.key > mongo.pem
  rm -f mongo-srv.crt mongo-srv.key
  echo "generated mongo.pem"
fi

if [ ! -f mongo-client.pem ]; then
  issue mongo-cli dbstudio-client 'basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=clientAuth'
  cat mongo-cli.crt mongo-cli.key > mongo-client.pem
  rm -f mongo-cli.crt mongo-cli.key
  echo "generated mongo-client.pem"
fi

# Backlog + phase 9 services. Server certificates for the CQL / HTTP databases (host + compose
# service names as SAN), issued only when missing.
extra_srv_ext() {
  printf '%s\n' "subjectAltName=DNS:localhost,IP:127.0.0.1,DNS:$1" \
    "basicConstraints=CA:FALSE" "keyUsage=digitalSignature,keyEncipherment" \
    "extendedKeyUsage=${2:-serverAuth}"
}
if [ ! -f scylla.crt ] || [ ! -f scylla.key ]; then
  issue scylla localhost "$(extra_srv_ext scylla)"
  echo "generated scylla.crt"
fi
if [ ! -f clickhouse.crt ] || [ ! -f clickhouse.key ]; then
  issue clickhouse localhost "$(extra_srv_ext clickhouse26)"
  echo "generated clickhouse.crt"
fi

# CockroachDB certs directory (mounted as --certs-dir): ca.crt, node.crt/key (CN=node, SAN
# localhost, cockroach, 127.0.0.1) and the client certificate of the root user. Same CA as above.
mkdir -p cockroach
if [ ! -f cockroach/node.crt ] || [ ! -f cockroach/node.key ] || [ ! -f cockroach/ca.crt ]; then
  cp ca.crt cockroach/ca.crt
  issue cockroach-node node "$(extra_srv_ext cockroach serverAuth,clientAuth)"
  mv cockroach-node.crt cockroach/node.crt
  mv cockroach-node.key cockroach/node.key
  echo "generated cockroach/node.crt"
fi
if [ ! -f cockroach/client.root.crt ] || [ ! -f cockroach/client.root.key ]; then
  issue cockroach-root root 'basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth'
  mv cockroach-root.crt cockroach/client.root.crt
  mv cockroach-root.key cockroach/client.root.key
  echo "generated cockroach/client.root.crt"
fi
chmod 644 cockroach/*

# Containers run as uid 999 (postgres/mongodb/redis): the test keys must be world-readable.
chmod 644 *.key *.pem *.crt
rm -f ca.srl
echo "certs ready in $dir"
