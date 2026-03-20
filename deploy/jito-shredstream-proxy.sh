#!/usr/bin/env bash
set -euo pipefail

MODE="${JITO_SHREDSTREAM_MODE:-docker}"
PROXY_BIN="${JITO_SHREDSTREAM_PROXY_BIN:-/usr/local/bin/jito-shredstream-proxy}"
PROXY_SUBCOMMAND="${JITO_SHREDSTREAM_SUBCOMMAND:-shredstream}"
DOCKER_BIN="${JITO_SHREDSTREAM_DOCKER_BIN:-/usr/bin/docker}"
DOCKER_IMAGE="${JITO_SHREDSTREAM_IMAGE:-jitolabs/jito-shredstream-proxy:latest}"
DOCKER_CONTAINER_NAME="${JITO_SHREDSTREAM_CONTAINER_NAME:-jito-shredstream-proxy}"
AUTH_KEYPAIR="${JITO_SHREDSTREAM_AUTH_KEYPAIR:-}"
BLOCK_ENGINE_URL="${JITO_SHREDSTREAM_BLOCK_ENGINE_URL:-https://mainnet.block-engine.jito.wtf}"
DESIRED_REGIONS="${JITO_SHREDSTREAM_DESIRED_REGIONS:-frankfurt}"
# Default to the local Agave TVU port. Override with
# JITO_SHREDSTREAM_DEST_IP_PORTS if you want to feed another consumer.
DEST_IP_PORTS="${JITO_SHREDSTREAM_DEST_IP_PORTS:-127.0.0.1:8802}"
SRC_BIND_PORT="${JITO_SHREDSTREAM_SRC_BIND_PORT:-}"
EXTRA_ARGS="${JITO_SHREDSTREAM_EXTRA_ARGS:-}"

if [[ -z "${AUTH_KEYPAIR}" ]]; then
    echo "JITO_SHREDSTREAM_AUTH_KEYPAIR is required" >&2
    exit 1
fi

if [[ "${MODE}" == "native" ]]; then
    if [[ ! -x "${PROXY_BIN}" ]]; then
        echo "jito-shredstream-proxy not found or not executable: ${PROXY_BIN}" >&2
        exit 1
    fi

    native_args=(
        "--block-engine-url" "${BLOCK_ENGINE_URL}"
        "--auth-keypair" "${AUTH_KEYPAIR}"
        "--desired-regions" "${DESIRED_REGIONS}"
        "--dest-ip-ports" "${DEST_IP_PORTS}"
    )

    if [[ -n "${SRC_BIND_PORT}" ]]; then
        native_args+=("--src-bind-port" "${SRC_BIND_PORT}")
    fi

    if [[ -n "${EXTRA_ARGS}" ]]; then
        read -r -a extra <<<"${EXTRA_ARGS}"
        native_args+=("${extra[@]}")
    fi

    exec "${PROXY_BIN}" "${PROXY_SUBCOMMAND}" "${native_args[@]}"
fi

if [[ ! -x "${DOCKER_BIN}" ]]; then
    echo "docker not found or not executable: ${DOCKER_BIN}" >&2
    exit 1
fi

container_auth_path="/app/jito-auth.json"
docker_args=(
    "run"
    "--rm"
    "--name" "${DOCKER_CONTAINER_NAME}"
    "--network" "host"
    "--env" "RUST_LOG=info"
    "--env" "BLOCK_ENGINE_URL=${BLOCK_ENGINE_URL}"
    "--env" "AUTH_KEYPAIR=${container_auth_path}"
    "--env" "DESIRED_REGIONS=${DESIRED_REGIONS}"
    "--env" "DEST_IP_PORTS=${DEST_IP_PORTS}"
    "--volume" "${AUTH_KEYPAIR}:${container_auth_path}:ro"
    "${DOCKER_IMAGE}"
    "${PROXY_SUBCOMMAND}"
)

if [[ -n "${SRC_BIND_PORT}" ]]; then
    docker_args=(
        "run"
        "--rm"
        "--name" "${DOCKER_CONTAINER_NAME}"
        "--network" "host"
        "--env" "RUST_LOG=info"
        "--env" "BLOCK_ENGINE_URL=${BLOCK_ENGINE_URL}"
        "--env" "AUTH_KEYPAIR=${container_auth_path}"
        "--env" "DESIRED_REGIONS=${DESIRED_REGIONS}"
        "--env" "DEST_IP_PORTS=${DEST_IP_PORTS}"
        "--env" "SRC_BIND_PORT=${SRC_BIND_PORT}"
        "--volume" "${AUTH_KEYPAIR}:${container_auth_path}:ro"
        "${DOCKER_IMAGE}"
        "${PROXY_SUBCOMMAND}"
    )
fi

if [[ -n "${EXTRA_ARGS}" ]]; then
    read -r -a extra <<<"${EXTRA_ARGS}"
    docker_args+=("${extra[@]}")
fi

exec "${DOCKER_BIN}" "${docker_args[@]}"
