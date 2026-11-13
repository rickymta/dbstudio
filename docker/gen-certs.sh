#!/bin/sh
# Generates a throw-away CA and a server certificate for localhost / 127.0.0.1,
# used by the TLS-enabled test databases in compose.test.yml. Test use only.
set -eu
dir="$(cd "$(dirname "$0")" && pwd)/certs"
mkdir -p "$dir"
cd "$dir"
if [ -f ca.crt ] && [ -f server.crt ] && [ -f server.key ] && [ -f other-ca.crt ]; then
  echo "certs already exist in $dir"
  exit 0
fi
openssl genrsa -out ca.key 2048 2>/dev/null
openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj "/CN=DBStudio Test CA" -out ca.crt
openssl genrsa -out server.key 2048 2>/dev/null
openssl req -new -key server.key -subj "/CN=localhost" -out server.csr
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 3650 -sha256 -extfile server.ext -out server.crt 2>/dev/null
# A second, unrelated CA: connecting with it must fail verification.
openssl genrsa -out other-ca.key 2048 2>/dev/null
openssl req -x509 -new -key other-ca.key -sha256 -days 3650 -subj "/CN=DBStudio Other CA" -out other-ca.crt
chmod 644 server.key
rm -f server.csr server.ext ca.srl
echo "generated certs in $dir"
