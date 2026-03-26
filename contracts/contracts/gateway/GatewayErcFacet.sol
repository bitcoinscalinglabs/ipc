// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {GatewayActorModifiers} from "../lib/LibGatewayActorStorage.sol";
import {TokenMetadata} from "../lib/LibGatewayActorStorage.sol";
import {SubnetID} from "../structs/Subnet.sol";
import {SubnetIDHelper} from "../lib/SubnetIDHelper.sol";
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

        s.registeredBridgeableTokens[tokenAddress] = true;

        bytes32 key = keccak256(abi.encode(s.networkName, tokenAddress));
        s.tokenMetadata[key] = TokenMetadata({name: name, symbol: symbol, decimals: dec});

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
}
