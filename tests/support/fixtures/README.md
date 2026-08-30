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

## `erc2771_forwarder.bin` / `erc2771_target.bin`

Creation bytecode of the **real** OpenZeppelin `ERC2771Forwarder` (v5.1.0) and a trivial
ERC-2771 target, for the gasless meta-tx confirm-parity suite (`tests/gasless.rs`). Using the
genuine OZ forwarder is the point: only its on-chain `ECDSA.recover` can prove walletkit signs
the `ForwardRequest` in the exact 65-byte `r‖s‖v` form (v ∈ {27, 28}) the forwarder expects.

- `erc2771_forwarder.bin` — a no-arg subclass `Forwarder is ERC2771Forwarder` constructed with
  name `"ERC2771Forwarder"` (EIP-712 version `"1"`), matching `ForwarderDomain::default()`. The
  no-arg constructor keeps the committed creation code self-contained (no appended args);
  `Localnet::deploy_erc2771_forwarder` deploys it verbatim.
- `erc2771_target.bin` — `RecordingTarget is ERC2771Context`: `poke()` records the ERC-2771
  `_msgSender()` and bumps `pokes`; `boom()` reverts. Its constructor takes the forwarder
  address, so `Localnet::deploy_erc2771_target` appends the 32-byte-padded forwarder to the
  creation code.

Regenerate with `solc 0.8.30` via a scratch Foundry project:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;
import {ERC2771Forwarder} from "openzeppelin-contracts/contracts/metatx/ERC2771Forwarder.sol";
import {ERC2771Context} from "openzeppelin-contracts/contracts/metatx/ERC2771Context.sol";

contract Forwarder is ERC2771Forwarder {
    constructor() ERC2771Forwarder("ERC2771Forwarder") {}
}
contract RecordingTarget is ERC2771Context {
    address public lastSender;
    uint256 public pokes;
    constructor(address forwarder) ERC2771Context(forwarder) {}
    function poke() external { lastSender = _msgSender(); pokes += 1; }
    function boom() external { lastSender = _msgSender(); revert("boom"); }
}
```

`forge install OpenZeppelin/openzeppelin-contracts@v5.1.0` then
`forge inspect src/Fixtures.sol:Forwarder bytecode` (and `:RecordingTarget`) — take each `0x…`
output as the `.bin`.
