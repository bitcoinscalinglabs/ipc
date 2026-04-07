// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {GatewayActorModifiers} from "../lib/LibGatewayActorStorage.sol";
import {TokenMetadata} from "../lib/LibGatewayActorStorage.sol";
import {SubnetID, IPCAddress} from "../structs/Subnet.sol";
import {SubnetIDHelper} from "../lib/SubnetIDHelper.sol";
import {IpcEnvelope, IpcMsgKind} from "../structs/CrossNet.sol";
import {FvmAddressHelper} from "../lib/FvmAddressHelper.sol";
import {LibGateway} from "../lib/LibGateway.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {NotTokenOwner} from "../errors/IPCErrors.sol";

/// @dev Minimal interface for tokens that expose an owner() function (EIP-173 / Ownable).
interface IERC20Ownable {
    function owner() external view returns (address);
}

/// @notice Gateway facet for ERC20 token bridging registration.
///         Handles home-subnet token registration and cross-subnet metadata propagation.
contract GatewayErcFacet is GatewayActorModifiers {
    using SubnetIDHelper for SubnetID;

    /// @notice Emitted when a token is registered for cross-subnet bridging on its home subnet.
    /// An ERC20 token is uniquely identified by the pair (homeSubnet, token),
    /// where homeSubnet is the subnet in which it is natively/locally deployed.
    event ErcTokenRegistered(
        SubnetID homeSubnet,
        address indexed token,
        string name,
        string symbol,
        uint8 decimals
    );

    /// @notice Register a locally-deployed ERC20 token for cross-subnet bridging.
    ///         Caller must be the token owner (via Ownable.owner()).
    ///         Idempotent: a second call for the same token is silently ignored — first write wins.
    /// @param tokenAddress The address of the ERC20 token to register.
    function registerBridgeableToken(address tokenAddress) external {
        if (IERC20Ownable(tokenAddress).owner() != msg.sender) {
            revert NotTokenOwner();
        }

        // Idempotent: first write wins — prevents a second ErcTokenRegistered event
        // with different metadata, which would break cross-subnet metadata agreement.
        if (s.registeredBridgeableTokens[tokenAddress]) {
            return;
        }

        string memory name = IERC20Metadata(tokenAddress).name();
        string memory symbol = IERC20Metadata(tokenAddress).symbol();
        uint8 dec = IERC20Metadata(tokenAddress).decimals();
        uint256 initialSupply = IERC20(tokenAddress).totalSupply();

        s.registeredBridgeableTokens[tokenAddress] = true;
        s.registeredTokenAddresses.push(tokenAddress);
        s.lastCheckpointedSupply[tokenAddress] = initialSupply;

        bytes32 key = keccak256(abi.encode(s.networkName, tokenAddress));
        s.tokenMetadata[key] = TokenMetadata({name: name, symbol: symbol, decimals: dec});

        // Commit bottom-up ErcRegistration message for propagation to Bitcoin and all subnets.
        IpcEnvelope memory envelope = IpcEnvelope({
            kind: IpcMsgKind.ErcRegistration,
            from: IPCAddress({subnetId: s.networkName, rawAddress: FvmAddressHelper.from(msg.sender)}),
            to: IPCAddress({subnetId: s.networkName.getParentSubnet(), rawAddress: FvmAddressHelper.from(address(0))}),
            value: 0,
            nonce: 0,
            message: abi.encode(s.networkName, tokenAddress, name, symbol, dec, initialSupply)
        });
        LibGateway.commitBottomUpMsg(envelope);

        emit ErcTokenRegistered(s.networkName, tokenAddress, name, symbol, dec);
    }

    /// @notice Store token metadata received via IPC:ETR from Bitcoin.
    ///         Called by the btc manager after an IPC:ETR record is confirmed on Bitcoin.
    ///         Idempotent: if metadata for this (homeSubnet, homeToken) already exists, the call
    ///         is silently ignored — first write wins, preserving cross-subnet agreement.
    /// @param homeSubnet       The subnet where the token is natively deployed.
    /// @param homeToken        The token address on its home subnet.
    /// @param name             Token name.
    /// @param symbol           Token symbol.
    /// @param decimals         Token decimals.
    function recordTokenMetadata(
        SubnetID calldata homeSubnet,
        address homeToken,
        string calldata name,
        string calldata symbol,
        uint8 decimals
    ) external {
        bytes32 key = keccak256(abi.encode(homeSubnet, homeToken));
        // First write wins — metadata is immutable once recorded.
        if (bytes(s.tokenMetadata[key].name).length != 0) {
            return;
        }
        s.tokenMetadata[key] = TokenMetadata({name: name, symbol: symbol, decimals: decimals});
    }

    /// @notice Returns supply deltas for all registered tokens since the last checkpoint.
    ///         Called by fendermint at checkpoint time to compute IPC:ETS records.
    ///         Uses try/catch so a broken token contract cannot block checkpointing.
    /// @return tokens  Array of token addresses (max-sized; first `count` entries are valid).
    /// @return deltas  Corresponding signed deltas (positive = mint, negative = burn).
    /// @return count   Number of valid entries (tokens with non-zero deltas).
    function getTokenSupplyDeltas()
        external
        view
        returns (address[] memory tokens, int256[] memory deltas, uint256 count)
    {
        uint256 len = s.registeredTokenAddresses.length;
        tokens = new address[](len);
        deltas = new int256[](len);
        count = 0;

        for (uint256 i = 0; i < len; ) {
            address token = s.registeredTokenAddresses[i];
            try IERC20(token).totalSupply() returns (uint256 currentSupply) {
                int256 delta = int256(currentSupply) - int256(s.lastCheckpointedSupply[token]);
                if (delta != 0) {
                    tokens[count] = token;
                    deltas[count] = delta;
                    count++;
                }
            } catch {
                // Skip broken tokens — don't block checkpointing.
            }
            unchecked { ++i; }
        }
    }

    /// @notice Updates stored supply snapshots to current totalSupply values.
    ///         Called by fendermint after reading deltas, so the next checkpoint starts fresh.
    ///         Uses try/catch so a broken token contract cannot block checkpointing.
    function updateSupplySnapshots() external {
        uint256 len = s.registeredTokenAddresses.length;
        for (uint256 i = 0; i < len; ) {
            address token = s.registeredTokenAddresses[i];
            try IERC20(token).totalSupply() returns (uint256 currentSupply) {
                s.lastCheckpointedSupply[token] = currentSupply;
            } catch {
                // Skip broken tokens.
            }
            unchecked { ++i; }
        }
    }
}
