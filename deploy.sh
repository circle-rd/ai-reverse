#!/usr/bin/env bash
# RAIVERSED tunnel deployment.
#
# Installs and starts the reverse-agent next to one local OpenAI-compatible inference
# server. The agent dials out only; it publishes no port.
set -euo pipefail
umask 077
IFS=$'\n\t'

readonly SCRIPT_NAME="deploy.sh"
readonly AGENT_IMAGE="ghcr.io/circle-rd/ai-reverse-agent:${RAIVERSED_IMAGE_TAG:-latest}"
readonly GPU_IMAGE="ghcr.io/circle-rd/ai-reverse-agent-gpu:${RAIVERSED_IMAGE_TAG:-latest}"
readonly DEFAULT_CLIENT_ID="reverse-agent"
readonly ENROLL_TIMEOUT="${RAIVERSED_ENROLL_TIMEOUT:-120}"
readonly APPROVAL_TIMEOUT="${RAIVERSED_APPROVAL_TIMEOUT:-300}"
readonly MIN_DOCKER="20.10.0"
readonly MIN_COMPOSE="2.20.0"

AUTO=0
MODEL=""
UPSTREAM=""
RELAY=""
ISSUER=""
CLIENT_ID=""
INSTALL_DIR=""
IMAGE=""
ENGINE_KEY_FILE=""
FORCE_GPU=0
NO_GPU=0
INSECURE=0
DRY_RUN=0
RECONFIGURE=0
SKIP_PULL=0
PROMPT_ANSWER=""
SELECTED_PORT=""
SELECTED_PROCESS=""
SELECTED_URL=""
SELECTED_AUTH=0
LOG_START=""

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    CYAN=$'\033[36m'
    GREEN=$'\033[32m'
    YELLOW=$'\033[33m'
    RED=$'\033[31m'
    DIM=$'\033[2m'
    RESET=$'\033[0m'
else
    CYAN=""
    GREEN=""
    YELLOW=""
    RED=""
    DIM=""
    RESET=""
fi

usage() {
    cat <<EOF
$SCRIPT_NAME - deploy the RAIVERSED reverse-agent next to an inference server

Usage: $SCRIPT_NAME [options]

  --auto                 non-interactive: print only the enrolment URL on stdout
  --model NAME           select the server that serves NAME (with --auto)
  --upstream URL         use this engine instead of discovering one
  --relay URL            relay origin (default: prompt, or the existing .env)
  --issuer URL           identity-provider origin (default: prompt, or existing)
  --client-id ID         OAuth device-flow client id (default: $DEFAULT_CLIENT_ID)
  --install-dir DIR      deployment directory (default: /opt/reverse-agent as root)
  --image IMAGE          override the container image
  --engine-key-file FILE engine bearer key, stored as a Docker secret
  --gpu / --no-gpu       force or disable the GPU image and device access
  --insecure             allow plain HTTP to a non-loopback relay or issuer
  --reconfigure          rediscover the engine even if a deployment exists
  --no-pull              use a local image without pulling it
  --dry-run              show the detected server and generated files, change nothing
  -h, --help             this help

Environment: AGENT_UPSTREAM_API_KEY (engine key), RAIVERSED_IMAGE_TAG,
RAIVERSED_ENROLL_TIMEOUT, RAIVERSED_APPROVAL_TIMEOUT.
EOF
}

say() { [ "$AUTO" -eq 1 ] || printf '%s\n' "$*"; }
step() { [ "$AUTO" -eq 1 ] || printf '\n%s*%s %s\n' "$CYAN" "$RESET" "$*"; }
warn() { [ "$AUTO" -eq 1 ] || printf '%swarning:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
    printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}

cleanup() {
    [ -n "${WORK_DIR:-}" ] && rm -rf "$WORK_DIR"
}
trap cleanup EXIT

while [ $# -gt 0 ]; do
    case "$1" in
        --auto) AUTO=1 ;;
        --model) MODEL="${2:?--model needs a value}"; shift ;;
        --upstream) UPSTREAM="${2:?--upstream needs a value}"; shift ;;
        --relay) RELAY="${2:?--relay needs a value}"; shift ;;
        --issuer) ISSUER="${2:?--issuer needs a value}"; shift ;;
        --client-id) CLIENT_ID="${2:?--client-id needs a value}"; shift ;;
        --install-dir) INSTALL_DIR="${2:?--install-dir needs a value}"; shift ;;
        --image) IMAGE="${2:?--image needs a value}"; shift ;;
        --engine-key-file) ENGINE_KEY_FILE="${2:?--engine-key-file needs a value}"; shift ;;
        --gpu) FORCE_GPU=1 ;;
        --no-gpu) NO_GPU=1 ;;
        --insecure) INSECURE=1 ;;
        --reconfigure) RECONFIGURE=1 ;;
        --no-pull) SKIP_PULL=1 ;;
        --dry-run) DRY_RUN=1 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done
[ "$FORCE_GPU" -eq 1 ] && [ "$NO_GPU" -eq 1 ] && die "--gpu and --no-gpu are exclusive"

banner() {
    [ "$AUTO" -eq 1 ] && return
    printf '\n%s' "$CYAN"
    cat <<'EOF'
  ####   ###  ##### #   # ##### ####  ##### ##### ####
  #   # #   #   #   #   # #     #   # #     #     #   #
  ####  #####   #   #   # ####  ####  ####  ####  #   #
  #  #  #   #   #    # #  #     #  #      # #     #   #
  #   # #   # #####   #   ##### #   # ##### ##### ####
EOF
    printf '%s' "$RESET"
    printf '%s  PRIVATE P2P AI PROVIDER - OFFICIAL TUNNEL%s\n' "$DIM" "$RESET"
}

prompt_value() {
    label=$1
    default=$2
    if [ -n "$default" ]; then
        printf '%s [%s]: ' "$label" "$default"
    else
        printf '%s: ' "$label"
    fi
    if [ -r /dev/tty ]; then
        IFS= read -r PROMPT_ANSWER < /dev/tty
    else
        IFS= read -r PROMPT_ANSWER
    fi
    [ -z "$PROMPT_ANSWER" ] && PROMPT_ANSWER=$default
}

version_ge() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

check_prereqs() {
    step "Checking prerequisites"
    [ "$(uname -s)" = "Linux" ] || die "a Linux host is required"
    case "$(uname -m)" in
        x86_64|amd64|aarch64|arm64) ;;
        *) die "unsupported architecture: $(uname -m) (images target amd64 and arm64)" ;;
    esac

    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        case " ${ID:-} ${ID_LIKE:-} " in
            *" ubuntu "*|*" debian "*) ;;
            *) warn "tested on Ubuntu and Debian; continuing on ${PRETTY_NAME:-this Linux}" ;;
        esac
    fi

    for tool in docker curl ss sort sed grep awk timeout; do
        command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
    done
    docker compose version >/dev/null 2>&1 \
        || die "Docker Compose v2 is required (docker compose)"
    timeout 15 docker info >/dev/null 2>&1 \
        || die "cannot reach the Docker daemon (start Docker or join the docker group)"
    say "  Docker $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unknown), Compose $(docker compose version --short 2>/dev/null || echo unknown)"

    docker_version=$(docker version --format '{{.Server.Version}}' 2>/dev/null | sed 's/+.*//')
    compose_version=$(docker compose version --short 2>/dev/null | sed -e 's/^v//' -e 's/+.*//')
    version_ge "$docker_version" "$MIN_DOCKER" \
        || die "Docker Engine $MIN_DOCKER or newer is required (found $docker_version)"
    version_ge "$compose_version" "$MIN_COMPOSE" \
        || die "Docker Compose $MIN_COMPOSE or newer is required (found $compose_version)"
}

env_value() {
    [ -f "$INSTALL_DIR/.env" ] || return 0
    sed -n "s/^$1=//p" "$INSTALL_DIR/.env" | tail -n1 | sed -e 's/^"//' -e 's/"$//'
}

resolve_install_dir() {
    if [ -z "$INSTALL_DIR" ]; then
        if [ "$(id -u)" -eq 0 ]; then
            INSTALL_DIR=/opt/reverse-agent
        else
            INSTALL_DIR="$HOME/.reverse-agent"
        fi
    fi
    if [ "$DRY_RUN" -eq 1 ]; then
        INSTALL_DIR=$(printf '%s' "$INSTALL_DIR" | sed 's#/*$##')
        return
    fi
    mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
    INSTALL_DIR=$(cd "$INSTALL_DIR" && pwd)
    mkdir -p "$INSTALL_DIR/secrets"
    chmod 700 "$INSTALL_DIR/secrets" 2>/dev/null || true
}

normalize_url() {
    printf '%s' "${1%/}"
}

url_origin() {
    printf '%s' "$1" | sed -E 's#^([a-zA-Z][a-zA-Z0-9+.-]*://[^/]+).*#\1#'
}

validate_url() {
    key=$1
    value=$2
    case "$value" in
        https://?*) ;;
        http://?*)
            case "$value" in
                http://127.*|http://localhost*|http://\[::1\]*) ;;
                *) [ "$INSECURE" -eq 1 ] || die "$key uses plain HTTP to a non-loopback host; pass --insecure to allow it" ;;
            esac
            ;;
        *) die "$key must be an http(s) origin (got '$value')" ;;
    esac
}

discover_servers() {
    step "Discovering local inference servers"
    [ "$(id -u)" -eq 0 ] || warn "process names require root; run with sudo to see them"
    : >"$WORK_DIR/candidates"
    ss -H -ltnp 2>/dev/null | while IFS=' ' read -r _state _recvq _sendq local_addr _peer proc; do
        [ -n "${local_addr:-}" ] || continue
        port=${local_addr##*:}
        case "$port" in ''|*[!0-9]*) continue ;; esac
        case "$local_addr" in
            127.*|0.0.0.0:*|'*:'*|'[::1]':*|'[::]':*|':::'*) ;;
            *) continue ;;
        esac
        case "$local_addr" in
            '[::1]':*) host='[::1]' ;;
            *) host=127.0.0.1 ;;
        esac
        grep -qx "$port" "$WORK_DIR/ports" 2>/dev/null && continue
        printf '%s\n' "$port" >>"$WORK_DIR/ports"

        name=$(printf '%s' "$proc" | sed -n 's/.*users:(("\([^"]*\)".*/\1/p')
        pid=$(printf '%s' "$proc" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')
        if [ -z "$name" ] && [ -n "$pid" ]; then
            name=$(tr -d '\n' <"/proc/$pid/comm" 2>/dev/null || true)
        fi
        [ -n "$name" ] || name="port $port"

        auth_args=()
        if [ -n "$ENGINE_KEY" ]; then
            printf 'header = "Authorization: Bearer %s"\n' "$ENGINE_KEY" >"$WORK_DIR/curlrc"
            chmod 600 "$WORK_DIR/curlrc"
            auth_args=(--config "$WORK_DIR/curlrc")
        fi
        body="$WORK_DIR/body.$port"
        status=$(curl -sS -o "$body" -w '%{http_code}' --max-time 2 \
            "${auth_args[@]}" "http://$host:$port/v1/models" 2>/dev/null || printf '000')
        case "$status" in
            200)
                grep -Eq '"object"[[:space:]]*:|"data"[[:space:]]*\[|"models"[[:space:]]*\[' "$body" 2>/dev/null || continue
                models=$(extract_models "$body")
                [ -n "$models" ] || continue
                printf '%s\t%s\t%s\t%s\n' "$port" "$name" "$models" "0" >>"$WORK_DIR/candidates"
                ;;
        esac
    done
    if [ ! -s "$WORK_DIR/candidates" ]; then
        die "no OpenAI-compatible server found on a local TCP port; start one or pass --upstream"
    fi
}

extract_models() {
    {
        grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' "$1" 2>/dev/null || true
        grep -o '"name"[[:space:]]*:[[:space:]]*"[^"]*"' "$1" 2>/dev/null || true
    } | sed -e 's/.*:[[:space:]]*"//' -e 's/"$//' | awk 'NF && !seen[$0]++' | tr '\n' ',' \
        | sed -e 's/,$//' -e 's/,/, /g'
}

candidate_field() {
    awk -F'\t' -v n="$1" -v f="$2" 'NR==n {print $f}' "$WORK_DIR/candidates"
}

choose_server() {
    if [ -n "$UPSTREAM" ]; then
        SELECTED_URL=$(normalize_url "$UPSTREAM")
        SELECTED_PORT=$(printf '%s' "$SELECTED_URL" | sed -n 's/.*:\([0-9][0-9]*\)$/\1/p')
        SELECTED_PROCESS="${SELECTED_PORT:+port $SELECTED_PORT}"
        return
    fi
    discover_servers
    count=$(wc -l <"$WORK_DIR/candidates" | tr -d ' ')

    if [ -n "$MODEL" ]; then
        awk -F'\t' -v m="$MODEL" '
            { n=split($3, a, ", "); for (i=1; i<=n; i++) if (a[i]==m) { print; break } }
        ' "$WORK_DIR/candidates" >"$WORK_DIR/matches"
        matches=$(wc -l <"$WORK_DIR/matches" | tr -d ' ')
        [ "$matches" -ge 1 ] || die "no local server serves model '$MODEL'"
        if [ "$matches" -gt 1 ]; then
            [ "$AUTO" -eq 1 ] && die "several local servers serve '$MODEL'; pass --upstream"
            cp "$WORK_DIR/matches" "$WORK_DIR/candidates"
            count=$matches
        else
            cp "$WORK_DIR/matches" "$WORK_DIR/candidates"
            count=1
        fi
    fi

    if [ "$count" -eq 1 ]; then
        select_candidate 1
        say "  Using ${GREEN}${SELECTED_PROCESS}${RESET} on ${SELECTED_URL}"
        return
    fi

    if [ "$AUTO" -eq 1 ]; then
        die "several inference servers found; pass --model or --upstream"
    fi

    printf '\n  %-3s %-16s %-24s %s\n' "#" "PROCESS" "ADDRESS" "MODEL(S)"
    n=1
    while IFS=$'\t' read -r port name models _; do
        printf '  %-3s %-16s %-24s %s\n' "$n" "${name:0:16}" "127.0.0.1:$port" "$models"
        n=$((n + 1))
    done <"$WORK_DIR/candidates"
    printf '\nSelect the server to expose [1-%s]: ' "$count"
    if [ -r /dev/tty ]; then
        IFS= read -r PROMPT_ANSWER < /dev/tty
    else
        IFS= read -r PROMPT_ANSWER
    fi
    case "$PROMPT_ANSWER" in
        ''|*[!0-9]*) die "invalid selection" ;;
    esac
    [ "$PROMPT_ANSWER" -ge 1 ] && [ "$PROMPT_ANSWER" -le "$count" ] || die "invalid selection"
    select_candidate "$PROMPT_ANSWER"
    say "  Using ${GREEN}${SELECTED_PROCESS}${RESET} on ${SELECTED_URL}"
}

select_candidate() {
    SELECTED_PORT=$(candidate_field "$1" 1)
    SELECTED_PROCESS=$(candidate_field "$1" 2)
    SELECTED_AUTH=$(candidate_field "$1" 4)
    SELECTED_URL="http://127.0.0.1:$SELECTED_PORT"
    if [ "$SELECTED_AUTH" = "1" ]; then
        warn "the engine requires an API key; pass --engine-key-file if it is not already stored"
    fi
    return 0
}

resolve_settings() {
    step "Configuring the tunnel"
    [ -n "$RELAY" ] || RELAY=$(env_value AGENT_RELAY_URL)
    [ -n "$ISSUER" ] || ISSUER=$(env_value AGENT_AUTH_ISSUER)
    [ -n "$CLIENT_ID" ] || CLIENT_ID=$(env_value AGENT_AUTH_CLIENT_ID)
    [ -n "$CLIENT_ID" ] || CLIENT_ID=$DEFAULT_CLIENT_ID
    if [ -z "$RELAY" ]; then
        [ "$AUTO" -eq 1 ] && die "AGENT_RELAY_URL is required (pass --relay)"
        prompt_value "Relay URL" ""
        RELAY=$PROMPT_ANSWER
    fi
    if [ -z "$ISSUER" ]; then
        [ "$AUTO" -eq 1 ] && die "AGENT_AUTH_ISSUER is required (pass --issuer)"
        prompt_value "Identity provider URL" ""
        ISSUER=$PROMPT_ANSWER
    fi
    RELAY=$(normalize_url "$RELAY")
    ISSUER=$(normalize_url "$ISSUER")
    validate_url "AGENT_RELAY_URL" "$RELAY"
    validate_url "AGENT_AUTH_ISSUER" "$ISSUER"
    RESOURCE=$(env_value AGENT_AUTH_RESOURCE)
    [ -n "$RESOURCE" ] || RESOURCE=$(url_origin "$RELAY")
    SCOPE=$(env_value AGENT_AUTH_SCOPE)
    [ -n "$SCOPE" ] || SCOPE="openid profile offline_access tunnel:serve"
}

load_engine_key() {
    SECRET=0
    ENGINE_KEY=""
    if [ -n "$ENGINE_KEY_FILE" ]; then
        [ -r "$ENGINE_KEY_FILE" ] || die "cannot read $ENGINE_KEY_FILE"
        ENGINE_KEY=$(cat "$ENGINE_KEY_FILE")
        SECRET=1
    elif [ -n "${AGENT_UPSTREAM_API_KEY:-}" ]; then
        ENGINE_KEY=$AGENT_UPSTREAM_API_KEY
        SECRET=1
    elif [ -s "$INSTALL_DIR/secrets/upstream_api_key" ]; then
        ENGINE_KEY=$(cat "$INSTALL_DIR/secrets/upstream_api_key")
        SECRET=1
    elif [ -n "$(env_value AGENT_UPSTREAM_API_KEY)" ]; then
        ENGINE_KEY=$(env_value AGENT_UPSTREAM_API_KEY)
    fi
    if [ -n "$ENGINE_KEY" ]; then
        case "$ENGINE_KEY" in
            *$'\n'*|*'"'*) die "the engine key must be a single line without quotes" ;;
        esac
    fi
}

detect_gpu() {
    GPU=none
    if [ "$NO_GPU" -eq 1 ]; then
        return
    fi
    if [ "$FORCE_GPU" -eq 1 ] || { command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L 2>/dev/null | grep -q '^GPU'; }; then
        GPU=nvidia
        docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -qi nvidia \
            || die "NVIDIA GPU found but the NVIDIA Container Toolkit is not configured for Docker; install nvidia-container-toolkit, run 'nvidia-ctk runtime configure --runtime=docker', restart Docker, or pass --no-gpu"
        return
    fi
    if [ -e /dev/dri/renderD128 ] || [ -e /dev/dri/card0 ]; then
        GPU=dri
    fi
}

write_config() {
    step "Writing configuration"
    [ -n "$IMAGE" ] || {
        if [ "$GPU" = "none" ]; then IMAGE=$AGENT_IMAGE; else IMAGE=$GPU_IMAGE; fi
    }
    HOSTNAME_VALUE=$(env_value AGENT_HOSTNAME)
    [ -n "$HOSTNAME_VALUE" ] || HOSTNAME_VALUE=$(hostname -s 2>/dev/null || hostname 2>/dev/null || true)
    case "$HOSTNAME_VALUE" in
        ''|[0-9]*|[0-9]*.*|*[!A-Za-z0-9-]*) HOSTNAME_VALUE="" ;;
    esac
    if [ "${#HOSTNAME_VALUE}" -gt 63 ] \
        || ! printf '%s' "$HOSTNAME_VALUE" | grep -Eq '^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$' \
        || ! printf '%s' "$HOSTNAME_VALUE" | grep -q '[A-Za-z]'; then
        HOSTNAME_VALUE=""
    fi

    MACHINE_MOUNT=0
    if [ -s /etc/machine-id ] && [ "$(tr -d '\n' </etc/machine-id)" != "00000000000000000000000000000000" ]; then
        MACHINE_MOUNT=1
    fi

    {
        printf 'AGENT_RELAY_URL=%s\n' "$RELAY"
        printf 'AGENT_UPSTREAM_URL=%s\n' "$SELECTED_URL"
        printf 'AGENT_AUTH_ISSUER=%s\n' "$ISSUER"
        printf 'AGENT_AUTH_CLIENT_ID=%s\n' "$CLIENT_ID"
        printf 'AGENT_AUTH_RESOURCE=%s\n' "$RESOURCE"
        printf 'AGENT_AUTH_SCOPE=%s\n' "$SCOPE"
        [ -n "$HOSTNAME_VALUE" ] && printf 'AGENT_HOSTNAME=%s\n' "$HOSTNAME_VALUE"
        [ "$MACHINE_MOUNT" -eq 1 ] && printf 'AGENT_MACHINE_ID_FILE=/run/host-machine-id\n'
        printf 'AGENT_CREDENTIAL_FILE=/var/lib/reverse-agent/credential\n'
        [ "$SECRET" -eq 1 ] && printf 'AGENT_UPSTREAM_API_KEY_FILE=/run/secrets/upstream_api_key\n'
        [ "$INSECURE" -eq 1 ] && printf 'AGENT_INSECURE_TRANSPORT=true\n'
        LOG_LEVEL=$(env_value RUST_LOG)
        printf 'RUST_LOG=%s\n' "${LOG_LEVEL:-info}"
    } >"$WORK_DIR/env"
    MANAGED="AGENT_RELAY_URL AGENT_UPSTREAM_URL AGENT_AUTH_ISSUER AGENT_AUTH_CLIENT_ID AGENT_AUTH_RESOURCE AGENT_AUTH_SCOPE AGENT_HOSTNAME AGENT_MACHINE_ID_FILE AGENT_CREDENTIAL_FILE AGENT_UPSTREAM_API_KEY_FILE AGENT_INSECURE_TRANSPORT RUST_LOG"
    [ "$SECRET" -eq 1 ] && MANAGED="$MANAGED AGENT_UPSTREAM_API_KEY"
    if [ -f "$INSTALL_DIR/.env" ]; then
        while IFS= read -r line; do
            key=${line%%=*}
            case " $MANAGED " in
                *" $key "*) ;;
                *) printf '%s\n' "$line" >>"$WORK_DIR/env" ;;
            esac
        done <"$INSTALL_DIR/.env"
    fi

    {
        printf 'name: reverse-agent\n'
        printf 'services:\n'
        printf '  reverse-agent:\n'
        printf '    image: %s\n' "$IMAGE"
        printf '    container_name: reverse-agent\n'
        printf '    restart: unless-stopped\n'
        printf '    network_mode: host\n'
        printf '    read_only: true\n'
        printf '    cap_drop: [ALL]\n'
        printf '    security_opt: ["no-new-privileges:true"]\n'
        printf '    ulimits:\n      core: 0\n'
        printf '    stop_grace_period: 10s\n'
        printf '    logging:\n      driver: json-file\n      options:\n        max-size: "10m"\n        max-file: "5"\n'
        printf '    env_file:\n      - .env\n'
        printf '    volumes:\n      - agent-state:/var/lib/reverse-agent\n'
        [ "$MACHINE_MOUNT" -eq 1 ] && printf '      - /etc/machine-id:/run/host-machine-id:ro\n'
        [ "$SECRET" -eq 1 ] && printf '    secrets:\n      - upstream_api_key\n'
        case "$GPU" in
            nvidia)
                printf '    pid: host\n'
                printf '    deploy:\n      resources:\n        reservations:\n          devices:\n            - driver: nvidia\n              count: all\n              capabilities: [gpu]\n'
                ;;
            dri)
                gid=$(stat -c '%g' /dev/dri/renderD128 2>/dev/null || stat -c '%g' /dev/dri/card0 2>/dev/null || echo 0)
                printf '    pid: host\n'
                printf '    devices:\n      - /dev/dri:/dev/dri\n'
                printf '    group_add:\n      - "%s"\n' "$gid"
                ;;
        esac
        printf 'volumes:\n  agent-state:\n'
        [ "$SECRET" -eq 1 ] && printf 'secrets:\n  upstream_api_key:\n    file: ./secrets/upstream_api_key\n'
    } >"$WORK_DIR/compose.yml"

    if [ "$DRY_RUN" -eq 1 ]; then
        printf '\n%s--- %s/compose.yml ---%s\n' "$DIM" "$INSTALL_DIR" "$RESET"
        cat "$WORK_DIR/compose.yml"
        printf '\n%s--- %s/.env (secrets omitted) ---%s\n' "$DIM" "$INSTALL_DIR" "$RESET"
        sed 's/^AGENT_UPSTREAM_API_KEY_FILE=.*/AGENT_UPSTREAM_API_KEY_FILE=<secret>/' "$WORK_DIR/env"
        exit 0
    fi

    if [ -n "$ENGINE_KEY" ]; then
        if [ -n "$ENGINE_KEY_FILE" ]; then
            install -m 600 "$ENGINE_KEY_FILE" "$INSTALL_DIR/secrets/upstream_api_key"
        elif [ -n "${AGENT_UPSTREAM_API_KEY:-}" ]; then
            printf '%s' "$AGENT_UPSTREAM_API_KEY" >"$INSTALL_DIR/secrets/upstream_api_key"
            chmod 600 "$INSTALL_DIR/secrets/upstream_api_key"
        fi
    fi
    install -m 600 "$WORK_DIR/env" "$INSTALL_DIR/.env"
    install -m 644 "$WORK_DIR/compose.yml" "$INSTALL_DIR/compose.yml"
    say "  Deployment: $INSTALL_DIR"
    say "  Engine:     $SELECTED_URL"
    say "  Image:      $IMAGE"
}

run_compose() {
    if [ "$AUTO" -eq 1 ]; then
        (cd "$INSTALL_DIR" && "$@" >>"$LOG_FILE" 2>&1) \
            || die "docker compose failed; see $LOG_FILE"
    else
        (cd "$INSTALL_DIR" && "$@")
    fi
}

start_agent() {
    step "Starting the agent"
    LOG_FILE="$INSTALL_DIR/deploy.log"
    : >"$LOG_FILE"
    chmod 600 "$LOG_FILE"
    LOG_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    [ "$SKIP_PULL" -eq 1 ] || run_compose docker compose pull
    run_compose docker compose up -d --force-recreate
}

wait_for_enrolment() {
    step "Waiting for the enrolment link"
    deadline=$((SECONDS + ENROLL_TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        logs=$(docker logs --since "$LOG_START" reverse-agent 2>&1 || true)
        line=$(printf '%s\n' "$logs" | grep -F 'auth.waiting_for_approval' | tail -n1 || true)
        if [ -n "$line" ]; then
            ENROL_URL=$(printf '%s' "$line" | sed -n 's/.*"url":"\([^"]*\)".*/\1/p')
            ENROL_CODE=$(printf '%s' "$line" | sed -n 's/.*"code":"\([^"]*\)".*/\1/p')
            [ -n "$ENROL_URL" ] && return 0
        fi
        if printf '%s' "$logs" | grep -q 'tunnel.connected\|auth.enrolled'; then
            return 2
        fi
        if ! docker inspect -f '{{.State.Running}}' reverse-agent 2>/dev/null | grep -q true; then
            printf '%s\n' "$logs" | tail -n 20 >&2
            die "the agent exited before enrolment; see $LOG_FILE"
        fi
        sleep 1
    done
    printf '%s\n' "$logs" | tail -n 20 >&2
    die "no enrolment link after ${ENROLL_TIMEOUT}s; see $LOG_FILE"
}

wait_for_approval() {
    deadline=$((SECONDS + APPROVAL_TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if docker logs --since "$LOG_START" reverse-agent 2>&1 | grep -q 'tunnel.connected'; then
            return 0
        fi
        sleep 2
    done
    return 1
}

banner
check_prereqs
resolve_install_dir
WORK_DIR=$(mktemp -d)
trap cleanup EXIT
load_engine_key

if [ "$RECONFIGURE" -eq 0 ] && [ -z "$UPSTREAM" ] && [ -z "$MODEL" ]; then
    UPSTREAM=$(env_value AGENT_UPSTREAM_URL)
fi
choose_server
resolve_settings
detect_gpu
write_config

if [ "$AUTO" -eq 1 ]; then
    step "Starting the agent"
    LOG_FILE="$INSTALL_DIR/deploy.log"
    : >"$LOG_FILE"
    chmod 600 "$LOG_FILE"
    LOG_START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    [ "$SKIP_PULL" -eq 1 ] || run_compose docker compose pull
    run_compose docker compose up -d --force-recreate
    set +e
    wait_for_enrolment
    result=$?
    set -e
    case "$result" in
        0) printf '%s\n' "$ENROL_URL" ;;
        2) ;;
        *) exit 1 ;;
    esac
    exit 0
fi

start_agent
set +e
wait_for_enrolment
result=$?
set -e
case "$result" in
    0)
        printf '\n%sAuthorization required%s\n' "$CYAN" "$RESET"
        printf '  Open: %s\n' "$ENROL_URL"
        [ -n "$ENROL_CODE" ] && printf '  Code: %s\n' "$ENROL_CODE"
        printf '\nWaiting for approval...\n'
        if wait_for_approval; then
            printf '%sTunnel connected.%s\n' "$GREEN" "$RESET"
        else
            warn "approval is still pending; follow it with: docker logs -f reverse-agent"
        fi
        ;;
    2)
        printf '%sAgent already enrolled; tunnel is running.%s\n' "$GREEN" "$RESET"
        ;;
esac
printf '\nUseful commands:\n  docker logs -f reverse-agent\n  docker compose -f %s/compose.yml restart\n' "$INSTALL_DIR"
