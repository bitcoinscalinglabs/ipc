#!/usr/bin/env bash
#
# Deploy an ERC20 token on a fendermint subnet and register it for cross-subnet bridging.
#
# Required environment variables:
#   RPC_URL          — subnet's Ethereum JSON-RPC endpoint (e.g. http://localhost:8545)
#   PRIVATE_KEY      — deployer's private key (hex, with or without 0x prefix)
#   GATEWAY_ADDRESS  — the subnet's GatewayDiamond contract address
#
# Usage:
#   bash scripts/deploy_bridgeable_token.sh \
#     --name "MyToken" --symbol "MTK" --decimals 18 --initial-supply 1000000
#
set -euo pipefail

# ── Parse arguments ──

NAME=""
SYMBOL=""
DECIMALS=""
INITIAL_SUPPLY=""
BROADCAST=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --name)       NAME="$2";           shift 2 ;;
        --symbol)     SYMBOL="$2";         shift 2 ;;
        --decimals)   DECIMALS="$2";       shift 2 ;;
        --initial-supply) INITIAL_SUPPLY="$2"; shift 2 ;;
        --broadcast)  BROADCAST="--broadcast"; shift ;;
        *)
            echo "error: unknown argument '$1'"
            echo "usage: $0 --name <name> --symbol <symbol> --decimals <decimals> --initial-supply <amount> [--broadcast]"
            exit 1
            ;;
    esac
done

# ── Check required environment variables ──

missing=()
[[ -z "${RPC_URL:-}" ]]         && missing+=("RPC_URL")
[[ -z "${PRIVATE_KEY:-}" ]]     && missing+=("PRIVATE_KEY")
[[ -z "${GATEWAY_ADDRESS:-}" ]] && missing+=("GATEWAY_ADDRESS")

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "error: missing required environment variables: ${missing[*]}"
    echo ""
    echo "  RPC_URL          — subnet's Ethereum JSON-RPC endpoint"
    echo "  PRIVATE_KEY      — deployer's private key (hex)"
    echo "  GATEWAY_ADDRESS  — the subnet's GatewayDiamond contract address"
    exit 1
fi

# ── Check required arguments ──

if [[ -z "$NAME" || -z "$SYMBOL" || -z "$DECIMALS" || -z "$INITIAL_SUPPLY" ]]; then
    echo "error: all arguments are required"
    echo "usage: $0 --name <name> --symbol <symbol> --decimals <decimals> --initial-supply <amount>"
    exit 1
fi

# ── Check tools ──

if ! command -v forge &>/dev/null; then
    echo "error: 'forge' (Foundry) is not installed or not in PATH"
    echo "install: https://book.getfoundry.sh/getting-started/installation"
    exit 1
fi

if ! command -v cast &>/dev/null; then
    echo "error: 'cast' (Foundry) is not installed or not in PATH"
    exit 1
fi

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
    cd "$CONTRACT_DIR" && "${FORGE_CMD[@]}"
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

echo ""
echo "Registering token on gateway $GATEWAY_ADDRESS ..."

REGISTER_OUTPUT=$(
    cast send "$GATEWAY_ADDRESS" \
        "registerBridgeableToken(address)" \
        "$TOKEN_ADDRESS" \
        --rpc-url "$RPC_URL" \
        --private-key "$PRIVATE_KEY"
) || {
    echo "error: registration failed"
    echo "$REGISTER_OUTPUT"
    exit 1
}

echo "Token registered for cross-subnet bridging."
echo ""
echo "Summary:"
echo "  Token address:   $TOKEN_ADDRESS"
echo "  Name:            $NAME"
echo "  Symbol:          $SYMBOL"
echo "  Decimals:        $DECIMALS"
echo "  Initial supply:  $INITIAL_SUPPLY (whole units)"
echo "  Owner:           $DEPLOYER"
echo "  Gateway:         $GATEWAY_ADDRESS"
