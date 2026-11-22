#!/usr/bin/env bash

docker run --rm -i "${NIX_IMAGE:?}" /bin/sh -e <<'SH'
test -r "$SSL_CERT_FILE"
test -w "$HOME"
test -w /tmp
ssh -V
rsync --version
rworker help exec
SH

# vim: set ft=sh et ts=2 sw=2 :
