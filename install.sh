#!/usr/bin/env bash
#
# ergo-node-rust — interactive setup
#
# Asks a handful of questions and writes a working ./conf.d/ in the current
# directory. Everything not asked here uses the binary's built-in defaults;
# see ergo.toml.example (shipped alongside this script) for the full annotated
# option set.
#
# ⚠ Network-dependent values — seed peers, listen port, API port — are NOT
# typed into this script. They are read from the shipped per-network defaults
# file, which is the single source for them.
#
# That is not tidiness. This script used to carry its own copy and it was
# WRONG: it wrote mainnet -> 9052 and testnet -> 9053, the values from before
# v0.6.10 inverted them, so anyone who ran it and accepted the default got a
# config pointing at the wrong API port for their network. The fix is not to
# correct the numbers here; it is for them not to be here.

set -euo pipefail

CONFD="./conf.d"

# Where the per-network defaults live: beside this script in a tarball, or
# under /usr/share on a .deb install.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for candidate in \
    "${SCRIPT_DIR}/deploy/defaults" \
    "${SCRIPT_DIR}/defaults" \
    "/usr/share/ergo-node-rust/defaults"
do
    if [[ -d "${candidate}" ]]; then
        DEFAULTS_DIR="${candidate}"
        break
    fi
done

if [[ -z "${DEFAULTS_DIR:-}" ]]; then
    echo "error: could not find the per-network defaults directory." >&2
    echo "  Looked in:" >&2
    echo "    ${SCRIPT_DIR}/deploy/defaults" >&2
    echo "    ${SCRIPT_DIR}/defaults" >&2
    echo "    /usr/share/ergo-node-rust/defaults" >&2
    echo "  Run this from the source tree, or install the .deb." >&2
    exit 1
fi

# ── Helpers ───────────────────────────────────────────────────────────
prompt() {
    # prompt VAR_NAME "Question" "default"
    local var="$1" question="$2" default="$3" reply=""
    read -r -p "${question} [${default}]: " reply
    printf -v "${var}" '%s' "${reply:-${default}}"
}

prompt_choice() {
    # prompt_choice VAR_NAME "Question" "default" "valid|values"
    local var="$1" question="$2" default="$3" valid="$4" reply=""
    while true; do
        read -r -p "${question} [${default}]: " reply
        reply="${reply:-${default}}"
        if [[ "|${valid}|" == *"|${reply}|"* ]]; then
            printf -v "${var}" '%s' "${reply}"
            return
        fi
        echo "  ↳ must be one of: ${valid//|/, }"
    done
}

want() {
    # want "Topic name" -> 0 if the operator wants to configure it
    local reply=""
    read -r -p "Configure $1? [y/N]: " reply
    [[ "${reply,,}" == y || "${reply,,}" == yes ]]
}

# ── Existing-config guard ─────────────────────────────────────────────
if [[ -e "${CONFD}" || -e "./ergo.toml" ]]; then
    echo "A config already exists here (${CONFD} and/or ./ergo.toml)."
    read -r -p "Overwrite the generated files? [y/N]: " reply
    case "${reply,,}" in
        y|yes) ;;
        *) echo "Aborted. Existing config left in place."; exit 0 ;;
    esac
fi

# ── Banner ────────────────────────────────────────────────────────────
cat <<'EOF'

ergo-node-rust — interactive setup
──────────────────────────────────
Press Enter to accept the bracketed default at each prompt.

Everything has a working default. You will be asked which topics you want
to change; skip them all and you still get a running node.

EOF

# ── 1. Network ────────────────────────────────────────────────────────
prompt_choice NETWORK "Network (mainnet/testnet)" "mainnet" "mainnet|testnet"

if [[ ! -f "${DEFAULTS_DIR}/${NETWORK}.toml" ]]; then
    echo "error: ${DEFAULTS_DIR}/${NETWORK}.toml is missing." >&2
    exit 1
fi

# ── Memory check ──────────────────────────────────────────────────────
#
# Advisory, and shown before the node-type question so a small box can pick
# light/digest in the same pass instead of finding out from a failed start.
# 4096 MB matches MEMORY_RECOMMENDED_BYTES in the node.
MEM_MB=""
if [[ -r /proc/meminfo ]]; then
    MEM_MB=$(awk '/^MemTotal:/ { print int($2 / 1024); exit }' /proc/meminfo || true)
fi
if [[ -n "${MEM_MB}" && "${MEM_MB}" -lt 4096 ]]; then
    cat <<EOF

⚠ This machine has ${MEM_MB} MB of RAM.

  A full (utxo) node wants 4 GB or more and REFUSES TO START below 3 GB —
  it holds the UTXO tree in memory, and cold sync is the demanding phase.

  Answer "y" to the node type question below and choose light or digest.
  Neither holds that tree, and both run comfortably here.

EOF
fi

# ── 2. Topics ─────────────────────────────────────────────────────────
echo
STATE_TYPE=""; BLOCKS_TO_KEEP=""; DATA_DIR=""; API_ADDRESS=""
LISTEN_PORT=""; MAX_INBOUND=""; FASTSYNC=""; MINER_PK=""
MEMORY_BUDGET_MB=""; SEED_PEERS_ADD=""

if want "node type (full/light/digest, and history retention)"; then
    prompt_choice STATE_TYPE "  State type (utxo/light/digest)" "utxo" "utxo|light|digest"
    if [[ "${STATE_TYPE}" == "utxo" ]]; then
        echo "  ↳ blocks_to_keep: -1 = full archival, 0 = at-tip only, N = retain last N"
        prompt BLOCKS_TO_KEEP "  Block history retention" "-1"
    fi
fi

if want "storage location"; then
    prompt DATA_DIR "  Data directory" "./ergo-node-data"
fi

if want "network interfaces (listen port, API address)"; then
    prompt LISTEN_PORT "  P2P listen port" "9030"
    echo "  ↳ leave blank for the network default (mainnet 9053, testnet 9052)"
    echo "  ↳ 0.0.0.0 exposes the API on every interface; prefer 127.0.0.1"
    prompt API_ADDRESS "  REST API bind address" ""
    prompt MAX_INBOUND "  Max inbound peers" "20"
fi

if want "bootstrap (fast initial sync)"; then
    prompt_choice FASTSYNC "  Use fast initial sync" "true" "true|false"
fi

if want "mining"; then
    echo "  ↳ the 33-byte compressed PUBLIC key, not a seed or mnemonic"
    prompt MINER_PK "  Miner public key (hex)" ""
fi

if want "memory budget"; then
    echo "  ↳ leave blank (recommended) and the node sizes itself from the"
    echo "    cgroup limit if there is one, else from total RAM"
    prompt MEMORY_BUDGET_MB "  Memory budget in MB" ""
fi

if want "additional seed peers"; then
    echo "  ↳ comma-separated host:port, ADDED to the shipped list"
    prompt SEED_PEERS_ADD "  Additional seed peers" ""
fi

# ── Write config ──────────────────────────────────────────────────────
#
# Two layers rather than one file, because the answers have to be able to
# override keys the defaults file already sets — and TOML has no way to state
# [listen.ipv6] twice in one document. conf.d/ is merged in lexical order, so
# 50- wins over 00-. Anything you add later in 99-local.toml wins over both.
mkdir -p "${CONFD}"
rm -f "${CONFD}/00-defaults.toml" "${CONFD}/50-local.toml"
cp "${DEFAULTS_DIR}/${NETWORK}.toml" "${CONFD}/00-defaults.toml"

{
    echo "# ergo-node-rust — generated by install.sh on $(date -u +"%Y-%m-%d %H:%M:%S UTC")"
    echo "#"
    echo "# Your answers. Merged on top of 00-defaults.toml."
    echo "# Re-running install.sh overwrites this file; put anything you want"
    echo "# kept in 99-local.toml, which nothing generated ever touches."
    echo ""
    echo "[node]"
    [[ -n "${DATA_DIR}" ]]          && echo "data_dir = \"${DATA_DIR}\""
    [[ -n "${STATE_TYPE}" ]]        && echo "state_type = \"${STATE_TYPE}\""
    [[ -n "${BLOCKS_TO_KEEP}" ]]    && echo "blocks_to_keep = ${BLOCKS_TO_KEEP}"
    [[ -n "${API_ADDRESS}" ]]       && echo "api_address = \"${API_ADDRESS}\""
    [[ -n "${FASTSYNC}" ]]          && echo "fastsync = ${FASTSYNC}"
    # Blank means "derive at runtime", so a blank must write nothing at all
    # rather than a zero — writing a number here switches derivation off.
    [[ -n "${MEMORY_BUDGET_MB}" ]]  && echo "memory_budget_mb = ${MEMORY_BUDGET_MB}"

    if [[ -n "${MINER_PK}" ]]; then
        echo ""
        echo "[node.mining]"
        echo "miner_pk = \"${MINER_PK}\""
    fi

    if [[ -n "${LISTEN_PORT}" || -n "${MAX_INBOUND}" ]]; then
        echo ""
        echo "[listen.ipv6]"
        [[ -n "${LISTEN_PORT}" ]] && echo "address = \"[::]:${LISTEN_PORT}\""
        [[ -n "${MAX_INBOUND}" ]] && echo "max_inbound = ${MAX_INBOUND}"
    fi

    # `_add` appends to the shipped list. A bare `seed_peers` would replace it
    # outright, which is not what "additional" asked for.
    if [[ -n "${SEED_PEERS_ADD}" ]]; then
        echo ""
        echo "[outbound]"
        printf 'seed_peers_add = ['
        IFS=',' read -ra _peers <<< "${SEED_PEERS_ADD}"
        for p in "${_peers[@]}"; do
            p="${p//[[:space:]]/}"
            [[ -n "${p}" ]] && printf '"%s", ' "${p}"
        done
        echo ']'
    fi
} > "${CONFD}/50-local.toml"

cat <<EOF

Wrote ${CONFD}/00-defaults.toml  (${NETWORK} defaults — replaced on re-run)
      ${CONFD}/50-local.toml     (your answers)

Both are merged in filename order. To override anything without your changes
being overwritten, create ${CONFD}/99-local.toml — nothing generated touches it.

Start the node with:  ./ergo-node-rust

EOF
