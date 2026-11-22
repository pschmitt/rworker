#!/usr/bin/env bash

check_runtime() {
  docker run --rm "$@" \
    --env RWORKER_CHECK_HTTPS=1 \
    --env RWORKER_PROFILE=gl-linux-x64-lease \
    --volume "$PWD/scripts/github-job-agent.py:/agent.py:ro" \
    "${NIX_IMAGE:?}" python3 /agent.py --check-runtime
}

main() {
  check_runtime --env RWORKER_UNENCRYPTED_PROFILES=gl-linux-x64-lease || return 1
  check_runtime --privileged
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi

# vim: set ft=sh et ts=2 sw=2 :
