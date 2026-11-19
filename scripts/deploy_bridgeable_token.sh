#!/usr/bin/env bash
#
# Deploy an ERC20 token on a fendermint subnet and register it for cross-subnet bridging.
#
# Usage:
#   bash scripts/deploy_bridgeable_token.sh \
#     --rpc-url http://localhost:8545 \
#     --private-key 0xabcd... \
#     --gateway 0x77aa... \
#     --name "MyToken" --symbol "MTK" --decimals 18 --initial-supply 1000000 \
#     --broadcast
#
set -euo pipefail

# ── Parse arguments ──

NAME=""
SYMBOL=""
DECIMALS=""
INITIAL_SUPPLY=""
BROADCAST=""
RPC_URL="${RPC_URL:-}"
PRIVATE_KEY="${PRIVATE_KEY:-}"
GATEWAY_ADDRESS="${GATEWAY_ADDRESS:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rpc-url)        RPC_URL="$2";        shift 2 ;;
        --private-key)    PRIVATE_KEY="$2";    shift 2 ;;
        --gateway)        GATEWAY_ADDRESS="$2"; shift 2 ;;
        --name)           NAME="$2";           shift 2 ;;
        --symbol)         SYMBOL="$2";         shift 2 ;;
        --decimals)       DECIMALS="$2";       shift 2 ;;
        --initial-supply) INITIAL_SUPPLY="$2"; shift 2 ;;
        --broadcast)      BROADCAST="--broadcast"; shift ;;
        *)
            echo "error: unknown argument '$1'"
            echo "usage: $0 --rpc-url <url> --private-key <key> --gateway <addr> --name <name> --symbol <symbol> --decimals <decimals> --initial-supply <amount> [--broadcast]"
            exit 1
            ;;
    esac
done

# ── Check required params ──

missing=()
[[ -z "$RPC_URL" ]]         && missing+=("--rpc-url")
[[ -z "$PRIVATE_KEY" ]]     && missing+=("--private-key")
[[ -z "$GATEWAY_ADDRESS" ]] && missing+=("--gateway")
[[ -z "$NAME" ]]            && missing+=("--name")
[[ -z "$SYMBOL" ]]          && missing+=("--symbol")
[[ -z "$DECIMALS" ]]        && missing+=("--decimals")
[[ -z "$INITIAL_SUPPLY" ]]  && missing+=("--initial-supply")

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "error: missing required arguments: ${missing[*]}"
    echo "usage: $0 --rpc-url <url> --private-key <key> --gateway <addr> --name <name> --symbol <symbol> --decimals <decimals> --initial-supply <amount> [--broadcast]"
    exit 1
fi

# ── Check tools ──

if ! command -v forge &>/dev/null; then
    echo "error: 'forge' (Foundry) is not installed or not in PATH"
    exit 1
fi

if ! command -v cast &>/dev/null; then
    echo "error: 'cast' (Foundry) is not installed or not in PATH"
    exit 1
fi

# A validator can submit checkpoint signatures between nonce lookup and broadcast.
send_with_nonce_retry() {
    local tool="$1" action="$2"; shift 2
    local nonce output error error_file attempt
    error_file=$(mktemp) || return 1
    for attempt in 1 2 3 4 5; do
        if ! nonce=$(cast nonce "$DEPLOYER" --block pending --rpc-url "$RPC_URL" --rpc-timeout 5) \
            || [[ ! "$nonce" =~ ^(0[xX][0-9a-fA-F]+|[0-9]+)$ ]]; then
            echo "error: cannot read a valid pending nonce for $DEPLOYER" >&2
            rm -f "$error_file"
            return 1
        fi
        if output=$("$tool" "$action" --nonce "$nonce" "$@" 2>"$error_file"); then
            cat "$error_file" >&2
            rm -f "$error_file"
            printf '%s\n' "$output"
            return 0
        fi
        error=$(cat "$error_file")
        if [[ "$error" =~ expected[[:space:]]sequence[[:space:]]([0-9]+),[[:space:]]got[[:space:]]([0-9]+) ]] \
            && (( BASH_REMATCH[2] == nonce && BASH_REMATCH[1] != nonce && attempt < 5 )); then
            echo "Nonce $nonce was rejected; refreshing the pending nonce (attempt $((attempt + 1))/5)." >&2
            sleep 1
            continue
        fi
        printf '%s\n' "$error" "$output" >&2
        rm -f "$error_file"
        return 1
    done
}

# Wait for execution and for the nonce source used by the next transaction to catch up.
wait_for_transaction() {
    local tx_hash="$1"
    local receipt_status tx_nonce observed_nonce block_tag deadline

    receipt_status=$(cast receipt "$tx_hash" status --rpc-url "$RPC_URL" --confirmations 1) || return 1
    if [[ ! "$receipt_status" =~ ^[[:space:]]*(true|1|0[xX]0*1)([[:space:]]+\(success\))?[[:space:]]*$ ]]; then
        echo "error: transaction $tx_hash failed (status $receipt_status)" >&2
        return 1
    fi
    tx_nonce=$(cast tx "$tx_hash" nonce --rpc-url "$RPC_URL") || return 1
    if [[ ! "$tx_nonce" =~ ^(0[xX][0-9a-fA-F]+|[0-9]+)$ ]]; then
        echo "error: invalid nonce for transaction $tx_hash: $tx_nonce" >&2
        return 1
    fi
    deadline=$((SECONDS + 90))
    for block_tag in latest pending; do
        observed_nonce="unavailable"
        while true; do
            if observed_nonce=$(cast nonce "$DEPLOYER" --block "$block_tag" --rpc-url "$RPC_URL" --rpc-timeout 5 2>/dev/null) \
                && [[ "$observed_nonce" =~ ^(0[xX][0-9a-fA-F]+|[0-9]+)$ ]] \
                && (( observed_nonce > tx_nonce )); then
                break
            fi
            if (( SECONDS >= deadline )); then
                echo "error: $block_tag nonce after transaction $tx_hash did not advance past $tx_nonce (got $observed_nonce)" >&2
                return 1
            fi
            sleep 1
        done
    done
}

# ── Derive deployer address and check balance ──

DEPLOYER=$(cast wallet address --private-key "$PRIVATE_KEY" 2>/dev/null) || {
    echo "error: invalid PRIVATE_KEY"
    exit 1
}
echo "Deployer address: $DEPLOYER"

BALANCE=$(cast balance "$DEPLOYER" --rpc-url "$RPC_URL" 2>/dev/null) || {
    echo "error: cannot connect to RPC_URL=$RPC_URL"
    exit 1
}
if [[ "$BALANCE" == "0" ]]; then
    echo "warning: deployer balance is 0 — transactions will fail without native tokens for gas"
fi

# ── Find the contract source ──

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONTRACT_DIR="$REPO_ROOT/contracts"
CONTRACT_PATH="contracts/examples/BridgeableToken.sol"

if [[ ! -f "$CONTRACT_DIR/$CONTRACT_PATH" ]]; then
    echo "error: contract not found at $CONTRACT_DIR/$CONTRACT_PATH"
    exit 1
fi

# ── Deploy the token ──

echo ""
echo "Deploying BridgeableToken: name=$NAME symbol=$SYMBOL decimals=$DECIMALS initialSupply=$INITIAL_SUPPLY (smallest units) broadcast=$BROADCAST"

FORGE_CMD=(forge create "$CONTRACT_PATH:BridgeableToken")
if [[ -n "$BROADCAST" ]]; then
    FORGE_CMD+=(--broadcast)
fi
FORGE_CMD+=(
    --rpc-url "$RPC_URL"
    --private-key "$PRIVATE_KEY"
    --constructor-args "$NAME" "$SYMBOL" "$DECIMALS" "$INITIAL_SUPPLY" "$DEPLOYER"
)

DEPLOY_OUTPUT=$(
    cd "$CONTRACT_DIR" || exit 1
    if [[ -n "$BROADCAST" ]]; then
        send_with_nonce_retry "${FORGE_CMD[@]}"
    else
        "${FORGE_CMD[@]}"
    fi
) || {
    echo "error: deployment failed"
    echo "$DEPLOY_OUTPUT"
    exit 1
}

# Extract deployed address from forge output
TOKEN_ADDRESS=$(echo "$DEPLOY_OUTPUT" | grep -i "Deployed to:" | awk '{print $NF}')
if [[ -z "$TOKEN_ADDRESS" ]]; then
    echo "error: could not parse deployed address from forge output:"
    echo "$DEPLOY_OUTPUT"
    exit 1
fi

echo "Token deployed at: $TOKEN_ADDRESS"

if [[ -z "$BROADCAST" ]]; then
    echo ""
    echo "Dry run complete. To actually deploy and register, add --broadcast."
    exit 0
fi

# ── Register the token on the Gateway ──

# Wait for the deploy transaction before fetching the registration nonce.
DEPLOY_TX_HASH=$(echo "$DEPLOY_OUTPUT" | awk 'tolower($0) ~ /transaction hash:/ {print $NF}')
if [[ ! "$DEPLOY_TX_HASH" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
    echo "error: could not parse deployment transaction hash" >&2
    exit 1
fi
wait_for_transaction "$DEPLOY_TX_HASH"

echo ""
echo "Registering token on gateway $GATEWAY_ADDRESS ..."

REGISTER_TX_HASH=$(
    send_with_nonce_retry cast send "$GATEWAY_ADDRESS" \
        "registerBridgeableToken(address)" \
        "$TOKEN_ADDRESS" \
        --rpc-url "$RPC_URL" \
        --private-key "$PRIVATE_KEY" \
        --async
) || {
    echo "error: registration failed"
    echo "$REGISTER_TX_HASH"
    exit 1
}
if [[ ! "$REGISTER_TX_HASH" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
    echo "error: could not parse registration transaction hash" >&2
    exit 1
fi
wait_for_transaction "$REGISTER_TX_HASH"

echo "Token registered for cross-subnet bridging."
echo ""
echo "Summary:"
echo "  Token address:   $TOKEN_ADDRESS"
echo "  Name:            $NAME"
echo "  Symbol:          $SYMBOL"
echo "  Decimals:        $DECIMALS"
echo "  Initial supply:  $INITIAL_SUPPLY (smallest units)"
echo "  Owner:           $DEPLOYER"
echo "  Gateway:         $GATEWAY_ADDRESS"
