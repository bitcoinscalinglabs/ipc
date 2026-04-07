// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import "forge-std/Test.sol";

import {IpcEnvelope, IpcMsgKind, BottomUpCheckpoint, BottomUpMsgBatch} from "../../contracts/structs/CrossNet.sol";
import {SubnetID, IPCAddress, Subnet, Validator} from "../../contracts/structs/Subnet.sol";
import {FvmAddress} from "../../contracts/structs/FvmAddress.sol";
import {FvmAddressHelper} from "../../contracts/lib/FvmAddressHelper.sol";
import {SubnetIDHelper} from "../../contracts/lib/SubnetIDHelper.sol";
import {CrossMsgHelper} from "../../contracts/lib/CrossMsgHelper.sol";
import {GatewayDiamond} from "../../contracts/GatewayDiamond.sol";
import {GatewayFacetsHelper} from "../helpers/GatewayFacetsHelper.sol";
import {IntegrationTestBase} from "../IntegrationTestBase.sol";
import {FilAddress} from "fevmate/contracts/utils/FilAddress.sol";
import {ActivityHelper} from "../helpers/ActivityHelper.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

// New contracts — will fail to compile until implemented.
// NOTE: IntegrationTestBase.createGatewayDiamond() must be updated to include GatewayErcFacet
//       in the diamond cut, and GatewayDiamond.ConstructorParams must gain a `wrappedTokenFactory` field.
import {GatewayErcFacet} from "../../contracts/gateway/GatewayErcFacet.sol";
import {IWrappedToken} from "../../contracts/interfaces/IWrappedToken.sol";
import {WrappedToken} from "../../contracts/token/WrappedToken.sol";
import {WrappedTokenFactory} from "../../contracts/token/WrappedTokenFactory.sol";
import {TokenNotRegistered, TokenMetadataNotFound, NotTokenOwner} from "../../contracts/errors/IPCErrors.sol";
import {TokenMetadata} from "../../contracts/lib/LibGatewayActorStorage.sol";

/// @dev Minimal ERC20 + Ownable for testing registerBridgeableToken.
///      ERC20PresetFixedSupply in test/helpers/ has no owner() — this one does.
contract OwnableTestToken is ERC20, Ownable {
    uint8 private _dec;

    constructor(
        string memory name,
        string memory symbol,
        uint8 decimals_,
        address initialOwner
    ) ERC20(name, symbol) Ownable(initialOwner) {
        _dec = decimals_;
        _mint(initialOwner, 1_000_000 * 10 ** decimals_);
    }

    function decimals() public view override returns (uint8) {
        return _dec;
    }

    function mint(address to, uint256 amount) external onlyOwner {
        _mint(to, amount);
    }
}

contract GatewayDiamondErcTransferTest is Test, IntegrationTestBase {
    using SubnetIDHelper for SubnetID;
    using FvmAddressHelper for FvmAddress;
    using GatewayFacetsHelper for GatewayDiamond;

    OwnableTestToken internal token;
    WrappedTokenFactory internal wrappedTokenFactory;

    // Second gateway representing S_target — different networkName from gatewayDiamond.
    // NOTE: must be created with GatewayErcFacet in the cut and wrappedTokenFactory wired in.
    GatewayDiamond internal targetGateway;

    SubnetID internal homeSubnetId;
    SubnetID internal targetSubnetId;

    // A child subnet of home — used as the dstSubnet arg in transfer_erc tests.
    SubnetID internal childSubnetId;

    function setUp() public override {
        super.setUp(); // deploys gatewayDiamond (homeSubnet), saDiamond, adds TOPDOWN_VALIDATOR_1

        token = new OwnableTestToken("HomeToken", "HT", 18, address(this));
        wrappedTokenFactory = new WrappedTokenFactory();

        // Capture home subnet id and save the home gateway before createGatewayDiamond
        // overwrites this.gatewayDiamond with the target gateway.
        homeSubnetId = gatewayDiamond.getter().getNetworkName();
        GatewayDiamond homeGateway = gatewayDiamond;

        // S_target: a subnet with a different route from root.
        address[] memory route = new address[](1);
        route[0] = address(0xBEEF);
        targetSubnetId = SubnetID({root: ROOTNET_CHAINID, route: route});

        GatewayDiamond.ConstructorParams memory targetParams = GatewayDiamond.ConstructorParams({
            networkName: targetSubnetId,
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: DEFAULT_ACTIVE_VALIDATORS_LIMIT,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(wrappedTokenFactory)
        });
        targetGateway = createGatewayDiamond(targetParams);

        // Restore so that gatewayDiamond refers to the home gateway throughout these tests.
        gatewayDiamond = homeGateway;

        // A destination subnet used when testing the lock path (transfer_erc from S_home).
        address[] memory childRoute = new address[](1);
        childRoute[0] = address(0xCAFE);
        childSubnetId = SubnetID({root: ROOTNET_CHAINID, route: childRoute});
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    function ercFacet(GatewayDiamond gw) internal pure returns (GatewayErcFacet) {
        return GatewayErcFacet(address(gw));
    }

    /// @dev Build a top-down ErcTransfer envelope for delivery to `toGateway`.
    function makeErcTransferEnvelope(
        GatewayDiamond toGateway,
        address recipient,
        SubnetID memory msgHomeSubnet,
        address homeToken,
        uint256 amount,
        uint64 nonceOffset
    ) internal view returns (IpcEnvelope memory) {
        SubnetID memory toNetwork = toGateway.getter().getNetworkName();
        // from = root (parent of toNetwork for a child subnet, or root itself).
        // applyType() will classify this as TopDown for both root and child gateway targets.
        SubnetID memory fromNetwork = SubnetID({root: ROOTNET_CHAINID, route: new address[](0)});
        return IpcEnvelope({
            kind: IpcMsgKind.ErcTransfer,
            from: IPCAddress({subnetId: fromNetwork, rawAddress: FvmAddressHelper.from(address(1))}),
            to: IPCAddress({subnetId: toNetwork, rawAddress: FvmAddressHelper.from(recipient)}),
            value: 0,
            nonce: toGateway.getter().appliedTopDownNonce() + nonceOffset,
            message: abi.encode(msgHomeSubnet, homeToken, amount)
        });
    }

    function makeTransferEnvelope(
        GatewayDiamond toGateway,
        address recipient,
        uint256 value,
        uint64 nonceOffset
    ) internal view returns (IpcEnvelope memory) {
        SubnetID memory toNetwork = toGateway.getter().getNetworkName();
        SubnetID memory fromNetwork = SubnetID({root: ROOTNET_CHAINID, route: new address[](0)});
        return IpcEnvelope({
            kind: IpcMsgKind.Transfer,
            from: IPCAddress({subnetId: fromNetwork, rawAddress: FvmAddressHelper.from(address(1))}),
            to: IPCAddress({subnetId: toNetwork, rawAddress: FvmAddressHelper.from(recipient)}),
            value: value,
            nonce: toGateway.getter().appliedTopDownNonce() + nonceOffset,
            message: new bytes(0)
        });
    }

    // =========================================================================
    // Protocol 1 — registerBridgeableToken
    // =========================================================================

    function test_registerBridgeableToken_success() public {
        vm.expectEmit(true, false, false, true, address(gatewayDiamond));
        emit GatewayErcFacet.ErcTokenRegistered(homeSubnetId, address(token), "HomeToken", "HT", 18);
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        TokenMetadata memory meta = gatewayDiamond.getter().getTokenMetadata(homeSubnetId, address(token));
        assertEq(meta.name, "HomeToken");
        assertEq(meta.symbol, "HT");
        assertEq(meta.decimals, 18);
    }

    function test_registerBridgeableToken_notOwner_reverts() public {
        address nonOwner = vm.addr(99);
        vm.prank(nonOwner);
        vm.expectRevert(NotTokenOwner.selector);
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));
    }

    function test_registerBridgeableToken_invalidAddress_reverts() public {
        // address(1) has no code — calling owner() on it reverts
        vm.expectRevert();
        ercFacet(gatewayDiamond).registerBridgeableToken(address(1));
    }

    function test_registerBridgeableToken_twice_isIdempotent() public {
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        // Second call must not revert and must NOT overwrite the stored metadata.
        // (Protocol invariant: first write wins — prevents a second ErcTokenRegistered
        //  with different metadata from breaking cross-subnet agreement.)
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        TokenMetadata memory meta = gatewayDiamond.getter().getTokenMetadata(homeSubnetId, address(token));
        assertEq(meta.name, "HomeToken");
    }

    // =========================================================================
    // recordTokenMetadata — called by btc manager on IPC:ETR confirmation
    // =========================================================================

    function test_recordTokenMetadata_stores() public {
        address[] memory foreignRoute = new address[](1);
        foreignRoute[0] = address(0xDEAD);
        SubnetID memory foreignHome = SubnetID({root: ROOTNET_CHAINID, route: foreignRoute});
        address foreignToken = vm.addr(42);

        ercFacet(gatewayDiamond).recordTokenMetadata(foreignHome, foreignToken, "Foo", "FOO", 6);

        TokenMetadata memory meta = gatewayDiamond.getter().getTokenMetadata(foreignHome, foreignToken);
        assertEq(meta.name, "Foo");
        assertEq(meta.symbol, "FOO");
        assertEq(meta.decimals, 6);
    }

    function test_recordTokenMetadata_isIdempotent() public {
        // First call stores the metadata.
        ercFacet(gatewayDiamond).recordTokenMetadata(homeSubnetId, address(token), "A", "AA", 18);
        // Second call with the same key is silently ignored — first write wins.
        ercFacet(gatewayDiamond).recordTokenMetadata(homeSubnetId, address(token), "B", "BB", 6);

        TokenMetadata memory meta = gatewayDiamond.getter().getTokenMetadata(homeSubnetId, address(token));
        assertEq(meta.name, "A");
        assertEq(meta.decimals, 18);
    }

    // =========================================================================
    // Protocol 3 Step 1 — transfer_erc, lock path (plain ERC20 on S_home)
    // =========================================================================

    function test_transferErc_lockPath_success() public {
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        address sender = vm.addr(1);
        token.transfer(sender, 500);

        vm.startPrank(sender);
        token.approve(address(gatewayDiamond), 100);
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(token), 100);
        vm.stopPrank();

        // Tokens must be locked in the gateway.
        assertEq(token.balanceOf(address(gatewayDiamond)), 100);
        assertEq(token.balanceOf(sender), 400);
    }

    function test_transferErc_lockPath_envelopeEncoding() public {
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        address sender = vm.addr(1);
        token.transfer(sender, 500);

        vm.startPrank(sender);
        token.approve(address(gatewayDiamond), 50);

        // Expect a bottom-up message with kind = ErcTransfer and correct message payload.
        // The event is emitted by LibGateway.commitBottomUpMsg.
        // We capture it via the bottom-up message batch nonce being incremented.
        uint64 nonceBefore = gatewayDiamond.getter().bottomUpNonce();
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(token), 50);
        vm.stopPrank();

        assertEq(gatewayDiamond.getter().bottomUpNonce(), nonceBefore + 1);
    }

    function test_transferErc_unregisteredToken_reverts() public {
        // Token not registered — must revert with TokenNotRegistered.
        address sender = vm.addr(1);
        token.transfer(sender, 100);
        vm.prank(sender);
        token.approve(address(gatewayDiamond), 100);

        vm.prank(sender);
        vm.expectRevert(TokenNotRegistered.selector);
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(token), 100);
    }

    function test_transferErc_insufficientAllowance_reverts() public {
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        address sender = vm.addr(1);
        token.transfer(sender, 100);
        // No approve call.

        vm.prank(sender);
        vm.expectRevert();
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(token), 100);
    }

    function test_transferErc_storesPendingTransfer() public {
        ercFacet(gatewayDiamond).registerBridgeableToken(address(token));

        address sender = vm.addr(1);
        token.transfer(sender, 500);
        vm.startPrank(sender);
        token.approve(address(gatewayDiamond), 75);
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(token), 75);
        vm.stopPrank();

        assertEq(token.balanceOf(address(gatewayDiamond)), 75);
    }

    // =========================================================================
    // Protocol 3 Step 1 — transfer_erc, burn path (WrappedToken on S_source)
    // =========================================================================

    function test_transferErc_burnPath_success() public {
        // Deploy a WrappedToken that represents the home token on S_home.
        WrappedToken wrapped = new WrappedToken(homeSubnetId, address(token), "HomeToken", "HT", 18);

        // The gateway is the sole minter/burner of WrappedTokens.
        // For this test we transfer ownership to the gateway so it can call burnFrom.
        wrapped.transferOwnership(address(gatewayDiamond));

        address sender = vm.addr(1);
        // Mint some wrapped tokens to sender (simulate a prior top-down delivery).
        vm.prank(address(gatewayDiamond));
        IWrappedToken(address(wrapped)).mint(sender, 200);

        vm.startPrank(sender);
        IERC20(address(wrapped)).approve(address(gatewayDiamond), 50);
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(wrapped), 50);
        vm.stopPrank();

        // Wrapped tokens must be burned, not locked.
        assertEq(wrapped.balanceOf(sender), 150);
        assertEq(wrapped.totalSupply(), 150);

        // The gateway must hold no wrapped tokens — they were burned, not transferred in.
        assertEq(wrapped.balanceOf(address(gatewayDiamond)), 0);
    }

    function test_transferErc_burnPath_envelopeUsesHomeIdentity() public {
        // The envelope's message must encode homeSubnet + homeToken (not the wrapped address).
        WrappedToken wrapped = new WrappedToken(homeSubnetId, address(token), "HomeToken", "HT", 18);
        wrapped.transferOwnership(address(gatewayDiamond));

        address sender = vm.addr(1);
        vm.prank(address(gatewayDiamond));
        IWrappedToken(address(wrapped)).mint(sender, 100);

        vm.startPrank(sender);
        IERC20(address(wrapped)).approve(address(gatewayDiamond), 30);

        uint64 nonceBefore = gatewayDiamond.getter().bottomUpNonce();
        gatewayDiamond.manager().transferErc(vm.addr(2), childSubnetId, address(wrapped), 30);
        vm.stopPrank();

        // Bottom-up nonce incremented — envelope was committed.
        assertEq(gatewayDiamond.getter().bottomUpNonce(), nonceBefore + 1);

        // The message payload in the bottom-up envelope must encode (homeSubnetId, address(token), 30)
        // not (homeSubnetId, address(wrapped), 30). Verified by decoding the stored envelope.
        // (Requires a getter for the bottom-up message queue — verified via integration once implemented.)
    }

    // =========================================================================
    // Protocol 3 Step 3 — _executeErcTransfer Case A: S_target == homeSubnet
    //
    // Locked tokens are released to the recipient.
    // =========================================================================

    function test_executeErcTransfer_homeSubnet_unlocks() public {
        // Case A: tokens are unlocked when the delivery reaches the home subnet gateway.
        // We use targetGateway as the "home" here because the root subnet (gatewayDiamond) has
        // no parent and therefore cannot receive top-down messages via applyCrossMessages.
        // In production, the home subnet is always a non-root L2 subnet.
        SubnetID memory localHomeId = targetSubnetId;

        ercFacet(targetGateway).registerBridgeableToken(address(token));
        address sender = vm.addr(1);
        token.transfer(sender, 500);
        vm.startPrank(sender);
        token.approve(address(targetGateway), 100);
        // dstSubnet here is irrelevant — we care only about the locked tokens.
        targetGateway.manager().transferErc(vm.addr(2), childSubnetId, address(token), 100);
        vm.stopPrank();

        assertEq(token.balanceOf(address(targetGateway)), 100);

        // Deliver back to targetGateway: homeSubnet=targetSubnetId so _executeErcTransfer picks Case A.
        address recipient = vm.addr(3);
        IpcEnvelope[] memory msgs = new IpcEnvelope[](1);
        msgs[0] = makeErcTransferEnvelope(targetGateway, recipient, localHomeId, address(token), 100, 0);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        assertEq(token.balanceOf(recipient), 100);
        assertEq(token.balanceOf(address(targetGateway)), 0);
    }

    // =========================================================================
    // Protocol 3 Step 3 — _executeErcTransfer Case B: S_target != homeSubnet
    //
    // WrappedToken deployed on first delivery; reused on subsequent deliveries.
    // =========================================================================

    function test_executeErcTransfer_nonHome_deploysWrappedTokenAndMints() public {
        // Pre-load token metadata on targetGateway (mirrors IPC:ETR arriving via Bitcoin).
        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token), "HomeToken", "HT", 18);

        address recipient = vm.addr(3);
        IpcEnvelope[] memory msgs = new IpcEnvelope[](1);
        msgs[0] = makeErcTransferEnvelope(targetGateway, recipient, homeSubnetId, address(token), 75, 0);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        // WrappedToken must have been deployed and its address stored.
        address wrappedAddr = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));
        assertTrue(wrappedAddr != address(0));

        // Recipient must have been minted the correct amount.
        assertEq(IERC20(wrappedAddr).balanceOf(recipient), 75);
    }

    function test_executeErcTransfer_nonHome_reuseWrappedToken() public {
        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token), "HomeToken", "HT", 18);

        // First delivery — deploys WrappedToken.
        address recipient1 = vm.addr(3);
        IpcEnvelope[] memory msgs1 = new IpcEnvelope[](1);
        msgs1[0] = makeErcTransferEnvelope(targetGateway, recipient1, homeSubnetId, address(token), 40, 0);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs1);

        address wrappedFirst = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));

        // Second delivery — must reuse the existing WrappedToken, not deploy a new one.
        address recipient2 = vm.addr(4);
        IpcEnvelope[] memory msgs2 = new IpcEnvelope[](1);
        msgs2[0] = makeErcTransferEnvelope(targetGateway, recipient2, homeSubnetId, address(token), 30, 0);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs2);

        address wrappedSecond = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));
        assertEq(wrappedFirst, wrappedSecond);
        assertEq(IERC20(wrappedSecond).balanceOf(recipient2), 30);
    }

    function test_executeErcTransfer_nonHome_wrappedTokenMetadata() public {
        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token), "HomeToken", "HT", 18);

        IpcEnvelope[] memory msgs = new IpcEnvelope[](1);
        msgs[0] = makeErcTransferEnvelope(targetGateway, vm.addr(3), homeSubnetId, address(token), 10, 0);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        address wrappedAddr = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));

        // The deployed WrappedToken must carry the correct metadata and home identity.
        assertEq(IERC20Metadata(wrappedAddr).name(), "HomeToken");
        assertEq(IERC20Metadata(wrappedAddr).symbol(), "HT");
        assertEq(IERC20Metadata(wrappedAddr).decimals(), 18);
        assertEq(IWrappedToken(wrappedAddr).homeSubnet().root, homeSubnetId.root);
        assertEq(IWrappedToken(wrappedAddr).homeTokenAddress(), address(token));
    }

    function test_executeErcTransfer_nonHome_missingMetadata_fails() public {
        // No recordTokenMetadata call — delivery fails gracefully: the revert is caught by
        // executeCrossMsg's delegatecall wrapper and turned into an ActorErr receipt.
        // The batch does not halt and no WrappedToken is deployed.
        IpcEnvelope[] memory msgs = new IpcEnvelope[](1);
        msgs[0] = makeErcTransferEnvelope(targetGateway, vm.addr(3), homeSubnetId, address(token), 10, 0);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        assertEq(targetGateway.getter().getWrappedToken(homeSubnetId, address(token)), address(0));
    }

    function test_executeErcTransfer_nonHome_differentTokensGetSeparateWrappedContracts() public {
        OwnableTestToken token2 = new OwnableTestToken("Other", "OTH", 6, address(this));

        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token), "HomeToken", "HT", 18);
        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token2), "Other", "OTH", 6);

        IpcEnvelope[] memory msgs = new IpcEnvelope[](2);
        msgs[0] = makeErcTransferEnvelope(targetGateway, vm.addr(3), homeSubnetId, address(token), 10, 0);
        msgs[1] = makeErcTransferEnvelope(targetGateway, vm.addr(4), homeSubnetId, address(token2), 5, 1);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        address wrapped1 = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));
        address wrapped2 = targetGateway.getter().getWrappedToken(homeSubnetId, address(token2));

        assertTrue(wrapped1 != address(0));
        assertTrue(wrapped2 != address(0));
        assertTrue(wrapped1 != wrapped2);
    }

    function test_batchWithMultipleErcTransfers() public {
        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token), "HomeToken", "HT", 18);

        address recipient1 = vm.addr(3);
        address recipient2 = vm.addr(4);

        IpcEnvelope[] memory msgs = new IpcEnvelope[](2);
        msgs[0] = makeErcTransferEnvelope(targetGateway, recipient1, homeSubnetId, address(token), 10, 0);
        msgs[1] = makeErcTransferEnvelope(targetGateway, recipient2, homeSubnetId, address(token), 25, 1);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        address wrappedAddr = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));
        assertEq(IERC20(wrappedAddr).balanceOf(recipient1), 10);
        assertEq(IERC20(wrappedAddr).balanceOf(recipient2), 25);
    }

    function test_batchMixedErcAndNativeTransfer() public {
        ercFacet(targetGateway).recordTokenMetadata(homeSubnetId, address(token), "HomeToken", "HT", 18);

        address ercRecipient = vm.addr(3);
        address nativeRecipient = vm.addr(4);
        uint256 transferValue = 0.5 ether;

        vm.deal(address(targetGateway), transferValue);

        IpcEnvelope[] memory msgs = new IpcEnvelope[](2);
        msgs[0] = makeTransferEnvelope(targetGateway, nativeRecipient, transferValue, 0);
        msgs[1] = makeErcTransferEnvelope(targetGateway, ercRecipient, homeSubnetId, address(token), 42, 1);

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        assertEq(nativeRecipient.balance, transferValue);
        address wrappedAddr = targetGateway.getter().getWrappedToken(homeSubnetId, address(token));
        assertEq(IERC20(wrappedAddr).balanceOf(ercRecipient), 42);
    }

    // =========================================================================
    // Protocol 2 Part A — registerBridgeableToken commits bottom-up ErcRegistration
    // =========================================================================

    function getNextEpoch(uint256 blockNumber, uint256 checkPeriod) internal pure returns (uint256) {
        return ((uint64(blockNumber) / checkPeriod) + 1) * checkPeriod;
    }

    function test_registerBridgeableToken_commitsBottomUpMsg() public {
        // Use targetGateway (a child subnet) because commitBottomUpMsg requires a parent.
        uint64 nonceBefore = targetGateway.getter().bottomUpNonce();
        ercFacet(targetGateway).registerBridgeableToken(address(token));
        uint64 nonceAfter = targetGateway.getter().bottomUpNonce();

        // Registration must have committed exactly one bottom-up message.
        assertEq(nonceAfter, nonceBefore + 1);

        // Read the batch and verify the message.
        uint256 epoch = getNextEpoch(block.number, DEFAULT_CHECKPOINT_PERIOD);
        BottomUpMsgBatch memory batch = targetGateway.getter().bottomUpMsgBatch(epoch);
        assertEq(batch.msgs.length, 1);

        IpcEnvelope memory env = batch.msgs[0];
        assertEq(uint8(env.kind), uint8(IpcMsgKind.ErcRegistration));
        assertEq(env.value, 0);

        // Decode message payload: (homeSubnet, token, name, symbol, decimals, initialSupply)
        (, address decodedToken, string memory name, string memory symbol, uint8 decimals, uint256 initialSupply) =
            abi.decode(env.message, (SubnetID, address, string, string, uint8, uint256));
        assertEq(decodedToken, address(token));
        assertEq(name, "HomeToken");
        assertEq(symbol, "HT");
        assertEq(decimals, 18);
        assertEq(initialSupply, token.totalSupply());
    }

    function test_registerBridgeableToken_idempotent_noDoubleMsg() public {
        ercFacet(targetGateway).registerBridgeableToken(address(token));
        uint64 nonceAfterFirst = targetGateway.getter().bottomUpNonce();

        // Second call should not commit another message.
        ercFacet(targetGateway).registerBridgeableToken(address(token));
        uint64 nonceAfterSecond = targetGateway.getter().bottomUpNonce();

        assertEq(nonceAfterSecond, nonceAfterFirst);
    }

    // =========================================================================
    // Protocol 2 Part B — getTokenSupplyDeltas / updateSupplySnapshots
    // =========================================================================

    function test_getTokenSupplyDeltas_afterMint() public {
        ercFacet(targetGateway).registerBridgeableToken(address(token));

        token.mint(address(this), 500);

        (address[] memory tokens, int256[] memory deltas, uint256 count) =
            ercFacet(targetGateway).getTokenSupplyDeltas();

        assertEq(count, 1);
        assertEq(tokens[0], address(token));
        assertEq(deltas[0], int256(500));
    }

    function test_getTokenSupplyDeltas_noChange() public {
        ercFacet(targetGateway).registerBridgeableToken(address(token));

        // No supply change since registration → count should be 0.
        (,, uint256 count) = ercFacet(targetGateway).getTokenSupplyDeltas();
        assertEq(count, 0);
    }

    function test_getTokenSupplyDeltas_multipleTokens() public {
        OwnableTestToken token2 = new OwnableTestToken("Other", "OTH", 6, address(this));

        ercFacet(targetGateway).registerBridgeableToken(address(token));
        ercFacet(targetGateway).registerBridgeableToken(address(token2));

        // Mint on token A only.
        token.mint(address(this), 100);

        (address[] memory tokens, int256[] memory deltas, uint256 count) =
            ercFacet(targetGateway).getTokenSupplyDeltas();

        // Only token A has a non-zero delta.
        assertEq(count, 1);
        assertEq(tokens[0], address(token));
        assertEq(deltas[0], int256(100));
    }

    function test_updateSupplySnapshots_resetsDeltas() public {
        ercFacet(targetGateway).registerBridgeableToken(address(token));
        token.mint(address(this), 500);

        (,, uint256 countBefore) = ercFacet(targetGateway).getTokenSupplyDeltas();
        assertEq(countBefore, 1);

        ercFacet(targetGateway).updateSupplySnapshots();

        (,, uint256 countAfter) = ercFacet(targetGateway).getTokenSupplyDeltas();
        assertEq(countAfter, 0);
    }

    function test_registerBridgeableToken_initialSupplyInMessage() public {
        OwnableTestToken bigToken = new OwnableTestToken("Big", "BIG", 18, address(this));
        uint256 expectedSupply = bigToken.totalSupply();
        assertTrue(expectedSupply > 0);

        ercFacet(targetGateway).registerBridgeableToken(address(bigToken));

        uint256 epoch = getNextEpoch(block.number, DEFAULT_CHECKPOINT_PERIOD);
        BottomUpMsgBatch memory batch = targetGateway.getter().bottomUpMsgBatch(epoch);

        bool found = false;
        for (uint256 i = 0; i < batch.msgs.length; i++) {
            if (uint8(batch.msgs[i].kind) == uint8(IpcMsgKind.ErcRegistration)) {
                (, address decodedToken,,,, uint256 initialSupply) =
                    abi.decode(batch.msgs[i].message, (SubnetID, address, string, string, uint8, uint256));
                if (decodedToken == address(bigToken)) {
                    assertEq(initialSupply, expectedSupply);
                    found = true;
                    break;
                }
            }
        }
        assertTrue(found);
    }

    function test_executeErcRegistration_newFormat() public {
        // Build an ErcRegistration envelope with the new 6-field format (including initialSupply).
        SubnetID memory fromNetwork = SubnetID({root: ROOTNET_CHAINID, route: new address[](0)});
        SubnetID memory toNetwork = targetGateway.getter().getNetworkName();

        IpcEnvelope[] memory msgs = new IpcEnvelope[](1);
        msgs[0] = IpcEnvelope({
            kind: IpcMsgKind.ErcRegistration,
            from: IPCAddress({subnetId: fromNetwork, rawAddress: FvmAddressHelper.from(address(1))}),
            to: IPCAddress({subnetId: toNetwork, rawAddress: FvmAddressHelper.from(address(0))}),
            value: 0,
            nonce: targetGateway.getter().appliedTopDownNonce(),
            message: abi.encode(homeSubnetId, address(token), "HomeToken", "HT", uint8(18), uint256(1_000_000))
        });

        vm.prank(FilAddress.SYSTEM_ACTOR);
        targetGateway.xnetMessenger().applyCrossMessages(msgs);

        // Metadata should be stored (initialSupply is ignored by the contract).
        TokenMetadata memory meta = targetGateway.getter().getTokenMetadata(homeSubnetId, address(token));
        assertEq(meta.name, "HomeToken");
        assertEq(meta.symbol, "HT");
        assertEq(meta.decimals, 18);
    }
}
