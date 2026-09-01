// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC165} from "@openzeppelin/contracts/utils/introspection/IERC165.sol";

import {IExtendedResolver} from "./IExtendedResolver.sol";

/// @title HandleResolver - the ENS resolver for `handles.link`.
///
/// @notice Answers every name under one domain by asking a gateway, and holds
///         no names of its own.
///
/// @dev **This contract stores nothing about anybody.** `IdentityNames` already
///      answers the question an ENS resolver asks, so a name here needs no
///      registry entry, no NFT and no record — it is a view onto state that
///      exists on another chain. What lives here is the arrangement: where to
///      ask, and whose answer to believe.
///
///      **One resolver covers every depth.** ENSIP-10 has the client walk up
///      from the full name until it finds a resolver, and hand that resolver
///      the ORIGINAL name. Set this once on `handles.link` and
///      `alice.x.handles.link` and `alice.x.base.handles.link` both arrive
///      here, with no name ever created between them.
///
///      **Resolution is three steps, and the address comes from the third.**
///      `resolve` reverts `OffchainLookup` carrying the endpoints; the client
///      fetches a signed blob; the client then calls `resolveWithProof` back
///      here, which checks the signature and returns the record. Both on-chain
///      halves are `view` — nothing is sent and no gas is spent.
///
///      **The gateway is not trusted.** It signs, and this verifies against a
///      pinned signer set. Seizing an endpoint buys nothing on its own: the
///      callback rejects an answer nobody in the set signed, so a stolen URL
///      can only refuse to answer.
///
///      **The owner key is the whole authority, and that is worth saying
///      plainly.** `setSigner` and `setUrls` are both `onlyOwner`, so whoever
///      holds it can trust its own signer and point at its own endpoint in two
///      transactions, with no gateway key involved. This contract does not
///      split that and cannot; what limits the damage is where the owner key
///      lives, and that every change is a transaction visible in the registry.
///      Holding the owner key, the deployer key and the ENS name's owner as ONE
///      identity — which the current setup does — means one compromise reaches
///      all three. See `libid/design/ens-integration.md`.
///
///      The signature covers the resolver, an expiry, the request and the
///      result, so an answer cannot be replayed against another resolver,
///      reused for a different query, or served after its deadline.
///
///      What it does NOT cover is the chain id — matching
///      `ensdomains/offchain-resolver`'s `SignatureVerifier`, which the test
///      suite pins byte for byte. So a deployment discipline stands in for it:
///      an answer signed for a resolver at address A on one chain verifies
///      unchanged at address A on another. Never share a signing key between
///      deployments that could land on the same address — which deterministic
///      deployment makes the usual outcome rather than the exotic one.
///
///      **Not upgradeable, on purpose.** Replacing it is `setResolver` on the
///      ENS name, which is one owner transaction and visible in the registry.
///      A proxy would add a second way to change behaviour and buy nothing.
contract HandleResolver is IExtendedResolver, IERC165, Ownable2Step {
    /// @notice Endpoints the client may ask, tried in order.
    /// @dev A list, so several deployments give redundancy. Each carries the
    ///      ERC-3668 `{sender}` and `{data}` placeholders.
    string[] public urls;

    /// @notice Whose signature this contract believes.
    mapping(address => bool) public signers;

    event UrlsChanged(string[] urls);
    event SignerChanged(address indexed signer, bool trusted);

    /// The answer carries a deadline that has passed.
    error SignatureExpired(uint64 expires, uint256 nowTs);
    /// Nobody in the signer set signed this answer.
    error UntrustedSigner(address recovered);
    /// A resolver with no endpoint can answer nothing.
    error NoUrls();
    /// The answer claims a lifetime longer than any answer may have.
    error DeadlineTooFar(uint64 expires, uint256 ceiling);

    /// The longest an answer may claim to be good for.
    ///
    /// `expires` is chosen entirely by the gateway, so without a ceiling a
    /// compromised or careless signing key mints answers valid until the heat
    /// death: capture one blob, replay it after the binding moves, and the
    /// chain returns the wallet the name USED to hold. An hour is already
    /// generous for a value a payment is routed by.
    ///
    /// A constant rather than owner state, on purpose: it bounds what the
    /// owner's own signer set can do, and a bound the owner can widen is not
    /// one.
    uint256 public constant MAX_LIFETIME = 1 hours;

    constructor(address owner_, string[] memory urls_, address[] memory signers_) Ownable(owner_) {
        _setUrls(urls_);
        for (uint256 i = 0; i < signers_.length; i++) {
            signers[signers_[i]] = true;
            emit SignerChanged(signers_[i], true);
        }
    }

    // ─── Resolution ─────────────────────────────────────────────────

    /// @inheritdoc IExtendedResolver
    ///
    /// @dev Always reverts. That is the protocol, not a failure: ERC-3668 uses
    ///      the revert to carry the endpoints, so the contract tells the client
    ///      where to look instead of anything being registered with a wallet.
    function resolve(bytes calldata name, bytes calldata data) external view returns (bytes memory) {
        if (urls.length == 0) revert NoUrls();

        // What the gateway is asked, and what the signature will cover. The
        // name AND the original call, so the gateway knows which record of
        // which name was wanted.
        bytes memory callData = abi.encodeWithSelector(IExtendedResolver.resolve.selector, name, data);

        revert OffchainLookup(address(this), urls, callData, this.resolveWithProof.selector, callData);
    }

    /// @notice The second on-chain step: check the gateway's answer and return
    ///         the record.
    ///
    /// @dev `view`, so a client runs it with `eth_call` and pays nothing.
    ///
    /// @param response The gateway's blob: `(result, expires, signature)`.
    /// @param extraData What `resolve` asked for, handed back unchanged.
    function resolveWithProof(bytes calldata response, bytes calldata extraData) external view returns (bytes memory) {
        (bytes memory result, uint64 expires, bytes memory signature) = abi.decode(response, (bytes, uint64, bytes));

        if (expires < block.timestamp) revert SignatureExpired(expires, block.timestamp);
        // Both ends, not just the near one: an unbounded deadline is a
        // replayable answer.
        if (expires > block.timestamp + MAX_LIFETIME) {
            revert DeadlineTooFar(expires, block.timestamp + MAX_LIFETIME);
        }

        address signer = ECDSA.recover(makeSignatureHash(address(this), expires, extraData, result), signature);
        if (!signers[signer]) revert UntrustedSigner(signer);

        return result;
    }

    /// @notice What a gateway must sign.
    ///
    /// @dev Public so a gateway in any language can reproduce it without
    ///      guessing, and a test can check the two agree.
    ///
    ///      `target` binds the answer to THIS resolver, `expires` to a window,
    ///      `request` to the exact query, `result` to the exact answer. Drop
    ///      any one and the answer becomes replayable somewhere it was never
    ///      meant for.
    ///
    ///      The `0x1900` prefix is EIP-191 version `0x00` — a signature over
    ///      data with an intended validator, which is precisely this contract.
    function makeSignatureHash(address target, uint64 expires, bytes memory request, bytes memory result)
        public
        pure
        returns (bytes32)
    {
        return keccak256(abi.encodePacked(hex"1900", target, expires, keccak256(request), keccak256(result)));
    }

    // ─── Configuration ──────────────────────────────────────────────

    function setUrls(string[] calldata urls_) external onlyOwner {
        _setUrls(urls_);
    }

    /// @dev Add before removing when rotating, or answers signed by the old key
    ///      and still in flight start failing.
    function setSigner(address signer, bool trusted) external onlyOwner {
        signers[signer] = trusted;
        emit SignerChanged(signer, trusted);
    }

    function urlCount() external view returns (uint256) {
        return urls.length;
    }

    function _setUrls(string[] memory urls_) private {
        delete urls;
        for (uint256 i = 0; i < urls_.length; i++) {
            urls.push(urls_[i]);
        }
        emit UrlsChanged(urls_);
    }

    // ─── Introspection ──────────────────────────────────────────────

    /// @dev A client checks `0x9061b923` before it will hand this contract a
    ///      name it did not find an exact entry for. Answer no, and wildcard
    ///      resolution never reaches here.
    function supportsInterface(bytes4 interfaceId) external pure returns (bool) {
        return interfaceId == type(IExtendedResolver).interfaceId || interfaceId == type(IERC165).interfaceId;
    }

    /// @dev Renouncing would freeze the endpoints and the signer set, and the
    ///      only repair left would be `setResolver` on the ENS name.
    function renounceOwnership() public pure override {
        revert("renounce disabled");
    }
}

/// @notice ERC-3668. Declared at file scope so the client-facing error is not
///         nested inside the contract's namespace.
error OffchainLookup(address sender, string[] urls, bytes callData, bytes4 callbackFunction, bytes extraData);
