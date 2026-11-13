// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.23;

import "forge-std/Test.sol";

import "../../contracts/errors/IPCErrors.sol";
import {NumberContractFacetSeven} from "../helpers/contracts/NumberContractFacetSeven.sol";
import {NumberContractFacetEight} from "../helpers/contracts/NumberContractFacetEight.sol";
import {EMPTY_BYTES, METHOD_SEND} from "../../contracts/constants/Constants.sol";
import {IERC165} from "../../contracts/interfaces/IERC165.sol";
import {IDiamond} from "../../contracts/interfaces/IDiamond.sol";
import {IDiamondLoupe} from "../../contracts/interfaces/IDiamondLoupe.sol";
import {IDiamondCut} from "../../contracts/interfaces/IDiamondCut.sol";
import {QuorumInfo} from "../../contracts/structs/Quorum.sol";
import {IpcEnvelope, CallMsg, BottomUpMsgBatch, BottomUpCheckpoint, ParentFinality} from "../../contracts/structs/CrossNet.sol";
import {FvmAddress} from "../../contracts/structs/FvmAddress.sol";
import {SlashPreSignature} from "../../contracts/structs/Bitcoin.sol";
import {SubnetID, Subnet, IPCAddress, Validator, StakingChange, StakingChangeRequest, Asset, StakingOperation} from "../../contracts/structs/Subnet.sol";
import {SubnetIDHelper} from "../../contracts/lib/SubnetIDHelper.sol";
import {FvmAddressHelper} from "../../contracts/lib/FvmAddressHelper.sol";
import {CrossMsgHelper} from "../../contracts/lib/CrossMsgHelper.sol";
import {FilAddress} from "fevmate/contracts/utils/FilAddress.sol";
import {GatewayDiamond, FunctionNotFound} from "../../contracts/GatewayDiamond.sol";
import {GatewayGetterFacet} from "../../contracts/gateway/GatewayGetterFacet.sol";
import {GatewayManagerFacet} from "../../contracts/gateway/GatewayManagerFacet.sol";

import {CheckpointingFacet} from "../../contracts/gateway/router/CheckpointingFacet.sol";
import {XnetMessagingFacet} from "../../contracts/gateway/router/XnetMessagingFacet.sol";
import {TopDownFinalityFacet} from "../../contracts/gateway/router/TopDownFinalityFacet.sol";

import {ERR_GENERAL_CROSS_MSG_DISABLED} from "../../contracts/gateway/GatewayMessengerFacet.sol";
import {DiamondCutFacet} from "../../contracts/diamond/DiamondCutFacet.sol";
import {LibDiamond} from "../../contracts/lib/LibDiamond.sol";
import {MerkleTreeHelper} from "../helpers/MerkleTreeHelper.sol";
import {TestUtils, MockIpcContract} from "../helpers/TestUtils.sol";
import {IntegrationTestBase, SubnetWithNativeTokenMock} from "../IntegrationTestBase.sol";
import {SelectorLibrary} from "../helpers/SelectorLibrary.sol";
import {GatewayFacetsHelper} from "../helpers/GatewayFacetsHelper.sol";

import {SubnetActorDiamond} from "../../contracts/SubnetActorDiamond.sol";
import {SubnetActorFacetsHelper} from "../helpers/SubnetActorFacetsHelper.sol";

import {FullActivityRollup, Consensus} from "../../contracts/structs/Activity.sol";
import {ActivityHelper} from "../helpers/ActivityHelper.sol";

import {Strings} from "@openzeppelin/contracts/utils/Strings.sol";

contract GatewayActorDiamondTest is Test, IntegrationTestBase, SubnetWithNativeTokenMock {
    using SubnetIDHelper for SubnetID;
    using CrossMsgHelper for IpcEnvelope;
    using FvmAddressHelper for FvmAddress;
    using GatewayFacetsHelper for GatewayDiamond;
    using SubnetActorFacetsHelper for SubnetActorDiamond;

    function setUp() public override {
        super.setUp();
    }

    function testGatewayDiamond_TransferOwnership() public {
        address owner = gatewayDiamond.ownership().owner();

        vm.expectRevert(LibDiamond.InvalidAddress.selector);
        gatewayDiamond.ownership().transferOwnership(address(0));

        gatewayDiamond.ownership().transferOwnership(address(1));

        address newOwner = gatewayDiamond.ownership().owner();
        require(owner != newOwner, "ownership should be updated");
        require(newOwner == address(1), "new owner not address 1");

        vm.expectRevert(NotOwner.selector);
        gatewayDiamond.ownership().transferOwnership(address(1));
    }

    function testGatewayDiamond_Constructor() public view {
        require(gatewayDiamond.getter().totalSubnets() == 0, "unexpected totalSubnets");
        require(gatewayDiamond.getter().bottomUpNonce() == 0, "unexpected bottomUpNonce");
        require(
            gatewayDiamond.getter().bottomUpCheckPeriod() == DEFAULT_CHECKPOINT_PERIOD,
            "unexpected bottomUpCheckPeriod"
        );
        require(
            gatewayDiamond.getter().getNetworkName().equals(SubnetID({root: ROOTNET_CHAINID, route: new address[](0)})),
            "unexpected getNetworkName"
        );
        require(
            gatewayDiamond.getter().majorityPercentage() == DEFAULT_MAJORITY_PERCENTAGE,
            "unexpected majorityPercentage"
        );

        IpcEnvelope memory storableMsg = gatewayDiamond.getter().postbox(0);
        IpcEnvelope memory msg1;
        require(msg1.toHash() == storableMsg.toHash(), "unexpected hash");
    }

    function testGatewayDiamond_NewGatewayWithDefaultParams() public view {
        GatewayDiamond.ConstructorParams memory params = defaultGatewayParams();

        require(
            gatewayDiamond.getter().bottomUpCheckPeriod() == params.bottomUpCheckPeriod,
            "unexpected bottom-up period"
        );
        require(
            gatewayDiamond.getter().majorityPercentage() == params.majorityPercentage,
            "unexpected majority percentage"
        );
    }

    function testGatewayDiamond_LoupeFunction() public view {
        require(gatewayDiamond.diamondLouper().facets().length == 10, "unexpected length");
        require(
            gatewayDiamond.diamondLouper().supportsInterface(type(IERC165).interfaceId) == true,
            "IERC165 not supported"
        );
        require(
            gatewayDiamond.diamondLouper().supportsInterface(type(IDiamondCut).interfaceId) == true,
            "IDiamondCut not supported"
        );
        require(
            gatewayDiamond.diamondLouper().supportsInterface(type(IDiamondLoupe).interfaceId) == true,
            "IDiamondLoupe not supported"
        );
    }

    function testGatewayDiamond_DiamondCut() public {
        // add method getNum to gateway diamond and assert it can be correctly called
        // replace method getNum and assert it was correctly updated
        // delete method getNum and assert it no longer is callable
        // assert that diamondCut cannot be called by non-owner

        NumberContractFacetSeven ncFacetA = new NumberContractFacetSeven();
        NumberContractFacetEight ncFacetB = new NumberContractFacetEight();

        DiamondCutFacet gwDiamondCutter = DiamondCutFacet(address(gatewayDiamond));
        IDiamond.FacetCut[] memory gwDiamondCut = new IDiamond.FacetCut[](1);

        bytes4[] memory ncGetterSelectors = new bytes4[](1);
        ncGetterSelectors[0] = NumberContractFacetSeven.getNum.selector;

        gwDiamondCut[0] = (
            IDiamond.FacetCut({
                facetAddress: address(ncFacetA),
                action: IDiamond.FacetCutAction.Add,
                functionSelectors: ncGetterSelectors
            })
        );
        //test that other user cannot call diamondcut to add function
        vm.prank(0x1234567890123456789012345678901234567890);
        vm.expectRevert(NotOwner.selector);
        gwDiamondCutter.diamondCut(gwDiamondCut, address(0), new bytes(0));

        gwDiamondCutter.diamondCut(gwDiamondCut, address(0), new bytes(0));

        NumberContractFacetSeven gwNumberContract = NumberContractFacetSeven(address(gatewayDiamond));
        assert(gwNumberContract.getNum() == 7);

        ncGetterSelectors = new bytes4[](1);
        ncGetterSelectors[0] = NumberContractFacetEight.getNum.selector;

        gwDiamondCut[0] = (
            IDiamond.FacetCut({
                facetAddress: address(ncFacetB),
                action: IDiamond.FacetCutAction.Replace,
                functionSelectors: ncGetterSelectors
            })
        );

        //test that other user cannot call diamondcut to replace function
        vm.prank(0x1234567890123456789012345678901234567890);
        vm.expectRevert(NotOwner.selector);
        gwDiamondCutter.diamondCut(gwDiamondCut, address(0), new bytes(0));

        gwDiamondCutter.diamondCut(gwDiamondCut, address(0), new bytes(0));

        assert(gwNumberContract.getNum() == 8);

        //remove facet for getNum
        gwDiamondCut[0] = (
            IDiamond.FacetCut({
                facetAddress: 0x0000000000000000000000000000000000000000,
                action: IDiamond.FacetCutAction.Remove,
                functionSelectors: ncGetterSelectors
            })
        );

        //test that other user cannot call diamondcut to remove function
        vm.prank(0x1234567890123456789012345678901234567890);
        vm.expectRevert(NotOwner.selector);
        gwDiamondCutter.diamondCut(gwDiamondCut, address(0), new bytes(0));

        gwDiamondCutter.diamondCut(gwDiamondCut, address(0), new bytes(0));

        //assert that calling getNum fails
        vm.expectRevert(abi.encodePacked(FunctionNotFound.selector, ncGetterSelectors));
        gwNumberContract.getNum();
    }

    function testGatewayDiamond_Deployment_Works_Root(uint64 checkpointPeriod) public {
        vm.assume(checkpointPeriod >= DEFAULT_CHECKPOINT_PERIOD);

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
            bottomUpCheckPeriod: checkpointPeriod,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });

        GatewayDiamond dep = createGatewayDiamond(constructorParams);
        GatewayGetterFacet depGetter = dep.getter();

        SubnetID memory networkName = depGetter.getNetworkName();

        require(depGetter.getCommitSha() == bytes32(DEFAULT_COMMIT_SHA), "unexpected commit sha");
        require(networkName.isRoot(), "unexpected networkName");
        require(depGetter.bottomUpCheckPeriod() == checkpointPeriod, "gw.bottomUpCheckPeriod() == checkpointPeriod");
        require(
            depGetter.majorityPercentage() == DEFAULT_MAJORITY_PERCENTAGE,
            "gw.majorityPercentage() == DEFAULT_MAJORITY_PERCENTAGE"
        );
    }

    function testGatewayDiamond_Deployment_Works_NotRoot(uint64 checkpointPeriod) public {
        vm.assume(checkpointPeriod >= DEFAULT_CHECKPOINT_PERIOD);

        address[] memory path = new address[](2);
        path[0] = address(0);
        path[1] = address(1);

        GatewayGetterFacet depGetter = new GatewayGetterFacet();
        GatewayManagerFacet depManager = new GatewayManagerFacet();

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
            bottomUpCheckPeriod: checkpointPeriod,
            majorityPercentage: 100,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });

        IDiamond.FacetCut[] memory diamondCut = new IDiamond.FacetCut[](2);

        diamondCut[0] = (
            IDiamond.FacetCut({
                facetAddress: address(depManager),
                action: IDiamond.FacetCutAction.Add,
                functionSelectors: gwManagerSelectors
            })
        );

        diamondCut[1] = (
            IDiamond.FacetCut({
                facetAddress: address(depGetter),
                action: IDiamond.FacetCutAction.Add,
                functionSelectors: gwGetterSelectors
            })
        );

        GatewayDiamond dep = new GatewayDiamond(diamondCut, constructorParams);
        depGetter = dep.getter();
        depManager = dep.manager();

        SubnetID memory networkName = depGetter.getNetworkName();

        require(networkName.isRoot() == false, "unexpected networkName");
        require(depGetter.bottomUpCheckPeriod() == checkpointPeriod, "unexpected bottomUpCheckPeriod");
        require(depGetter.majorityPercentage() == 100, "unexpected majorityPercentage");
    }

    function testGatewayDiamond_Register_Works_SingleSubnet(uint256 subnetCollateral) public {
        vm.assume(subnetCollateral < type(uint64).max);
        address subnetAddress = address(new SubnetWithNativeTokenMock());
        vm.prank(subnetAddress);
        vm.deal(subnetAddress, subnetCollateral);

        registerSubnet(subnetCollateral, subnetAddress);
        require(gatewayDiamond.getter().totalSubnets() == 1, "unexpected totalSubnets");
        Subnet[] memory subnets = gatewayDiamond.getter().listSubnets();
        require(subnets.length == 1, "unexpected subnets length");

        SubnetID memory subnetId = gatewayDiamond.getter().getNetworkName().createSubnetId(subnetAddress);

        (bool ok, Subnet memory targetSubnet) = gatewayDiamond.getter().getSubnet(subnetId);

        require(ok, "subnet not found");

        (SubnetID memory id, uint256 stake, , , ) = getSubnet(subnetAddress);

        require(targetSubnet.stake == stake, "unexpected stake");
        require(targetSubnet.stake == subnetCollateral, "unexpected collateral");
        require(id.equals(subnetId), "unexpected id");
    }

    function testGatewayDiamond_Register_Works_MultipleSubnets(uint8 numberOfSubnets) public {
        vm.assume(numberOfSubnets > 0);

        for (uint256 i = 1; i <= numberOfSubnets; i++) {
            address subnetAddress = address(new SubnetWithNativeTokenMock());
            vm.prank(subnetAddress);
            vm.deal(subnetAddress, DEFAULT_COLLATERAL_AMOUNT);

            registerSubnet(DEFAULT_COLLATERAL_AMOUNT, subnetAddress);
        }

        require(gatewayDiamond.getter().totalSubnets() == numberOfSubnets, "unexpected total subnets");
        Subnet[] memory subnets = gatewayDiamond.getter().listSubnets();
        require(subnets.length == numberOfSubnets, "unexpected length");
    }

    function testGatewayDiamond_Register_Fail_SubnetAlreadyExists() public {
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, address(this));

        vm.expectRevert(AlreadyRegisteredSubnet.selector);

        gatewayDiamond.manager().register{value: DEFAULT_COLLATERAL_AMOUNT}(0, DEFAULT_COLLATERAL_AMOUNT);
    }

    function testGatewayDiamond_AddStake_Works_SingleStaking(uint256 stakeAmount, uint256 registerAmount) public {
        address subnetAddress = address(new SubnetWithNativeTokenMock());
        vm.assume(registerAmount < type(uint64).max);
        vm.assume(stakeAmount > 0 && stakeAmount < type(uint256).max - registerAmount);

        uint256 totalAmount = stakeAmount + registerAmount;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, totalAmount);

        registerSubnet(registerAmount, subnetAddress);
        addStake(stakeAmount, subnetAddress);

        (, uint256 totalStaked, , , ) = getSubnet(subnetAddress);

        require(totalStaked == totalAmount, "unexpected staked amount");
    }

    function testGatewayDiamond_AddStake_Works_Reactivate() public {
        address subnetAddress = address(new SubnetWithNativeTokenMock());
        uint256 registerAmount = DEFAULT_COLLATERAL_AMOUNT;
        uint256 stakeAmount = DEFAULT_COLLATERAL_AMOUNT;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, registerAmount);

        registerSubnet(registerAmount, subnetAddress);
        gatewayDiamond.manager().releaseStake(registerAmount);

        vm.deal(subnetAddress, stakeAmount);
        addStake(stakeAmount, subnetAddress);

        (, uint256 staked, , , ) = getSubnet(subnetAddress);

        require(staked == stakeAmount, "unexpected amount");
    }

    function testGatewayDiamond_AddStake_Works_NotEnoughFundsToReactivate() public {
        address subnetAddress = address(new SubnetWithNativeTokenMock());
        uint256 registerAmount = DEFAULT_COLLATERAL_AMOUNT;
        uint256 stakeAmount = DEFAULT_COLLATERAL_AMOUNT - 1;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, registerAmount);

        registerSubnet(registerAmount, subnetAddress);
        gatewayDiamond.manager().releaseStake(registerAmount);

        vm.deal(subnetAddress, stakeAmount);
        addStake(stakeAmount, subnetAddress);

        (, uint256 staked, , , ) = getSubnet(subnetAddress);

        require(staked == stakeAmount, "unexpected amount");
    }

    function testGatewayDiamond_AddStake_Works_MultipleStakings(uint8 numberOfStakes) public {
        vm.assume(numberOfStakes > 0);

        address subnetAddress = address(new SubnetWithNativeTokenMock());
        uint256 singleStakeAmount = 1 ether;
        uint256 registerAmount = DEFAULT_COLLATERAL_AMOUNT;
        uint256 expectedStakedAmount = registerAmount;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, registerAmount + singleStakeAmount * numberOfStakes);

        registerSubnet(registerAmount, subnetAddress);

        for (uint256 i = 0; i < numberOfStakes; i++) {
            addStake(singleStakeAmount, subnetAddress);

            expectedStakedAmount += singleStakeAmount;
        }

        (, uint256 totalStake, , , ) = getSubnet(subnetAddress);

        require(totalStake == expectedStakedAmount, "unexpected stake");
    }

    function testGatewayDiamond_AddStake_Fail_ZeroAmount() public {
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, address(this));

        vm.expectRevert(NotEnoughFunds.selector);

        gatewayDiamond.manager().addStake{value: 0}(0);
    }

    function testGatewayDiamond_AddStake_Fail_SubnetNotExists() public {
        vm.expectRevert(NotRegisteredSubnet.selector);

        gatewayDiamond.manager().addStake{value: 1}(1);
    }

    function testGatewayDiamond_ReleaseStake_Works_FullAmount(uint256 stakeAmount) public {
        address subnetAddress = CHILD_NETWORK_ADDRESS;
        uint256 registerAmount = DEFAULT_COLLATERAL_AMOUNT;

        vm.assume(stakeAmount > 0 && stakeAmount < type(uint256).max - registerAmount);

        uint256 fullAmount = stakeAmount + registerAmount;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, fullAmount);

        registerSubnet(registerAmount, subnetAddress);
        addStake(stakeAmount, subnetAddress);

        gatewayDiamond.manager().releaseStake(fullAmount);

        (, uint256 stake, , , ) = getSubnet(subnetAddress);

        require(stake == 0, "unexpected stake");
        require(subnetAddress.balance == fullAmount, "unexpected balance");
    }

    function testGatewayDiamond_ReleaseStake_Works_SubnetInactive() public {
        address subnetAddress = address(new SubnetWithNativeTokenMock());
        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, DEFAULT_COLLATERAL_AMOUNT);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, subnetAddress);

        gatewayDiamond.manager().releaseStake(DEFAULT_COLLATERAL_AMOUNT / 2);

        (, uint256 stake, , , ) = getSubnet(subnetAddress);
        require(stake == DEFAULT_COLLATERAL_AMOUNT / 2, "unexpected stake");
    }

    function testGatewayDiamond_ReleaseStake_Works_PartialAmount(uint256 partialAmount) public {
        address subnetAddress = CHILD_NETWORK_ADDRESS;
        uint256 registerAmount = DEFAULT_COLLATERAL_AMOUNT;

        vm.assume(partialAmount > registerAmount && partialAmount < type(uint256).max - registerAmount);

        uint256 totalAmount = partialAmount + registerAmount;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, totalAmount);

        registerSubnet(registerAmount, subnetAddress);
        addStake(partialAmount, subnetAddress);

        gatewayDiamond.manager().releaseStake(partialAmount);

        (, uint256 stake, , , ) = getSubnet(subnetAddress);

        require(stake == registerAmount, "unexpected stake");
        require(subnetAddress.balance == partialAmount, "unexpected balance");
    }

    function testGatewayDiamond_ReleaseStake_Fail_ZeroAmount() public {
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, address(this));

        vm.expectRevert(CannotReleaseZero.selector);

        gatewayDiamond.manager().releaseStake(0);
    }

    function testGatewayDiamond_ReleaseStake_Fail_InsufficientSubnetBalance(
        uint256 releaseAmount,
        uint256 subnetBalance
    ) public {
        vm.assume(subnetBalance > DEFAULT_COLLATERAL_AMOUNT);
        vm.assume(releaseAmount > subnetBalance && releaseAmount < type(uint256).max - subnetBalance);

        address subnetAddress = address(new SubnetWithNativeTokenMock());
        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, releaseAmount);

        registerSubnet(subnetBalance, subnetAddress);

        vm.expectRevert(NotEnoughFundsToRelease.selector);

        gatewayDiamond.manager().releaseStake(releaseAmount);
    }

    function testGatewayDiamond_ReleaseStake_Fail_NotRegisteredSubnet() public {
        vm.expectRevert(NotRegisteredSubnet.selector);

        gatewayDiamond.manager().releaseStake(1);
    }

    function testGatewayDiamond_ReleaseStake_Works_TransitionToInactive() public {
        address subnetAddress = address(new SubnetWithNativeTokenMock());

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, DEFAULT_COLLATERAL_AMOUNT);

        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, subnetAddress);

        gatewayDiamond.manager().releaseStake(10);

        (, uint256 stake, , , ) = getSubnet(subnetAddress);

        require(stake == DEFAULT_COLLATERAL_AMOUNT - 10, "unexpected stake");
    }

    function testGatewayDiamond_Kill_Works() public {
        address subnetAddress = CHILD_NETWORK_ADDRESS;

        vm.startPrank(subnetAddress);
        vm.deal(subnetAddress, DEFAULT_COLLATERAL_AMOUNT);

        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, subnetAddress);

        require(subnetAddress.balance == 0, "unexpected balance");

        gatewayDiamond.manager().kill();

        (SubnetID memory id, uint256 stake, uint256 nonce, , uint256 circSupply) = getSubnet(subnetAddress);

        require(id.toHash() == SubnetID(0, new address[](0)).toHash(), "unexpected ID hash");
        require(stake == 0, "unexpected stake");
        require(nonce == 0, "unexpected nonce");
        require(circSupply == 0, "unexpected circSupply");
        require(gatewayDiamond.getter().totalSubnets() == 0, "unexpected total subnets");
        require(subnetAddress.balance == DEFAULT_COLLATERAL_AMOUNT, "unexpected balance");
        require(gatewayDiamond.getter().listSubnets().length == 0, "unexpected number of subnets");
        require(gatewayDiamond.getter().getSubnetKeys().length == 0, "unexpected number of subnet keys");
    }

    function testGatewayDiamond_Kill_Fail_SubnetNotExists() public {
        vm.expectRevert(NotRegisteredSubnet.selector);

        gatewayDiamond.manager().kill();
    }

    function testGatewayDiamond_SendCrossMessage_Fails_NoFunds() public {
        address caller = address(new MockIpcContract());
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);

        SubnetID memory destinationSubnet = gatewayDiamond.getter().getNetworkName().createSubnetId(caller);

        vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.Value));
        gatewayDiamond.messenger().sendContractXnetMessage{value: DEFAULT_CROSS_MSG_FEE}(
            TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                IPCAddress({subnetId: destinationSubnet, rawAddress: FvmAddressHelper.from(caller)}),
                1,
                0
            )
        );
    }

    function testGatewayDiamond_SendCrossMessage_Fails_Fuzz(uint256 fee) public {
        vm.assume(fee < DEFAULT_CROSS_MSG_FEE);

        address caller = CHILD_NETWORK_ADDRESS;
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);
        vm.prank(caller);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);

        vm.expectRevert();
        gatewayDiamond.messenger().sendContractXnetMessage{value: fee - 1}(
            TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                IPCAddress({
                    subnetId: SubnetID({root: 0, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                1,
                0
            )
        );
    }

    function testGatewayDiamond_Single_Funding() public {
        (address validatorAddress, , bytes memory publicKey) = TestUtils.newValidator(100);

        join(validatorAddress, publicKey);

        address funderAddress = address(101);
        uint256 fundAmount = 1 ether;

        vm.deal(funderAddress, fundAmount + 1);

        vm.prank(funderAddress);
        fund(funderAddress, fundAmount);
    }

    function testGatewayDiamond_Fund_Kill_Fail_CircSupplyMoreThanZero() public {
        (address validatorAddress, bytes memory publicKey) = TestUtils.deriveValidatorAddress(100);

        join(validatorAddress, publicKey);

        address funderAddress = address(101);
        uint256 fundAmount = 1 ether;

        vm.deal(funderAddress, fundAmount + 1);

        vm.startPrank(funderAddress);
        fund(funderAddress, fundAmount);
        vm.stopPrank();

        vm.startPrank(address(saDiamond));
        vm.expectRevert(NotEmptySubnetCircSupply.selector);
        gatewayDiamond.manager().kill();
    }

    function testGatewayDiamond_Fund_Revert_OnZeroValue() public {
        (address validatorAddress, bytes memory publicKey) = TestUtils.deriveValidatorAddress(100);
        join(validatorAddress, publicKey);

        address funderAddress = address(101);

        (SubnetID memory subnetId, , , , ) = getSubnet(address(saDiamond));

        vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.Value));
        gatewayDiamond.manager().fund{value: 0}(subnetId, FvmAddressHelper.from(funderAddress));
    }

    function testGatewayDiamond_Fund_Works_MultipleFundings(uint8 numberOfFunds) public {
        vm.assume(numberOfFunds > 10);
        vm.assume(numberOfFunds < 50);

        uint256 fundAmount = 1 ether;

        address funderAddress = address(101);

        (address validatorAddress, bytes memory publicKey) = TestUtils.deriveValidatorAddress(100);
        join(validatorAddress, publicKey);

        vm.startPrank(funderAddress);
        for (uint256 i = 0; i < numberOfFunds; i++) {
            vm.deal(funderAddress, fundAmount + 1);
            fund(funderAddress, fundAmount);
        }
    }

    function testGatewayDiamond_Fund_Fuzz_InsufficientAmount(uint256 amount) public {
        vm.assume(amount > 0);
        vm.assume(amount < DEFAULT_COLLATERAL_AMOUNT);

        address funderAddress = address(101);

        (address validatorAddress, bytes memory publicKey) = TestUtils.deriveValidatorAddress(100);
        join(validatorAddress, publicKey);

        vm.deal(funderAddress, amount);

        (SubnetID memory subnetId, , , , ) = getSubnet(address(saDiamond));
        vm.prank(funderAddress);
        gatewayDiamond.manager().fund{value: amount}(subnetId, FvmAddressHelper.from(msg.sender));
    }

    function testGatewayDiamond_Fund_Fails_NotRegistered() public {
        address funderAddress = address(101);
        uint256 fundAmount = 1 ether;

        (address validatorAddress, bytes memory publicKey) = TestUtils.deriveValidatorAddress(100);
        join(validatorAddress, publicKey);

        address[] memory wrongSubnetPath = new address[](2);
        wrongSubnetPath[0] = vm.addr(102);
        wrongSubnetPath[0] = vm.addr(103);

        address[] memory wrongPath = new address[](3);
        wrongPath[0] = address(1);
        wrongPath[1] = address(2);

        vm.deal(funderAddress, fundAmount + 1);

        vm.startPrank(funderAddress);

        SubnetID memory wrongSubnetId = SubnetID({root: ROOTNET_CHAINID, route: wrongSubnetPath});

        vm.expectRevert(NotRegisteredSubnet.selector);
        gatewayDiamond.manager().fund{value: fundAmount}(wrongSubnetId, FvmAddressHelper.from(msg.sender));

        vm.expectRevert(NotRegisteredSubnet.selector);
        gatewayDiamond.manager().fund{value: fundAmount}(
            SubnetID(ROOTNET_CHAINID, wrongPath),
            FvmAddressHelper.from(msg.sender)
        );
    }

    function testGatewayDiamond_Fund_Works_BLSAccountSingleFunding() public {
        (address validatorAddress, bytes memory publicKey) = TestUtils.deriveValidatorAddress(100);
        join(validatorAddress, publicKey);

        uint256 fundAmount = 1 ether;
        vm.deal(BLS_ACCOUNT_ADDREESS, fundAmount + 1);
        vm.startPrank(BLS_ACCOUNT_ADDREESS);

        fund(BLS_ACCOUNT_ADDREESS, fundAmount);
    }

    function testGatewayDiamond_Fund_Works_ReactivatedSubnet() public {
        (address validatorAddress, uint256 privKey, bytes memory publicKey) = TestUtils.newValidator(100);
        assert(validatorAddress == vm.addr(privKey));

        join(validatorAddress, publicKey);

        vm.prank(validatorAddress);
        saDiamond.manager().leave();

        join(validatorAddress, publicKey);

        address funderAddress = address(101);
        uint256 fundAmount = 1 ether;

        vm.deal(funderAddress, fundAmount + 1);
        fund(funderAddress, fundAmount);
    }

    function testGatewayDiamond_Release_Fails_InsufficientAmount() public {
        address[] memory path = new address[](2);
        path[0] = address(1);
        path[1] = address(2);

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });
        gatewayDiamond = createGatewayDiamond(constructorParams);

        address callerAddress = address(100);

        vm.startPrank(callerAddress);
        vm.deal(callerAddress, 1 ether);
        vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.Value));

        gatewayDiamond.manager().release{value: 0 ether}(FvmAddressHelper.from(msg.sender));
    }

    function testGatewayDiamond_Release_Works_BLSAccount(uint256 releaseAmount, uint256 crossMsgFee) public {
        vm.assume(crossMsgFee >= DEFAULT_CROSS_MSG_FEE);
        vm.assume(releaseAmount < type(uint256).max);
        vm.assume(crossMsgFee > 0 && crossMsgFee < releaseAmount);

        address[] memory path = new address[](2);
        path[0] = makeAddr("root");
        path[1] = makeAddr("subnet_one");

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });

        gatewayDiamond = createGatewayDiamond(constructorParams);

        vm.roll(0);
        vm.warp(0);
        vm.startPrank(BLS_ACCOUNT_ADDREESS);
        vm.deal(BLS_ACCOUNT_ADDREESS, releaseAmount + 1);
        release(releaseAmount);
    }

    function testGatewayDiamond_Release_Works_EmptyCrossMsgMeta(uint256 releaseAmount, uint256 crossMsgFee) public {
        vm.assume(crossMsgFee >= DEFAULT_CROSS_MSG_FEE);
        vm.assume(releaseAmount < type(uint256).max);
        vm.assume(crossMsgFee > 0 && crossMsgFee < releaseAmount);

        address[] memory path = new address[](2);
        path[0] = makeAddr("root");
        path[1] = makeAddr("subnet_one");

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });

        gatewayDiamond = createGatewayDiamond(constructorParams);
        address callerAddress = address(100);

        vm.roll(0);
        vm.warp(0);
        vm.startPrank(callerAddress);
        vm.deal(callerAddress, releaseAmount + 1);
        release(releaseAmount);
    }

    function testGatewayDiamond_Release_Works_NonEmptyCrossMsgMeta(uint256 releaseAmount, uint256 crossMsgFee) public {
        vm.assume(crossMsgFee >= DEFAULT_CROSS_MSG_FEE);
        vm.assume(releaseAmount < type(uint256).max / 2);
        vm.assume(crossMsgFee > 0 && crossMsgFee < releaseAmount);

        address[] memory path = new address[](2);
        path[0] = makeAddr("root");
        path[1] = makeAddr("subnet_one");

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });

        gatewayDiamond = createGatewayDiamond(constructorParams);

        address callerAddress = address(100);

        vm.roll(0);
        vm.warp(0);
        vm.startPrank(callerAddress);
        vm.deal(callerAddress, 2 * releaseAmount + 1);

        release(releaseAmount);
        release(releaseAmount);
    }

    function testGatewayDiamond_SendCrossMessage_Fails_NoDestination() public {
        address caller = address(new MockIpcContract());
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);

        vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.DstSubnet));
        gatewayDiamond.messenger().sendContractXnetMessage{value: 1}(
            TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                IPCAddress({
                    subnetId: SubnetID({root: 0, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                1,
                0
            )
        );
    }

    function testGatewayDiamond_SendCrossMessage_Fails_NoCurrentNetwork() public {
        address caller = address(new MockIpcContract());
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);
        SubnetID memory destinationSubnet = gatewayDiamond.getter().getNetworkName();

        vm.expectRevert(CannotSendCrossMsgToItself.selector);
        gatewayDiamond.messenger().sendContractXnetMessage{value: 1}(
            TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                IPCAddress({subnetId: destinationSubnet, rawAddress: FvmAddressHelper.from(caller)}),
                1,
                0
            )
        );
    }

    function testGatewayDiamond_SendCrossMessage_Fails_Failes_InvalidCrossMsgValue() public {
        address caller = address(new MockIpcContract());
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);
        SubnetID memory destinationSubnet = gatewayDiamond.getter().getNetworkName().createSubnetId(caller);

        vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.Value));
        gatewayDiamond.messenger().sendContractXnetMessage{value: DEFAULT_CROSS_MSG_FEE}(
            TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                IPCAddress({subnetId: destinationSubnet, rawAddress: FvmAddressHelper.from(caller)}),
                5,
                0
            )
        );
    }

    // TODO: this is no longer possible because EOA cannot be subnet
    // function testGatewayDiamond_SendCrossMessage_Fails_EoACaller() public {
    //     address caller = vm.addr(100);
    //     vm.startPrank(caller);
    //     vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);

    //     registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);

    //     SubnetID memory destinationSubnet = SubnetID(0, new address[](0));
    //     vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.Sender));

    //     gatewayDiamond.messenger().sendContractXnetMessage{value: DEFAULT_CROSS_MSG_FEE}(
    //         TestUtils.newXnetCallMsg(
    //             IPCAddress({
    //                 subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
    //                 rawAddress: FvmAddressHelper.from(caller)
    //             }),
    //             IPCAddress({subnetId: destinationSubnet, rawAddress: FvmAddressHelper.from(caller)}),
    //             1,
    //             0
    //         )
    //     );
    // }

    function testGatewayDiamond_SendCrossMessage_Fails_EmptyNetwork() public {
        // Caller of general-purpose messages must be a contract, not a EoA
        address caller = address(new MockIpcContract());
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE + 2);

        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);

        SubnetID memory destinationSubnet = SubnetID(0, new address[](0));

        vm.expectRevert(abi.encodeWithSelector(InvalidXnetMessage.selector, InvalidXnetMessageReason.DstSubnet));

        gatewayDiamond.messenger().sendContractXnetMessage{value: 1}(
            TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                IPCAddress({subnetId: destinationSubnet, rawAddress: FvmAddressHelper.from(caller)}),
                1,
                0
            )
        );
    }

    function testGatewayDiamond_CommitParentFinality_Fails_NotSystemActor() public {
        address caller = vm.addr(100);

        FvmAddress[] memory validators = new FvmAddress[](1);
        validators[0] = FvmAddressHelper.from(caller);
        uint256[] memory weights = new uint256[](1);
        weights[0] = 100;

        vm.prank(caller);
        vm.expectRevert(NotSystemActor.selector);

        ParentFinality memory finality = ParentFinality({height: block.number, blockHash: bytes32(0)});

        gatewayDiamond.topDownFinalizer().commitParentFinality(finality);
    }

    function testGatewayDiamond_applyFinality_works() public {
        address val0 = vm.addr(501);
        Validator[] memory genesisValidators = new Validator[](1);
        genesisValidators[0] = Validator({
            weight: 200,
            addr: val0,
            metadata: hex"04851c1bda327584479e98a7c28ea7adc097d290efd105310bcf714231bb99faf4295ccc3f24785678942b593be13cbe6c878a3611049c95e86ce1d5af19e6015e"
        });

        // create a new diamond

        GatewayDiamond.ConstructorParams memory gwConstructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: new address[](0)}),
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: genesisValidators,
            activeValidatorsLimit: DEFAULT_ACTIVE_VALIDATORS_LIMIT,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });
        gatewayDiamond = createGatewayDiamond(gwConstructorParams);

        require(
            gatewayDiamond.getter().getCurrentMembership().validators.length == 1,
            "current membership should be 1"
        );
        require(gatewayDiamond.getter().getCurrentConfigurationNumber() == 0, "unexpected config number");

        // changes included for two validators joining
        address val1 = vm.addr(100);
        address val2 = vm.addr(101);
        uint256 amount = 10000;
        StakingChangeRequest[] memory changes = new StakingChangeRequest[](2);

        changes[0] = StakingChangeRequest({
            configurationNumber: 1,
            change: StakingChange({validator: val1, op: StakingOperation.Deposit, payload: abi.encode(amount)})
        });
        changes[1] = StakingChangeRequest({
            configurationNumber: 2,
            change: StakingChange({validator: val2, op: StakingOperation.Deposit, payload: abi.encode(amount)})
        });

        vm.startPrank(FilAddress.SYSTEM_ACTOR);

        gatewayDiamond.topDownFinalizer().storeValidatorChanges(changes);
        uint64 configNumber = gatewayDiamond.topDownFinalizer().applyFinalityChanges();
        require(configNumber == 2, "wrong config number after applying finality");
        require(
            gatewayDiamond.getter().getCurrentMembership().validators.length == 3,
            "current membership should be 3"
        );
        require(gatewayDiamond.getter().getCurrentConfigurationNumber() == 2, "unexpected config number");
        require(gatewayDiamond.getter().getLastConfigurationNumber() == 0, "unexpected last config number");

        vm.stopPrank();

        // new change with a validator leaving
        changes = new StakingChangeRequest[](1);

        changes[0] = StakingChangeRequest({
            configurationNumber: 3,
            change: StakingChange({validator: val1, op: StakingOperation.Withdraw, payload: abi.encode(amount)})
        });

        vm.startPrank(FilAddress.SYSTEM_ACTOR);

        gatewayDiamond.topDownFinalizer().storeValidatorChanges(changes);
        configNumber = gatewayDiamond.topDownFinalizer().applyFinalityChanges();
        require(configNumber == 3, "wrong config number after applying finality");
        require(
            gatewayDiamond.getter().getLastConfigurationNumber() == 2,
            "apply result: unexpected last config number"
        );
        require(gatewayDiamond.getter().getCurrentConfigurationNumber() == 3, "apply result: unexpected config number");
        require(
            gatewayDiamond.getter().getCurrentMembership().validators.length == 2,
            "current membership should be 2"
        );
        require(gatewayDiamond.getter().getLastMembership().validators.length == 3, "last membership should be 3");

        // no changes
        configNumber = gatewayDiamond.topDownFinalizer().applyFinalityChanges();
        require(configNumber == 0, "wrong config number after applying finality");
        require(gatewayDiamond.getter().getLastConfigurationNumber() == 2, "no changes: unexpected last config number");
        require(gatewayDiamond.getter().getCurrentConfigurationNumber() == 3, "no changes: unexpected config number");
        require(
            gatewayDiamond.getter().getCurrentMembership().validators.length == 2,
            "current membership should be 2"
        );
        require(gatewayDiamond.getter().getLastMembership().validators.length == 3, "last membership should be 3");

        // new change with a validator joining
        changes = new StakingChangeRequest[](2);
        address val3 = vm.addr(505);
        bytes
            memory pubKey = hex"04e327a66b169732bde49d827d90781327af558fc12d5cd2d5004e7551ec00c66278be40fa222dd6f5942adad5ea9509cf1299b87c0ed8ca5a3fcc054f07a502fc";

        changes[0] = StakingChangeRequest({
            configurationNumber: 4,
            change: StakingChange({validator: val3, op: StakingOperation.SetMetadata, payload: pubKey})
        });
        changes[1] = StakingChangeRequest({
            configurationNumber: 5,
            change: StakingChange({validator: val3, op: StakingOperation.Deposit, payload: abi.encode(amount)})
        });

        vm.startPrank(FilAddress.SYSTEM_ACTOR);

        gatewayDiamond.topDownFinalizer().storeValidatorChanges(changes);
        configNumber = gatewayDiamond.topDownFinalizer().applyFinalityChanges();
        require(configNumber == 5, "wrong config number after applying finality");
        require(
            gatewayDiamond.getter().getLastConfigurationNumber() == 3,
            "apply result: unexpected last config number"
        );
        require(gatewayDiamond.getter().getCurrentConfigurationNumber() == 5, "apply result: unexpected config number");
        require(
            gatewayDiamond.getter().getCurrentMembership().validators.length == 3,
            "current membership should be 3"
        );
        require(gatewayDiamond.getter().getLastMembership().validators.length == 2, "last membership should be 2");

        vm.stopPrank();
    }

    function testGatewayDiamond_CommitParentFinality_Works_WithQuery() public {
        FvmAddress[] memory validators = new FvmAddress[](2);
        validators[0] = FvmAddressHelper.from(vm.addr(100));
        validators[1] = FvmAddressHelper.from(vm.addr(101));
        uint256[] memory weights = new uint256[](2);
        weights[0] = 100;
        weights[1] = 150;

        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        // increase the block number so that current block number is
        // not the same as init committed parent finality height
        vm.roll(10);

        ParentFinality memory finality = ParentFinality({height: block.number, blockHash: bytes32(0)});

        gatewayDiamond.topDownFinalizer().commitParentFinality(finality);
        ParentFinality memory committedFinality = gatewayDiamond.getter().getParentFinality(block.number);

        require(committedFinality.height == finality.height, "heights are not equal");
        require(committedFinality.blockHash == finality.blockHash, "blockHash is not equal");
        require(gatewayDiamond.getter().getLatestParentFinality().height == block.number, "finality height not equal");

        vm.stopPrank();
    }

    function testGatewayDiamond_createBottomUpCheckpoint() public {
        (, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, ) = MerkleTreeHelper.createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory old = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: 0,
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // failed to create a checkpoint with zero membership weight
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        vm.expectRevert(ZeroMembershipWeight.selector);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            0,
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // failed create a processed checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        vm.expectRevert(QuorumAlreadyProcessed.selector);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            old,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        BottomUpCheckpoint memory recv = gatewayDiamond.getter().bottomUpCheckpoint(
            gatewayDiamond.getter().bottomUpCheckPeriod()
        );
        require(recv.nextConfigurationNumber == 1, "nextConfigurationNumber incorrect");
        require(recv.blockHash == keccak256("block1"), "block hash incorrect");
        uint256 d = gatewayDiamond.getter().bottomUpCheckPeriod();

        // failed to create a checkpoint with the same height
        checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: d,
            blockHash: keccak256("block"),
            nextConfigurationNumber: 2,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        vm.expectRevert(CheckpointAlreadyExists.selector);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        (bool ok, uint256 e, ) = gatewayDiamond.getter().getCurrentBottomUpCheckpoint();
        require(ok, "checkpoint not exist");
        require(e == d, "out height incorrect");
    }

    function testGatewayDiamond_commitBottomUpCheckpoint_InvalidCheckpointSource() public {
        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        vm.expectRevert(InvalidCheckpointSource.selector);
        gatewayDiamond.checkpointer().commitCheckpoint(checkpoint);
    }

    function testGatewayDiamond_commitBottomUpCheckpoint_Works_NoMessages() public {
        address caller = address(saDiamond);
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);
        vm.stopPrank();

        (SubnetID memory subnetId, , , , ) = getSubnet(address(caller));

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: subnetId,
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        vm.prank(caller);
        gatewayDiamond.checkpointer().commitCheckpoint(checkpoint);
    }

    function testGatewayDiamond_commitBottomUpCheckpoint_Works_WithMessages() public {
        address caller = address(saDiamond);
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);
        vm.stopPrank();

        uint256 amount = 1;

        (SubnetID memory subnetId, , , , ) = getSubnet(address(caller));
        (bool exist, Subnet memory subnetInfo) = gatewayDiamond.getter().getSubnet(subnetId);
        require(exist, "subnet does not exist");
        require(subnetInfo.circSupply == 0, "unexpected initial circulation supply");

        gatewayDiamond.manager().fund{value: DEFAULT_COLLATERAL_AMOUNT}(
            subnetId,
            FvmAddressHelper.from(address(caller))
        );
        (, subnetInfo) = gatewayDiamond.getter().getSubnet(subnetId);
        require(subnetInfo.circSupply == DEFAULT_COLLATERAL_AMOUNT, "unexpected circulation supply after funding");

        IpcEnvelope[] memory msgs = new IpcEnvelope[](10);
        for (uint64 i = 0; i < 10; i++) {
            msgs[i] = TestUtils.newXnetCallMsg(
                IPCAddress({subnetId: subnetId, rawAddress: FvmAddressHelper.from(caller)}),
                IPCAddress({
                    subnetId: gatewayDiamond.getter().getNetworkName(),
                    rawAddress: FvmAddressHelper.from(vm.addr(100 + i))
                }),
                amount,
                i
            );
        }

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: subnetId,
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: msgs,
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        vm.prank(caller);
        gatewayDiamond.checkpointer().commitCheckpoint(checkpoint);

        (, subnetInfo) = gatewayDiamond.getter().getSubnet(subnetId);
        require(subnetInfo.circSupply == DEFAULT_COLLATERAL_AMOUNT - 10 * amount, "unexpected circulating supply");
    }

    function testGatewayDiamond_listIncompleteCheckpoints() public {
        (, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, ) = MerkleTreeHelper.createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint1 = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        BottomUpCheckpoint memory checkpoint2 = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: 2 * gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block2"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint1,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint2,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        uint256[] memory heights = gatewayDiamond.getter().getIncompleteCheckpointHeights();

        require(heights.length == 2, "unexpected heights");
        require(heights[0] == gatewayDiamond.getter().bottomUpCheckPeriod(), "heights[0] == period");
        require(heights[1] == 2 * gatewayDiamond.getter().bottomUpCheckPeriod(), "heights[1] == 2*period");

        QuorumInfo memory info = gatewayDiamond.getter().getCheckpointInfo(
            gatewayDiamond.getter().bottomUpCheckPeriod()
        );
        require(info.rootHash == membershipRoot, "info.rootHash == membershipRoot");
        require(
            info.threshold == gatewayDiamond.getter().getQuorumThreshold(weights[0] + weights[1] + weights[2]),
            "checkpoint 1 correct threshold"
        );

        info = gatewayDiamond.getter().getCheckpointInfo(2 * gatewayDiamond.getter().bottomUpCheckPeriod());
        require(info.rootHash == membershipRoot, "info.rootHash == membershipRoot");
        require(
            info.threshold == gatewayDiamond.getter().getQuorumThreshold(weights[0] + weights[1] + weights[2]),
            "checkpoint 2 correct threshold"
        );

        BottomUpCheckpoint[] memory incomplete = gatewayDiamond.getter().getIncompleteCheckpoints();
        require(incomplete.length == 2, "incomplete.length == 2");
        require(
            incomplete[0].blockHeight == gatewayDiamond.getter().bottomUpCheckPeriod(),
            "incomplete[0].blockHeight"
        );
        require(incomplete[0].blockHash == keccak256("block1"), "incomplete[0].blockHash");
        require(
            incomplete[1].blockHeight == 2 * gatewayDiamond.getter().bottomUpCheckPeriod(),
            "incomplete[1].blockHeight"
        );
        require(incomplete[1].blockHash == keccak256("block2"), "incomplete[1].blockHash");
    }

    function testGatewayDiamond_addCheckpointSignature_newCheckpoint() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // adds signatures

        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        for (uint64 i = 0; i < 3; i++) {
            (v, r, s) = vm.sign(privKeys[i], keccak256(abi.encode(checkpoint)));
            signature = abi.encodePacked(r, s, v);

            vm.startPrank(vm.addr(privKeys[i]));
            gatewayDiamond.checkpointer().addCheckpointSignature(
                checkpoint.blockHeight,
                membershipProofs[i],
                weights[i],
                signature
            );
            vm.stopPrank();
        }

        require(
            gatewayDiamond.getter().getCheckpointCurrentWeight(checkpoint.blockHeight) == totalWeight(weights),
            "checkpoint weight was not updated"
        );

        (
            BottomUpCheckpoint memory ch,
            QuorumInfo memory info,
            address[] memory signatories,
            bytes[] memory signatures
        ) = gatewayDiamond.getter().getCheckpointSignatureBundle(gatewayDiamond.getter().bottomUpCheckPeriod());
        require(ch.blockHash == keccak256("block"), "unexpected block hash");
        require(info.hash == keccak256(abi.encode(checkpoint)), "unexpected checkpoint hash");
        require(signatories.length == 3, "unexpected signatories length");
        require(signatures.length == 3, "unexpected signatures length");
    }

    function testGatewayDiamond_addCheckpointSignature_quorum() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // adds signatures

        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        for (uint64 i = 0; i < 2; i++) {
            (v, r, s) = vm.sign(privKeys[i], keccak256(abi.encode(checkpoint)));
            signature = abi.encodePacked(r, s, v);

            vm.startPrank(vm.addr(privKeys[i]));
            gatewayDiamond.checkpointer().addCheckpointSignature(
                checkpoint.blockHeight,
                membershipProofs[i],
                weights[i],
                signature
            );
            vm.stopPrank();
        }

        QuorumInfo memory info = gatewayDiamond.getter().getCheckpointInfo(1);
        require(!info.reached, "not reached");
        require(gatewayDiamond.getter().getIncompleteCheckpointHeights().length == 1, "unexpected size");

        info = gatewayDiamond.getter().getCheckpointInfo(1);

        (v, r, s) = vm.sign(privKeys[2], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        vm.startPrank(vm.addr(privKeys[2]));
        gatewayDiamond.checkpointer().addCheckpointSignature(
            checkpoint.blockHeight,
            membershipProofs[2],
            weights[2],
            signature
        );
        vm.stopPrank();

        info = gatewayDiamond.getter().getCheckpointInfo(checkpoint.blockHeight);
        require(info.reached, "not reached");
        require(gatewayDiamond.getter().getIncompleteCheckpointHeights().length == 0, "unexpected size");

        require(
            gatewayDiamond.getter().getCheckpointCurrentWeight(checkpoint.blockHeight) == totalWeight(weights),
            "checkpoint weight was not updated"
        );
        (v, r, s) = vm.sign(privKeys[3], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        vm.startPrank(vm.addr(privKeys[3]));
        gatewayDiamond.checkpointer().addCheckpointSignature(
            checkpoint.blockHeight,
            membershipProofs[3],
            weights[3],
            signature
        );
        vm.stopPrank();
    }

    function testGatewayDiamond_addCheckpointSignature_notAuthorized() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            10,
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        (v, r, s) = vm.sign(privKeys[0], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        uint256 h = gatewayDiamond.getter().bottomUpCheckPeriod();
        vm.startPrank(vm.addr(privKeys[1]));
        vm.expectRevert(abi.encodeWithSelector(NotAuthorized.selector, vm.addr(privKeys[0])));
        gatewayDiamond.checkpointer().addCheckpointSignature(h, membershipProofs[2], weights[2], signature);
        vm.stopPrank();
    }

    function testGatewayDiamond_addCheckpointSignature_invalidSignature_replayedSignature() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            10,
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        (v, r, s) = vm.sign(privKeys[0], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        uint256 h = gatewayDiamond.getter().bottomUpCheckPeriod();
        vm.startPrank(vm.addr(privKeys[0]));

        // send incorrect signature
        vm.expectRevert(InvalidSignature.selector);
        gatewayDiamond.checkpointer().addCheckpointSignature(h, membershipProofs[0], weights[0], new bytes(0));

        // send correct signature
        gatewayDiamond.checkpointer().addCheckpointSignature(h, membershipProofs[0], weights[0], signature);

        // replay the previous signature
        vm.expectRevert(SignatureReplay.selector);
        gatewayDiamond.checkpointer().addCheckpointSignature(h, membershipProofs[0], weights[0], signature);

        vm.stopPrank();
    }

    function testGatewayDiamond_addCheckpointSignature_incorrectCheckpoint() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            10,
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        (v, r, s) = vm.sign(privKeys[0], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        vm.startPrank(vm.addr(privKeys[0]));

        // send correct signature for incorrect height
        vm.expectRevert(QuorumAlreadyProcessed.selector);
        gatewayDiamond.checkpointer().addCheckpointSignature(0, membershipProofs[0], weights[0], signature);

        // send correct signature for incorrect height
        vm.expectRevert(CheckpointNotCreated.selector);
        gatewayDiamond.checkpointer().addCheckpointSignature(100, membershipProofs[0], weights[0], signature);

        vm.stopPrank();
    }

    function testGatewayDiamond_addBitcoinCheckpointSignature_quorum() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // adds signatures

        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        for (uint64 i = 0; i < 2; i++) {
            (v, r, s) = vm.sign(privKeys[i], keccak256(abi.encode(checkpoint)));
            signature = abi.encodePacked(r, s, v);

            vm.startPrank(vm.addr(privKeys[i]));
            gatewayDiamond.checkpointer().addCheckpointSignature(
                checkpoint.blockHeight,
                membershipProofs[i],
                weights[i],
                signature
            );

            // Add Bitcoin checkpoint signatures
            gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
                checkpoint.blockHeight,
                abi.encodePacked("psbt"),
                abi.encodePacked(string.concat("val", Strings.toString(i), "sig")),
                abi.encodePacked("batchtransfer1")
            );
            vm.stopPrank();
        }

        // Check the Bitcoin checkpoint signatures were properly added
        {
            (
                bytes memory retrievedPsbt,
                bytes memory retrievedBatchTransferTx,
                address[] memory signatories,
                bytes[] memory retrievedSignatures
            ) = gatewayDiamond.checkpointer().getBitcoinCheckpointSignatures(checkpoint.blockHeight);

            require(keccak256(retrievedPsbt) == keccak256(abi.encodePacked("psbt")), "incorrect PSBT");
            require(signatories.length == 2, "incorrect number of signatories");
            require(signatories[0] == vm.addr(privKeys[0]), "incorrect first signatory");
            require(signatories[1] == vm.addr(privKeys[1]), "incorrect second signatory");
            require(
                keccak256(retrievedSignatures[0]) == keccak256(abi.encodePacked("val0sig")),
                "incorrect first signature"
            );
            require(
                keccak256(retrievedSignatures[1]) == keccak256(abi.encodePacked("val1sig")),
                "incorrect second signature"
            );
            require(
                keccak256(retrievedBatchTransferTx) == keccak256(abi.encodePacked("batchtransfer1")),
                "incorrect batch transfer tx"
            );
        }
    }

    function testGatewayDiamond_addBitcoinCheckpointSignature_differentPSBT() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // Add regular checkpoint signatures first
        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        for (uint64 i = 0; i < 3; i++) {
            (v, r, s) = vm.sign(privKeys[i], keccak256(abi.encode(checkpoint)));
            signature = abi.encodePacked(r, s, v);

            vm.startPrank(vm.addr(privKeys[i]));
            gatewayDiamond.checkpointer().addCheckpointSignature(
                checkpoint.blockHeight,
                membershipProofs[i],
                weights[i],
                signature
            );
            vm.stopPrank();
        }

        // First validator adds a Bitcoin signature for PSBT1
        vm.startPrank(vm.addr(privKeys[0]));
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("val0sig"),
            abi.encodePacked("batchtransfer1")
        );
        vm.stopPrank();

        // Second validator adds a Bitcoin signature for PSBT1
        vm.startPrank(vm.addr(privKeys[1]));
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("val1sig"),
            abi.encodePacked("batchtransfer1")
        );
        vm.stopPrank();

        // Third validator adds a Bitcoin signature for PSBT2 (different PSBT)
        vm.startPrank(vm.addr(privKeys[2]));
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt2"),
            abi.encodePacked("val2sig"),
            abi.encodePacked("batchtransfer2")
        );
        vm.stopPrank();

        // Check both PSBTs were stored properly, with PSBT1 having more signatures
        {
            (
                bytes memory retrievedPsbt,
                bytes memory retrievedBatchTransferTx,
                address[] memory signatories,

            ) = gatewayDiamond.checkpointer().getBitcoinCheckpointSignatures(checkpoint.blockHeight);

            // Should return PSBT1 as it has 2 signatures vs 1 for PSBT2
            require(keccak256(retrievedPsbt) == keccak256(abi.encodePacked("psbt1")), "incorrect PSBT");
            require(signatories.length == 2, "incorrect number of signatories");
            require(
                keccak256(retrievedBatchTransferTx) == keccak256(abi.encodePacked("batchtransfer1")),
                "incorrect batch transfer tx"
            );
        }
    }

    function testGatewayDiamond_addBitcoinCheckpointSignature_NotAuthorized() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // First validator adds a regular checkpoint signature
        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        (v, r, s) = vm.sign(privKeys[0], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        vm.startPrank(vm.addr(privKeys[0]));
        gatewayDiamond.checkpointer().addCheckpointSignature(
            checkpoint.blockHeight,
            membershipProofs[0],
            weights[0],
            signature
        );
        vm.stopPrank();

        // Attempt to add a Bitcoin signature from a validator who hasn't signed the regular checkpoint
        vm.startPrank(vm.addr(privKeys[1]));
        vm.expectRevert(abi.encodeWithSelector(NotAuthorized.selector, vm.addr(privKeys[1])));
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("val1sig"),
            abi.encodePacked("batchtransfer1")
        );
        vm.stopPrank();

        // Try with a completely unauthorized address
        address unauthorizedAddress = address(0x1234);
        vm.startPrank(unauthorizedAddress);
        vm.expectRevert(abi.encodeWithSelector(NotAuthorized.selector, unauthorizedAddress));
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("val1sig"),
            abi.encodePacked("batchtransfer1")
        );
        vm.stopPrank();
    }

    function testGatewayDiamond_addBitcoinCheckpointSignature_UnknownCheckpoint() public {
        (uint256[] memory privKeys, , ) = TestUtils.getFourValidators(vm);

        // Try to add a Bitcoin signature for a non-existent checkpoint
        uint256 nonExistentHeight = 999;

        vm.startPrank(vm.addr(privKeys[0]));
        vm.expectRevert(CheckpointNotCreated.selector);
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            nonExistentHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("val0sig"),
            abi.encodePacked("batchtransfer1")
        );
        vm.stopPrank();
    }

    function testGatewayDiamond_addBitcoinCheckpointSignature_SignatureReplay() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        // create a checkpoint
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        // First validator adds a regular checkpoint signature
        uint8 v;
        bytes32 r;
        bytes32 s;
        bytes memory signature;

        (v, r, s) = vm.sign(privKeys[0], keccak256(abi.encode(checkpoint)));
        signature = abi.encodePacked(r, s, v);

        vm.startPrank(vm.addr(privKeys[0]));
        gatewayDiamond.checkpointer().addCheckpointSignature(
            checkpoint.blockHeight,
            membershipProofs[0],
            weights[0],
            signature
        );

        // Add Bitcoin signature
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("val0sig"),
            abi.encodePacked("batchtransfer1")
        );

        // Try to add the same Bitcoin signature again (same PSBT)
        vm.expectRevert(SignatureReplay.selector);
        gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
            checkpoint.blockHeight,
            abi.encodePacked("psbt1"),
            abi.encodePacked("new_val0sig"),
            abi.encodePacked("batchtransfer1")
        );
        vm.stopPrank();
    }

    function testGatewayDiamond_pruneBottomUpCheckpoints_PrunesBitcoinCheckpoints() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        uint256 index = gatewayDiamond.getter().getCheckpointRetentionHeight();
        require(index == 1, "unexpected initial retention height");

        // Create 5 checkpoints with heights 1-5
        for (uint64 i = 1; i <= 5; i++) {
            BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
                subnetID: gatewayDiamond.getter().getNetworkName(),
                blockHeight: i * gatewayDiamond.getter().bottomUpCheckPeriod(),
                blockHash: keccak256(abi.encodePacked("block", i)),
                nextConfigurationNumber: 1,
                slashPreSignatureCount: 0,
                msgs: new IpcEnvelope[](0),
                activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
            });

            // Create the checkpoint
            vm.startPrank(FilAddress.SYSTEM_ACTOR);
            gatewayDiamond.checkpointer().createBottomUpCheckpoint(
                checkpoint,
                membershipRoot,
                weights[0] + weights[1],
                ActivityHelper.dummyActivityRollup()
            );
            vm.stopPrank();

            // Add a regular signature so we can add a Bitcoin signature
            uint8 v;
            bytes32 r;
            bytes32 s;
            bytes memory signature;

            (v, r, s) = vm.sign(privKeys[0], keccak256(abi.encode(checkpoint)));
            signature = abi.encodePacked(r, s, v);

            vm.startPrank(vm.addr(privKeys[0]));
            gatewayDiamond.checkpointer().addCheckpointSignature(
                checkpoint.blockHeight,
                membershipProofs[0],
                weights[0],
                signature
            );

            // Add Bitcoin checkpoint signature with unique data for each checkpoint
            gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
                checkpoint.blockHeight,
                abi.encodePacked("psbt", i),
                abi.encodePacked("sig", i),
                abi.encodePacked("batchtx", i)
            );
            vm.stopPrank();
        }

        // Verify all Bitcoin checkpoints exist before pruning
        for (uint64 i = 1; i <= 5; i++) {
            uint256 height = i * gatewayDiamond.getter().bottomUpCheckPeriod();
            (
                bytes memory psbt,
                bytes memory batchTx,
                address[] memory signatories,
                bytes[] memory signatures
            ) = gatewayDiamond.checkpointer().getBitcoinCheckpointSignatures(height);

            // Verify the checkpoint data
            require(keccak256(psbt) == keccak256(abi.encodePacked("psbt", i)), "incorrect PSBT before pruning");
            require(
                keccak256(batchTx) == keccak256(abi.encodePacked("batchtx", i)),
                "incorrect batch tx before pruning"
            );
            require(signatories.length == 1, "incorrect number of signatories before pruning");
            require(signatories[0] == vm.addr(privKeys[0]), "incorrect signatory before pruning");
            require(
                keccak256(signatures[0]) == keccak256(abi.encodePacked("sig", i)),
                "incorrect signature before pruning"
            );
        }

        // Prune checkpoints with height < 3
        uint256 newRetentionHeight = 3 * gatewayDiamond.getter().bottomUpCheckPeriod();
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().pruneBottomUpCheckpoints(newRetentionHeight);
        vm.stopPrank();

        // Verify new retention height
        index = gatewayDiamond.getter().getCheckpointRetentionHeight();
        require(index == newRetentionHeight, "retention height not updated correctly");

        // Verify Bitcoin checkpoints with height < 3 are pruned
        for (uint64 i = 1; i <= 5; i++) {
            uint256 height = i * gatewayDiamond.getter().bottomUpCheckPeriod();

            if (height < newRetentionHeight) {
                // These should be pruned
                vm.expectRevert(CheckpointNotCreated.selector);
                gatewayDiamond.checkpointer().getBitcoinCheckpointSignatures(height);
            } else {
                // These should still exist
                (
                    bytes memory psbt,
                    bytes memory batchTx,
                    address[] memory signatories,
                    bytes[] memory signatures
                ) = gatewayDiamond.checkpointer().getBitcoinCheckpointSignatures(height);

                // Verify the checkpoint data is still intact
                require(keccak256(psbt) == keccak256(abi.encodePacked("psbt", i)), "incorrect PSBT after pruning");
                require(
                    keccak256(batchTx) == keccak256(abi.encodePacked("batchtx", i)),
                    "incorrect batch tx after pruning"
                );
                require(signatories.length == 1, "incorrect number of signatories after pruning");
                require(signatories[0] == vm.addr(privKeys[0]), "incorrect signatory after pruning");
                require(
                    keccak256(signatures[0]) == keccak256(abi.encodePacked("sig", i)),
                    "incorrect signature after pruning"
                );
            }
        }
    }

    function testGatewayDiamond_garbage_collect_bottomUpCheckpoints() public {
        (, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, ) = MerkleTreeHelper.createMerkleProofsForValidators(addrs, weights);

        uint256 index = gatewayDiamond.getter().getCheckpointRetentionHeight();
        require(index == 1, "unexpected height");

        BottomUpCheckpoint memory checkpoint;

        // create a checkpoint
        uint64 n = 10;
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        for (uint64 i = 1; i <= n; i++) {
            checkpoint = BottomUpCheckpoint({
                subnetID: gatewayDiamond.getter().getNetworkName(),
                blockHeight: i * gatewayDiamond.getter().bottomUpCheckPeriod(),
                blockHash: keccak256("block"),
                nextConfigurationNumber: 1,
                slashPreSignatureCount: 0,
                msgs: new IpcEnvelope[](0),
                activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
            });

            gatewayDiamond.checkpointer().createBottomUpCheckpoint(
                checkpoint,
                membershipRoot,
                10,
                ActivityHelper.dummyActivityRollup()
            );
        }
        vm.stopPrank();

        index = gatewayDiamond.getter().getCheckpointRetentionHeight();
        require(index == 1, "retention height is not 1");

        uint256[] memory heights = gatewayDiamond.getter().getIncompleteCheckpointHeights();
        require(heights.length == n, "heights.len is not n");

        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().pruneBottomUpCheckpoints(4);
        vm.stopPrank();

        index = gatewayDiamond.getter().getCheckpointRetentionHeight();
        require(index == 4, "height was not updated");
        heights = gatewayDiamond.getter().getIncompleteCheckpointHeights();
        require(heights.length == n, "index is not the same");
    }

    function testGatewayDiamond_commitCheckpoint_Fails_WrongNumberMessages() public {
        address caller = address(saDiamond);
        vm.startPrank(caller);
        vm.deal(caller, DEFAULT_COLLATERAL_AMOUNT + DEFAULT_CROSS_MSG_FEE);
        registerSubnet(DEFAULT_COLLATERAL_AMOUNT, caller);
        vm.stopPrank();

        uint256 amount = 1;

        (SubnetID memory subnetId, , , , ) = getSubnet(address(caller));
        (bool exist, Subnet memory subnetInfo) = gatewayDiamond.getter().getSubnet(subnetId);
        require(exist, "subnet does not exist");
        require(subnetInfo.circSupply == 0, "unexpected initial circulation supply");

        gatewayDiamond.manager().fund{value: DEFAULT_COLLATERAL_AMOUNT}(
            subnetId,
            FvmAddressHelper.from(address(caller))
        );
        (, subnetInfo) = gatewayDiamond.getter().getSubnet(subnetId);
        require(subnetInfo.circSupply == DEFAULT_COLLATERAL_AMOUNT, "unexpected circulation supply after funding");

        uint64 size = gatewayDiamond.getter().maxMsgsPerBottomUpBatch() + 1;
        IpcEnvelope[] memory msgs = new IpcEnvelope[](size);
        for (uint64 i = 0; i < size; i++) {
            msgs[i] = TestUtils.newXnetCallMsg(
                IPCAddress({subnetId: subnetId, rawAddress: FvmAddressHelper.from(caller)}),
                IPCAddress({
                    subnetId: gatewayDiamond.getter().getNetworkName(),
                    rawAddress: FvmAddressHelper.from(caller)
                }),
                amount,
                i
            );
        }

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: subnetId,
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: msgs,
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        vm.prank(caller);
        vm.expectRevert(MaxMsgsPerBatchExceeded.selector);
        gatewayDiamond.checkpointer().commitCheckpoint(checkpoint);
    }

    function testGatewayDiamond_PopulateBottomUpMsgBatch_Works() public {
        uint256 releaseAmount = 10;
        address from = address(100);

        address[] memory path = new address[](2);
        path[0] = makeAddr("root");
        path[1] = makeAddr("subnet_one");

        GatewayDiamond.ConstructorParams memory constructorParams = GatewayDiamond.ConstructorParams({
            networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
            bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
            majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
            genesisValidators: new Validator[](0),
            activeValidatorsLimit: 100,
            commitSha: DEFAULT_COMMIT_SHA,
            wrappedTokenFactory: address(0),
            ipcBtcToken: address(0x66),
            ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
            ipcBtcName: "IPC-BTC",
            ipcBtcSymbol: "IPC-BTC",
            ipcBtcDecimals: 18,
            ipcBtcRegisterNative: false
        });

        gatewayDiamond = createGatewayDiamond(constructorParams);
        uint256 d = gatewayDiamond.getter().bottomUpCheckPeriod();

        // a few messags in first batch
        uint64 numMsgs = 10;
        vm.roll(1);
        vm.startPrank(from);
        vm.deal(from, numMsgs * (releaseAmount + DEFAULT_CROSS_MSG_FEE));

        for (uint64 i = 0; i < numMsgs; i++) {
            release(releaseAmount);
        }
        require(gatewayDiamond.getter().bottomUpMsgBatch(d).msgs.length == numMsgs, "no messages");

        numMsgs = gatewayDiamond.getter().maxMsgsPerBottomUpBatch() + 10;
        vm.roll(d + 1);
        vm.startPrank(from);
        vm.deal(from, numMsgs * (releaseAmount + DEFAULT_CROSS_MSG_FEE));

        for (uint64 i = 0; i < numMsgs; i++) {
            release(releaseAmount);
        }
        // no batch at d+1: the checkpoint at 2d carries 10 messages, the 10 others wait for the one at 3d
        require(gatewayDiamond.getter().bottomUpMsgBatch(d + 1).blockHeight == 0, "a batch was cut at d+1");
        require(
            gatewayDiamond.getter().bottomUpMsgBatch(2 * d).msgs.length ==
                gatewayDiamond.getter().maxMsgsPerBottomUpBatch(),
            "wrong number of messages in full batch"
        );
        require(
            gatewayDiamond.getter().bottomUpMsgBatch(3 * d).msgs.length == 10,
            "wrong number of messages after full batch"
        );
        // every message is in exactly one batch, in nonce order
        uint64 nonce;
        for (uint256 k = 1; k <= 3; k++) {
            IpcEnvelope[] memory msgs = gatewayDiamond.getter().bottomUpMsgBatch(k * d).msgs;
            for (uint256 j; j < msgs.length; j++) {
                require(msgs[j].nonce == nonce, "a message lost or carried twice");
                nonce++;
            }
        }
        require(nonce == 30, "wrong number of messages in the batches");
    }

    function testGatewayDiamond_setSubnetFinalHeight_keepsMaximum() public {
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().setSubnetFinalHeight(10);
        require(gatewayDiamond.getter().subnetFinalHeight() == 10, "height not set to 10");
        gatewayDiamond.checkpointer().setSubnetFinalHeight(5);
        require(gatewayDiamond.getter().subnetFinalHeight() == 10, "lower height replaced the maximum");
        gatewayDiamond.checkpointer().setSubnetFinalHeight(20);
        require(gatewayDiamond.getter().subnetFinalHeight() == 20, "higher height not set");
        vm.stopPrank();
    }

    function testGatewayDiamond_setSubnetFinalHeight_fails_notSystemActor() public {
        vm.prank(vm.addr(100));
        vm.expectRevert(NotSystemActor.selector);
        gatewayDiamond.checkpointer().setSubnetFinalHeight(10);
    }

    function testGatewayDiamond_createBottomUpCheckpoint_keepsSlashPreSignatureCount() public {
        (, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);
        (bytes32 membershipRoot, ) = MerkleTreeHelper.createMerkleProofsForValidators(addrs, weights);
        uint256 d = gatewayDiamond.getter().bottomUpCheckPeriod();

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: d,
            blockHash: keccak256("block1"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 7,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });

        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2],
            ActivityHelper.dummyActivityRollup()
        );
        vm.stopPrank();

        BottomUpCheckpoint memory recv = gatewayDiamond.getter().bottomUpCheckpoint(d);
        require(recv.slashPreSignatureCount == 7, "slash pre-signature count not kept");
    }

    /// @dev Creates a checkpoint at the first period height, adds the checkpoint signature of every validator,
    /// then the Bitcoin signature of validator i over psbts[i] unless psbts[i] is empty.
    function _signBitcoinCheckpoint(
        uint256[] memory weights,
        bytes[] memory psbts
    ) internal returns (uint256 height) {
        (uint256[] memory privKeys, address[] memory addrs, ) = TestUtils.getFourValidators(vm);

        (bytes32 membershipRoot, bytes32[][] memory membershipProofs) = MerkleTreeHelper
            .createMerkleProofsForValidators(addrs, weights);

        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });
        height = checkpoint.blockHeight;

        uint256 total = weights[0] + weights[1] + weights[2] + weights[3];
        vm.prank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            total,
            ActivityHelper.dummyActivityRollup()
        );

        for (uint64 i = 0; i < 4; i++) {
            _addSignatures(i, privKeys[i], checkpoint, membershipProofs[i], weights[i], psbts[i]);
        }
    }

    function _addSignatures(
        uint64 i,
        uint256 privKey,
        BottomUpCheckpoint memory checkpoint,
        bytes32[] memory proof,
        uint256 weight,
        bytes memory psbt
    ) internal {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(privKey, keccak256(abi.encode(checkpoint)));

        vm.startPrank(vm.addr(privKey));
        gatewayDiamond.checkpointer().addCheckpointSignature(
            checkpoint.blockHeight,
            proof,
            weight,
            abi.encodePacked(r, s, v)
        );
        if (psbt.length != 0) {
            gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
                checkpoint.blockHeight,
                psbt,
                abi.encodePacked("sig", Strings.toString(i)),
                abi.encodePacked("batchtransfer")
            );
        }
        vm.stopPrank();
    }

    function testGatewayDiamond_getBitcoinCheckpointSignatures_mostWeight() public {
        uint256[] memory weights = new uint256[](4);
        weights[0] = 70;
        weights[1] = 10;
        weights[2] = 10;
        weights[3] = 10;

        bytes[] memory psbts = new bytes[](4);
        psbts[0] = "psbtB";
        psbts[1] = "psbtA";
        psbts[2] = "psbtA";
        psbts[3] = "psbtA";

        uint256 height = _signBitcoinCheckpoint(weights, psbts);
        (bytes memory psbt, , address[] memory signatories, ) = gatewayDiamond
            .checkpointer()
            .getBitcoinCheckpointSignatures(height);

        require(keccak256(psbt) == keccak256("psbtB"), "heaviest PSBT not returned");
        require(signatories.length == 1, "incorrect number of signatories");
    }

    function testGatewayDiamond_getBitcoinCheckpointSignatures_mostSignersWhenEqualWeight() public {
        (, , uint256[] memory weights) = TestUtils.getFourValidators(vm);

        bytes[] memory psbts = new bytes[](4);
        psbts[0] = "psbtB";
        psbts[1] = "psbtA";
        psbts[2] = "psbtA";
        psbts[3] = "psbtA";

        uint256 height = _signBitcoinCheckpoint(weights, psbts);
        (bytes memory psbt, , address[] memory signatories, ) = gatewayDiamond
            .checkpointer()
            .getBitcoinCheckpointSignatures(height);

        require(keccak256(psbt) == keccak256("psbtA"), "PSBT with most signers not returned");
        require(signatories.length == 3, "incorrect number of signatories");
    }

    function testGatewayDiamond_hasBitcoinCheckpointSignature() public {
        (, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);

        bytes[] memory psbts = new bytes[](4);
        psbts[0] = "psbtB";
        psbts[1] = "psbtA";
        psbts[2] = "psbtA";
        psbts[3] = "";

        uint256 height = _signBitcoinCheckpoint(weights, psbts);
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();

        for (uint256 i = 0; i < 3; i++) {
            require(checkpointer.hasBitcoinCheckpointSignature(height, addrs[i]), "Bitcoin signature not found");
        }
        require(
            !checkpointer.hasBitcoinCheckpointSignature(height, addrs[3]),
            "a checkpoint signature counted as a Bitcoin signature"
        );
        require(!checkpointer.hasBitcoinCheckpointSignature(height, vm.addr(99)), "a non-signer has a signature");
        require(
            !checkpointer.hasBitcoinCheckpointSignature(height * 2, addrs[0]),
            "a signature found at another height"
        );
    }

    function testGatewayDiamond_addSlashPreSignature_keepsOrderAndRejectsReplay() public {
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();
        bytes32 t1 = bytes32(uint256(1));
        bytes32 t2 = bytes32(uint256(2));

        vm.startPrank(vm.addr(1));
        checkpointer.addSlashPreSignature(t1, 1, _slashSig("sigA"));
        checkpointer.addSlashPreSignature(t1, 2, _slashSig("sigC"));
        vm.stopPrank();
        vm.startPrank(vm.addr(2));
        checkpointer.addSlashPreSignature(t1, 1, _slashSig("sigB"));
        checkpointer.addSlashPreSignature(t2, 1, _slashSig("sigD"));

        // one post per output and poster
        vm.expectRevert(SignatureReplay.selector);
        checkpointer.addSlashPreSignature(t1, 1, _slashSig("sigE"));
        vm.stopPrank();

        uint256 height = 10;
        vm.prank(FilAddress.SYSTEM_ACTOR);
        checkpointer.carrySlashPreSignatures(height, 0);
        SlashPreSignature[] memory posted = checkpointer.getSlashPreSignatures(height);
        require(posted.length == 4, "a replay was stored");
        require(posted[0].txid == t1 && posted[0].vout == 1, "first entry");
        require(keccak256(posted[0].signature) == keccak256(_slashSig("sigA")), "a replay overwrote a signature");
        require(posted[1].txid == t1 && posted[1].vout == 1, "second entry");
        require(posted[2].txid == t1 && posted[2].vout == 2, "third entry");
        require(posted[3].txid == t2 && posted[3].vout == 1, "fourth entry");
        require(keccak256(posted[3].signature) == keccak256(_slashSig("sigD")), "fourth signature");
    }

    function testGatewayDiamond_carrySlashPreSignatures_carriesOutputsPostedSinceLastCarry() public {
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();
        bytes32 txid = bytes32(uint256(7));

        // stored per poster, without power or threshold: the next checkpoint carries the output
        vm.prank(vm.addr(1));
        checkpointer.addSlashPreSignature(txid, 0, _slashSig("s0"));
        vm.prank(vm.addr(2));
        checkpointer.addSlashPreSignature(txid, 0, _slashSig("s1"));
        vm.prank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(10, 0) == 2, "the output was not carried");
        (uint256 carriedHeight, bool posted) = checkpointer.getSlashOutput(txid, 0, vm.addr(2));
        require(carriedHeight == 10 && posted, "wrong output state");
        (, posted) = checkpointer.getSlashOutput(txid, 0, vm.addr(3));
        require(!posted, "a non-poster has posted");
        vm.prank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(20, 0) == 0, "an output was carried twice");

        // a later post makes the output due again, with all its signatures; the earlier checkpoint keeps its two
        vm.prank(vm.addr(3));
        checkpointer.addSlashPreSignature(txid, 0, _slashSig("s2"));
        vm.prank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(30, 0) == 3, "the later post was not carried");
        require(checkpointer.getSlashPreSignatures(10).length == 2, "a carried checkpoint changed");
        require(checkpointer.getSlashPreSignatures(30).length == 3, "wrong carried signatures");
        (carriedHeight, ) = checkpointer.getSlashOutput(txid, 0, vm.addr(1));
        require(carriedHeight == 30, "the last carry is not recorded");

        // a poster posts once per output; another output of the txid is apart
        vm.prank(vm.addr(1));
        vm.expectRevert(SignatureReplay.selector);
        checkpointer.addSlashPreSignature(txid, 0, _slashSig("s3"));
        vm.prank(vm.addr(1));
        checkpointer.addSlashPreSignature(txid, 1, _slashSig("t0"));
        (carriedHeight, posted) = checkpointer.getSlashOutput(txid, 1, vm.addr(1));
        require(carriedHeight == 0 && posted, "outputs mixed");

        vm.expectRevert(InvalidSignatureLength.selector);
        checkpointer.addSlashPreSignature(txid, 2, abi.encodePacked(bytes32(0)));
    }

    function testGatewayDiamond_carrySlashPreSignatures_sizeLimit() public {
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();
        // 17 outputs of 350 slash pre-signatures: 16 fit in 380,000 bytes (16 * (40 + 67 * 350))
        for (uint256 o; o < 17; o++) {
            for (uint16 i; i < 350; i++) {
                vm.prank(address(uint160(i + 1)));
                checkpointer.addSlashPreSignature(bytes32(o + 1), 0, _slashSig("s"));
            }
        }
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(10, 0) == 16 * 350, "wrong count at the limit");
        require(checkpointer.carrySlashPreSignatures(20, 0) == 350, "the output beyond the limit was not carried next");
        require(checkpointer.carrySlashPreSignatures(30, 0) == 0, "an output was carried twice");
        vm.stopPrank();
        SlashPreSignature[] memory carried = checkpointer.getSlashPreSignatures(20);
        require(carried.length == 350 && carried[0].txid == bytes32(uint256(17)), "wrong output carried next");
    }

    function testGatewayDiamond_carrySlashPreSignatures_carriesTheFirstSignaturesOfAnOutputBeyondTheLimit() public {
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();
        // 5,672 slash pre-signatures of one output: 40 + 67 * 5,672 bytes exceed 380,000, 5,671 fit
        uint256 posts = 5_672;
        uint256 fit = 5_671;
        for (uint256 i; i < posts; i++) {
            vm.prank(address(uint160(i + 1)));
            checkpointer.addSlashPreSignature(bytes32(uint256(1)), 0, _slashSig(bytes32(i)));
        }
        vm.prank(vm.addr(1));
        checkpointer.addSlashPreSignature(bytes32(uint256(2)), 0, _slashSig("t"));

        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(10, 0) == fit, "the output beyond the limit was not carried");
        require(checkpointer.carrySlashPreSignatures(20, 0) == 1, "the next output was not carried next");
        require(checkpointer.carrySlashPreSignatures(30, 0) == 0, "the rest of the output was carried again");
        vm.stopPrank();

        SlashPreSignature[] memory carried = checkpointer.getSlashPreSignatures(10);
        require(carried.length == fit, "wrong number carried");
        require(keccak256(carried[0].signature) == keccak256(_slashSig(bytes32(0))), "not the first posted");
        require(
            keccak256(carried[fit - 1].signature) == keccak256(_slashSig(bytes32(fit - 1))),
            "not the first posted"
        );
        carried = checkpointer.getSlashPreSignatures(20);
        require(carried.length == 1 && carried[0].txid == bytes32(uint256(2)), "wrong output carried next");
        (uint256 carriedHeight, ) = checkpointer.getSlashOutput(bytes32(uint256(1)), 0, vm.addr(1));
        require(carriedHeight == 10, "the carry is not recorded");
    }

    function testGatewayDiamond_carrySlashPreSignatures_sizeLimitCountsSupplyAdjustments() public {
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();
        // 16 outputs of 350 slash pre-signatures fit in 380,000 bytes, but not with 100 supply adjustments
        for (uint256 o; o < 16; o++) {
            for (uint16 i; i < 350; i++) {
                vm.prank(address(uint160(i + 1)));
                checkpointer.addSlashPreSignature(bytes32(o + 1), 0, _slashSig("s"));
            }
        }
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(10, 100 * 64) == 15 * 350, "wrong count with the supply adjustments");
        require(checkpointer.carrySlashPreSignatures(20, 0) == 350, "the rest was not carried next");
        vm.stopPrank();
    }

    function testGatewayDiamond_carrySlashPreSignatures_sizeLimitCountsBottomUpMessages() public {
        address[] memory path = new address[](2);
        path[0] = makeAddr("root");
        path[1] = makeAddr("subnet_one");
        gatewayDiamond = createGatewayDiamond(
            GatewayDiamond.ConstructorParams({
                networkName: SubnetID({root: ROOTNET_CHAINID, route: path}),
                bottomUpCheckPeriod: DEFAULT_CHECKPOINT_PERIOD,
                majorityPercentage: DEFAULT_MAJORITY_PERCENTAGE,
                genesisValidators: new Validator[](0),
                activeValidatorsLimit: 100,
                commitSha: DEFAULT_COMMIT_SHA,
                wrappedTokenFactory: address(0),
                ipcBtcToken: address(0x66),
                ipcBtcEmissionSubnet: SubnetID({root: 0, route: new address[](0)}),
                ipcBtcName: "IPC-BTC",
                ipcBtcSymbol: "IPC-BTC",
                ipcBtcDecimals: 18,
                ipcBtcRegisterNative: false
            })
        );
        CheckpointingFacet checkpointer = gatewayDiamond.checkpointer();
        uint256 height = gatewayDiamond.getter().bottomUpCheckPeriod();

        address sender = address(new MockIpcContract());
        IpcEnvelope memory envelope = TestUtils.newXnetCallMsg(
            IPCAddress({subnetId: gatewayDiamond.getter().getNetworkName(), rawAddress: FvmAddressHelper.from(sender)}),
            IPCAddress({
                subnetId: SubnetID({root: ROOTNET_CHAINID, route: new address[](1)}),
                rawAddress: FvmAddressHelper.from(sender)
            }),
            0,
            0
        );
        envelope.to.subnetId.route[0] = path[0];
        envelope.message = abi.encode(CallMsg({method: abi.encodePacked(METHOD_SEND), params: new bytes(5000)}));
        vm.roll(1);
        vm.deal(sender, DEFAULT_CROSS_MSG_FEE);
        vm.prank(sender);
        gatewayDiamond.messenger().sendContractXnetMessage{value: 0}(envelope);
        IpcEnvelope[] memory batched = gatewayDiamond.getter().bottomUpMsgBatch(height).msgs;
        require(batched.length == 1 && batched[0].message.length > 4160, "the long message is not in the batch");

        // 16 outputs of 350 fit in 380,000 bytes alone; the 5,000-byte message leaves room for 15
        for (uint256 o; o < 17; o++) {
            for (uint16 i; i < 350; i++) {
                vm.prank(address(uint160(i + 1)));
                checkpointer.addSlashPreSignature(bytes32(o + 1), 0, _slashSig("s"));
            }
        }
        vm.startPrank(FilAddress.SYSTEM_ACTOR);
        require(checkpointer.carrySlashPreSignatures(height, 0) == 15 * 350, "wrong count with the batch");
        require(checkpointer.carrySlashPreSignatures(2 * height, 0) == 2 * 350, "the rest was not carried next");
        vm.stopPrank();
    }

    function testGatewayDiamond_getBitcoinCheckpointSignatures_returnsSignatoriesWithSignatures() public {
        (uint256[] memory privKeys, address[] memory addrs, uint256[] memory weights) = TestUtils.getFourValidators(vm);
        (bytes32 membershipRoot, ) = MerkleTreeHelper.createMerkleProofsForValidators(addrs, weights);
        BottomUpCheckpoint memory checkpoint = BottomUpCheckpoint({
            subnetID: gatewayDiamond.getter().getNetworkName(),
            blockHeight: gatewayDiamond.getter().bottomUpCheckPeriod(),
            blockHash: keccak256("block"),
            nextConfigurationNumber: 1,
            slashPreSignatureCount: 0,
            msgs: new IpcEnvelope[](0),
            activity: ActivityHelper.newCompressedActivityRollup(1, 3, bytes32(uint256(0)))
        });
        vm.prank(FilAddress.SYSTEM_ACTOR);
        gatewayDiamond.checkpointer().createBottomUpCheckpoint(
            checkpoint,
            membershipRoot,
            weights[0] + weights[1] + weights[2] + weights[3],
            ActivityHelper.dummyActivityRollup()
        );

        for (uint256 i; i < 3; i++) {
            vm.prank(vm.addr(privKeys[i]));
            gatewayDiamond.checkpointer().addBitcoinCheckpointSignature(
                checkpoint.blockHeight,
                abi.encodePacked("psbt"),
                abi.encodePacked("sig", i),
                abi.encodePacked("batchtransfer1")
            );
        }

        (, , address[] memory signatories, bytes[] memory signatures) = gatewayDiamond
            .checkpointer()
            .getBitcoinCheckpointSignatures(checkpoint.blockHeight);
        require(signatories.length == 3 && signatures.length == 3, "wrong number of signatories");
        for (uint256 i; i < 3; i++) {
            require(signatories[i] == vm.addr(privKeys[i]), "wrong signatory");
            require(keccak256(signatures[i]) == keccak256(abi.encodePacked("sig", i)), "wrong signature");
        }
    }

    function _slashSig(bytes32 tag) internal pure returns (bytes memory) {
        return abi.encodePacked(tag, bytes32(0));
    }

    function newListOfMessages(uint64 size) internal view returns (IpcEnvelope[] memory msgs) {
        msgs = new IpcEnvelope[](size);
        for (uint64 i = 0; i < size; i++) {
            msgs[i] = TestUtils.newXnetCallMsg(
                IPCAddress({
                    subnetId: gatewayDiamond.getter().getNetworkName(),
                    rawAddress: FvmAddressHelper.from(address(this))
                }),
                IPCAddress({
                    subnetId: gatewayDiamond.getter().getNetworkName(),
                    rawAddress: FvmAddressHelper.from(address(this))
                }),
                0,
                i
                // method: this.callback.selector,
            );
        }
    }

    function callback() public view {}
}
