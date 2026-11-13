#!/bin/sh
# Generates the throw-away SSH client key pair used by the `sshd` test service (key login with a
# passphrase). The files are git-ignored and only created when missing. Test use only.
#   private key : docker/ssh/id_ed25519      passphrase: dbstudio_key_pass
#   public key  : docker/ssh/id_ed25519.pub  (mounted into the sshd container as authorized key)
set -eu
dir="$(cd "$(dirname "$0")" && pwd)/ssh"
mkdir -p "$dir"
if [ -f "$dir/id_ed25519" ] && [ -f "$dir/id_ed25519.pub" ]; then
  echo "ssh keys already exist in $dir"
  exit 0
fi
rm -f "$dir/id_ed25519" "$dir/id_ed25519.pub"
ssh-keygen -q -t ed25519 -N 'dbstudio_key_pass' -C 'dbstudio-test' -f "$dir/id_ed25519"
chmod 644 "$dir/id_ed25519.pub"
echo "generated ssh keys in $dir"
