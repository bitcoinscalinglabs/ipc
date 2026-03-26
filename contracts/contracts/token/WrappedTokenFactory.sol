// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {WrappedToken} from "./WrappedToken.sol";
import {SubnetID} from "../structs/Subnet.sol";

/// @notice Deploys WrappedToken contracts on demand during ErcTransfer top-down delivery.
///         Called by the gateway inline inside _executeErcTransfer on first delivery of a token.
contract WrappedTokenFactory {
    event WrappedTokenDeployed(
        address indexed homeToken,
        address indexed wrappedToken
    );

    /// @notice Deploy a new WrappedToken and transfer ownership to the caller (gateway).
    /// @param homeSubnet   The subnet where the original token lives.
    /// @param homeToken    The address of the original token on its home subnet.
    /// @param name         Token name — from TokenMetadata stored by recordTokenMetadata().
    /// @param symbol       Token symbol.
    /// @param decimals_    Token decimals.
    /// @return The address of the newly deployed WrappedToken.
    function deployWrappedToken(
        SubnetID memory homeSubnet,
        address homeToken,
        string memory name,
        string memory symbol,
        uint8 decimals_
    ) external returns (address) {
        WrappedToken token = new WrappedToken(homeSubnet, homeToken, name, symbol, decimals_);
        // Transfer ownership to the calling gateway so it becomes the sole minter/burner.
        token.transferOwnership(msg.sender);
        emit WrappedTokenDeployed(homeToken, address(token));
        return address(token);
    }
}
