# Test fixtures

Binary artifacts embedded into the integration harness via `include_str!` (see
`tests/support/mod.rs`). Committed so tests need no toolchain at build time.

## `mock_erc20.bin`

Creation bytecode of a minimal ERC-20 (`name`="Mock", `symbol`="MOCK", 18 decimals; the
constructor mints `1_000_000e18` to the deployer; plus `approve`/`transfer`/`revertWith`).
Used by `Localnet::deploy_mock_erc20` for the hermetic read/preview tests.

Regenerate: compile the source below with `solc 0.8.30 --optimize --bin` and take the
`Binary:` output (prefix with `0x`).

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
contract MockErc20 {
    string public name = "Mock";
    string public symbol = "MOCK";
    uint8 public constant decimals = 18;
    uint256 public constant SUPPLY = 1_000_000 ether;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    constructor() { balanceOf[msg.sender] = SUPPLY; }
    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount; return true;
    }
    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "insufficient");
        balanceOf[msg.sender] -= amount; balanceOf[to] += amount; return true;
    }
    function revertWith() external pure { revert("nope"); }
}
```

## `multicall3.bin`

Deployed (runtime) bytecode of the canonical Multicall3, injected at
`0xcA11bde05977b3631167028862bE2a173976CA11` via `anvil_setCode` in
`Localnet::deploy_multicall3` — anvil does not predeploy it, but real chains have it via
keyless deploy. Verbatim copy of alloy's `MULTICALL3_DEPLOYED_CODE` constant
(`alloy-provider`), which matches the on-chain Multicall3.
