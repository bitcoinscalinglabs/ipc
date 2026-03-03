// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// @title RewardToken
/// @notice ERC20 reward token for IPC emission chain. Only the designated minter (system actor) can mint.
contract RewardToken is ERC20 {
    address public immutable minter;

    /// @param name Token name
    /// @param symbol Token symbol
    /// @param _minter Address allowed to mint (system actor)
    constructor(
        string memory name,
        string memory symbol,
        address _minter
    ) ERC20(name, symbol) {
        require(_minter != address(0), "RewardToken: zero minter");
        minter = _minter;
    }

    /// @notice Mint tokens to a recipient. Only callable by the minter (system actor).
    /// @param to Recipient address
    /// @param amount Amount to mint
    function mint(address to, uint256 amount) external {
        require(msg.sender == minter, "RewardToken: only minter");
        require(to != address(0), "RewardToken: zero address");
        require(amount > 0, "RewardToken: zero amount");
        _mint(to, amount);
    }
}
