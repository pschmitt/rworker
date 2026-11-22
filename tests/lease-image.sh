#!/usr/bin/env bash

docker run --rm --network none   --env RWORKER_PROFILE=gl-linux-x64-lease   --volume "$PWD/scripts/github-job-agent.py:/agent.py:ro"   "${NIX_IMAGE:?}" python3 /agent.py --check-runtime

# vim: set ft=sh et ts=2 sw=2 :
