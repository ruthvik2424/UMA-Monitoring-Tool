#!/usr/bin/env bash
# Tiny mTLS PKI for UMA — one CA, N server certs, N client certs.
# OpenSSL-only so it works on any host.
#
# Usage:
#   ./gen-certs.sh ca                           # bootstrap the CA
#   ./gen-certs.sh server <fqdn> [<altnames>…]  # issue collector server cert
#   ./gen-certs.sh client <hostname>            # issue per-host agent client cert
#   ./gen-certs.sh list                         # list issued certs
#
# Output files (so server/client can never overwrite each other when the
# same name is used for both — common during smoke tests on one host):
#   out/ca.{crt,key}
#   out/<name>.server.{crt,key}
#   out/<name>.client.{crt,key}

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$HERE/out"
mkdir -p "$OUT"

cmd="${1:-}"; shift || true

case "$cmd" in
  ca)
    openssl genrsa -out "$OUT/ca.key" 4096
    openssl req -x509 -new -nodes -key "$OUT/ca.key" -sha256 -days 3650 \
      -subj "/CN=UMA Root CA/O=UMA" \
      -addext "basicConstraints=critical,CA:TRUE,pathlen:1" \
      -addext "keyUsage=critical,keyCertSign,cRLSign" \
      -out "$OUT/ca.crt"
    echo "01" > "$OUT/ca.srl"
    echo "CA generated:"
    echo "  $OUT/ca.crt"
    echo "  $OUT/ca.key"
    ;;

  server)
    name="${1:?usage: gen-certs.sh server <fqdn> [<altnames>...]}"
    shift || true
    extra=("$@")
    cnf="$OUT/$name.server.cnf"
    {
      echo "[req]"
      echo "distinguished_name=req"
      echo "req_extensions=v3"
      echo "[v3]"
      echo "subjectAltName=@alt"
      echo "extendedKeyUsage=serverAuth"
      echo "keyUsage=critical,digitalSignature,keyEncipherment"
      echo "[alt]"
      i=1
      d=1
      p=1
      echo "DNS.${d} = $name"
      d=$((d+1))
      for alt in "${extra[@]}"; do
        if [[ "$alt" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
          echo "IP.${p} = $alt"
          p=$((p+1))
        else
          echo "DNS.${d} = $alt"
          d=$((d+1))
        fi
        i=$((i+1))
      done
    } > "$cnf"
    openssl genrsa -out "$OUT/$name.server.key" 4096
    openssl req -new -key "$OUT/$name.server.key" -out "$OUT/$name.server.csr" \
      -subj "/CN=$name/O=UMA/OU=server" -config "$cnf" -reqexts v3
    openssl x509 -req -in "$OUT/$name.server.csr" \
      -CA "$OUT/ca.crt" -CAkey "$OUT/ca.key" -CAserial "$OUT/ca.srl" \
      -out "$OUT/$name.server.crt" -days 825 -sha256 \
      -extfile "$cnf" -extensions v3
    rm -f "$OUT/$name.server.csr" "$cnf"
    echo "Server cert issued:"
    echo "  $OUT/$name.server.crt"
    echo "  $OUT/$name.server.key"
    ;;

  client)
    name="${1:?usage: gen-certs.sh client <hostname>}"
    cnf="$OUT/$name.client.cnf"
    {
      echo "[req]"
      echo "distinguished_name=req"
      echo "req_extensions=v3"
      echo "[v3]"
      echo "extendedKeyUsage=clientAuth"
      echo "keyUsage=critical,digitalSignature,keyEncipherment"
      echo "subjectAltName=DNS:$name"
    } > "$cnf"
    openssl genrsa -out "$OUT/$name.client.key" 4096
    openssl req -new -key "$OUT/$name.client.key" -out "$OUT/$name.client.csr" \
      -subj "/CN=$name/O=UMA/OU=agent" -config "$cnf" -reqexts v3
    openssl x509 -req -in "$OUT/$name.client.csr" \
      -CA "$OUT/ca.crt" -CAkey "$OUT/ca.key" -CAserial "$OUT/ca.srl" \
      -out "$OUT/$name.client.crt" -days 825 -sha256 \
      -extfile "$cnf" -extensions v3
    rm -f "$OUT/$name.client.csr" "$cnf"
    echo "Client cert issued:"
    echo "  $OUT/$name.client.crt"
    echo "  $OUT/$name.client.key"
    ;;

  list)
    ls -1 "$OUT"
    ;;

  *)
    cat <<EOF
Usage:
  gen-certs.sh ca
  gen-certs.sh server <fqdn> [<altname1> <altname2> ...]
  gen-certs.sh client <hostname>
  gen-certs.sh list

Output: $OUT/
  ca.{crt,key}
  <name>.server.{crt,key}    (EKU=serverAuth)
  <name>.client.{crt,key}    (EKU=clientAuth)

Outputs are intentionally NOT committed to git (see .gitignore).
EOF
    exit 1
    ;;
esac
