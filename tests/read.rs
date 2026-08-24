//! `ReadClient` integration, two tiers. The hermetic embedded-anvil test always runs (when
//! anvil is present) and guards the `aggregate3` decode + per-token failure isolation on a
//! deterministic mock. The pinned-fork test opts in via `WALLETKIT_FORK_RPC` (an archive
//! mainnet RPC) and asserts **every** method against exact real values captured at a fixed
//! block — forking mainnet there pins the state, so nothing rots. Reads don't touch the
//! store, so no backend matrix.

mod support;

use alloy_primitives::{U256, address};
use alloy_sol_types::SolCall;
use std::str::FromStr;
use support::{Backend, ForkedNet, Localnet, MockErc20};
use walletkit::adapters::{RpcReadClient, Transport};
use walletkit::core::deps::ReadClient;

#[tokio::test]
async fn reads_erc20_and_isolates_a_reverting_token() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 0, 1).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let read = RpcReadClient::new(Transport::url(net.endpoint()).unwrap().provider());
    let deployer = net.account_at(0);
    net.deploy_multicall3().await; // anvil doesn't predeploy it; balances/metadata batch through it
    let token = net.deploy_mock_erc20(0).await;

    // chain / code / native.
    assert_eq!(read.chain_id().await.unwrap(), net.chain_id());
    assert!(read.is_contract(token).await.unwrap());
    assert!(!read.is_contract(net.account_at(2)).await.unwrap());
    assert!(read.native_balance(deployer).await.unwrap() > U256::ZERO);

    // ERC-20 metadata + balance (constructor mints 1_000_000e18 to the deployer).
    let md = read.erc20_metadata(token).await.unwrap();
    assert_eq!(
        (md.name.as_str(), md.symbol.as_str(), md.decimals),
        ("Mock", "MOCK", 18)
    );
    let supply = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18));
    assert_eq!(read.erc20_balance(token, deployer).await.unwrap(), supply);

    // Allowance after an on-chain approve.
    let spender = net.account_at(1);
    let approve = MockErc20::approveCall {
        spender,
        amount: U256::from(42u64),
    };
    net.send_tx(0, token, approve.abi_encode().into()).await;
    assert_eq!(
        read.erc20_allowance(token, deployer, spender)
            .await
            .unwrap(),
        U256::from(42u64)
    );

    // balances(): native folds in; an EOA target has no balanceOf → isolated Err, the real
    // token still returns Ok, proving one bad entry can't fail the whole scan.
    let not_a_token = net.account_at(3);
    let overview = read
        .balances(deployer, &[token, not_a_token])
        .await
        .unwrap();
    assert!(overview.native > U256::ZERO);
    assert_eq!(overview.tokens.len(), 2);
    assert_eq!(overview.tokens[0].token, token);
    assert_eq!(
        overview.tokens[0].balance.as_ref().copied().unwrap(),
        supply
    );
    assert_eq!(overview.tokens[1].token, not_a_token);
    assert!(overview.tokens[1].balance.is_err());
}

/// The pinned block. Every value below was read from mainnet at exactly this height, so the
/// assertions are immutable. Re-pin (and re-capture) only if the archive drops this block.
const FORK_BLOCK: u64 = 21_000_000;

/// Every `ReadClient` method asserted against **exact real mainnet values** at [`FORK_BLOCK`]
/// — forking mainnet there pins the state, so nothing rots. Opt in by setting
/// `WALLETKIT_FORK_RPC` to an archive RPC (a free option is `https://eth.drpc.org`, or a keyed
/// Alchemy/Infura/reth endpoint); skips cleanly when unset or the fork is unavailable.
#[tokio::test]
async fn reads_exact_real_values_at_pinned_block() {
    let Ok(rpc) = std::env::var("WALLETKIT_FORK_RPC") else {
        eprintln!("skipping: set WALLETKIT_FORK_RPC to an archive mainnet RPC to run");
        return;
    };
    let Some(net) = ForkedNet::pin(&rpc, FORK_BLOCK).await else {
        eprintln!("skipping: anvil or archive fork at block {FORK_BLOCK} unavailable");
        return;
    };
    let read = RpcReadClient::new(Transport::url(net.endpoint()).unwrap().provider());

    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // string metadata, 6 dp
    let mkr = address!("0x9f8F72aA9304c8B593d555F12eF6589cC3A579A2"); // bytes32 metadata
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // holds huge ETH
    let bayc = address!("0xBC4CA0EdA7647A8aB7C2061c2E118A18a936f13D"); // ERC-721
    let os1155 = address!("0x495f947276749Ce646f68AC8c248420045cb7b5e"); // ERC-1155 (OpenSea)
    let burn = address!("0x000000000000000000000000000000000000dEaD"); // an EOA (no code)

    // Exact on-chain values captured at block 21_000_000.
    let weth_eth = U256::from_str("2958184688344113069878460").unwrap(); // WETH's ETH
    let usdc_of_weth = U256::from(6_124_268_984u64); // USDC.balanceOf(WETH)
    let weth_of_weth = U256::from_str("753359872738444976254").unwrap(); // WETH.balanceOf(WETH)
    let bayc1_owner = address!("0x46EFbAedc92067E6d60E84ED6395099723252496"); // owner of BAYC #1

    // chain_id / code / is_contract.
    assert_eq!(read.chain_id().await.unwrap(), 1);
    assert!(!read.code(usdc).await.unwrap().is_empty());
    assert!(read.code(burn).await.unwrap().is_empty());
    assert!(read.is_contract(usdc).await.unwrap());
    assert!(!read.is_contract(burn).await.unwrap());

    // native_balance (exact).
    assert_eq!(read.native_balance(weth).await.unwrap(), weth_eth);

    // erc20_metadata — `string` (USDC/WETH) and the `bytes32` fallback (MKR).
    let m = read.erc20_metadata(usdc).await.unwrap();
    assert_eq!((m.symbol.as_str(), m.decimals), ("USDC", 6));
    let m = read.erc20_metadata(weth).await.unwrap();
    assert_eq!(
        (m.name.as_str(), m.symbol.as_str(), m.decimals),
        ("Wrapped Ether", "WETH", 18)
    );
    let m = read.erc20_metadata(mkr).await.unwrap();
    assert_eq!(
        (m.name.as_str(), m.symbol.as_str(), m.decimals),
        ("Maker", "MKR", 18)
    );

    // erc20_balance / erc20_allowance (exact).
    assert_eq!(read.erc20_balance(usdc, weth).await.unwrap(), usdc_of_weth);
    assert_eq!(
        read.erc20_allowance(usdc, weth, usdc).await.unwrap(),
        U256::ZERO
    );

    // erc721 — exact owner of BAYC #1 and that owner's exact balance.
    assert_eq!(
        read.erc721_owner_of(bayc, U256::from(1u64)).await.unwrap(),
        bayc1_owner
    );
    assert_eq!(
        read.erc721_balance(bayc, bayc1_owner).await.unwrap(),
        U256::from(1u64)
    );

    // erc1155 — the burn address holds none of token #1 (exact 0; proves calldata + decode).
    assert_eq!(
        read.erc1155_balance(os1155, burn, U256::from(1u64))
            .await
            .unwrap(),
        U256::ZERO
    );

    // balances — exact native + per-token, folded and decoded through the real Multicall3.
    let ov = read.balances(weth, &[usdc, mkr, weth]).await.unwrap();
    assert_eq!(ov.native, weth_eth);
    assert_eq!(ov.tokens.len(), 3);
    assert_eq!(
        ov.tokens[0].balance.as_ref().copied().unwrap(),
        usdc_of_weth
    );
    assert_eq!(ov.tokens[1].balance.as_ref().copied().unwrap(), U256::ZERO); // MKR.balanceOf(WETH)
    assert_eq!(
        ov.tokens[2].balance.as_ref().copied().unwrap(),
        weth_of_weth
    );
}
