// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {ERC20Burnable} from "@openzeppelin/contracts/token/ERC20/extensions/ERC20Burnable.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

/// @notice A minimal ERC20 token that can be registered for cross-subnet bridging
///         via GatewayErcFacet.registerBridgeableToken().
///         Extends Ownable (required by registerBridgeableToken) and ERC20Burnable
///         (so the token owner can burn tokens, which adjusts L1 tracked supply via IPC:ETS).
/// @dev    Deploy with: forge create BridgeableToken --constructor-args "MyToken" "MTK" 18 1000000000000000000000000 $DEPLOYER
contract BridgeableToken is ERC20, ERC20Burnable, Ownable {
    uint8 private immutable _decimals;

    /// @param name_ Token name
    /// @param symbol_ Token symbol
    /// @param decimals_ Number of decimals
    /// @param initialSupply_ Initial supply in smallest units (minted to owner)
    /// @param owner_ The token owner (receives initial supply and can register for bridging)
    constructor(
        string memory name_,
        string memory symbol_,
        uint8 decimals_,
        uint256 initialSupply_,
        address owner_
    ) ERC20(name_, symbol_) Ownable(owner_) {
        _decimals = decimals_;
        _mint(owner_, initialSupply_);
    }

    function decimals() public view override returns (uint8) {
        return _decimals;
    }

    /// @notice Mint new tokens. Only the owner can call this.
    ///         Minting increases T.totalSupply(), which is tracked on Bitcoin L1
    ///         via IPC:ETS supply adjustments in the next checkpoint.
    function mint(address to, uint256 amount) external onlyOwner {
        _mint(to, amount);
    }
}
