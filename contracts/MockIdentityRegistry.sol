// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.20;

/// @notice Test double for the indexer: the exact event surface of
///         IdentityRegistry, each behind a function that just emits it or,
///         for HandlePublished, among a bind's events in `bind`'s order. The
///         integration test deploys this on anvil and replays scenarios, so
///         the indexer decodes real ABI-encoded logs rather than values a
///         Rust test constructed for itself.
///
///         Event signatures are copied verbatim from
///         libid-contracts/solidity/contracts/identity/IdentityRegistry.sol;
///         the hashed-identities suite checks every topic against the
///         crate's bindings.
contract MockIdentityRegistry {
    /// Keyed by nodes, as IdentityRegistry answers; the indexer checks it.
    function nodeKeyed() external pure returns (bool) {
        return true;
    }

    event IdentityBound(
        address indexed holder,
        bytes32 indexed idNode,
        bytes32 indexed handleNode,
        bytes32 platformId,
        uint64 observedAt,
        uint16 ceremonyVersion
    );
    event CeremonyBound(
        bytes32 indexed authorizationDigest, address indexed holder, bytes32 indexed platformId, bytes clientIdentifier
    );
    event BindFeePaid(bytes32 indexed authorizationDigest, address indexed receiver, uint256 amount);
    event HandleRetired(bytes32 indexed platformId, bytes32 indexed handleNode, address indexed holder);
    event ProofVerifierConfigured(address verifier);
    event HandleRevealed(bytes32 indexed platformId, bytes32 indexed handleNode, string handle);
    event HandlePublished(address indexed holder, bytes32 indexed platformId, bytes32 indexed handleNode);
    event HandleUnpublished(address indexed holder);

    /// @notice One `bind`, and what it finds before it writes.
    struct Bind {
        address holder;
        bytes32 idNode;
        bytes32 handleNode;
        bytes32 platformId;
        uint64 observedAt;
        uint16 ceremonyVersion;
        /// The address whose name for `handleNode` the bind withdraws; zero for none.
        address takenFrom;
        /// The handle node the identity held before; zero for none.
        bytes32 retiredNode;
        /// Who held `retiredNode`.
        address retiredHolder;
        /// Whether `retiredHolder`'s name was `retiredNode`.
        bool retiredNamed;
        /// The disclosed handle; empty for a private bind.
        string handle;
        /// Whether the disclosed handle becomes the holder's name.
        bool primary;
        bytes32 authorizationDigest;
    }

    function emitIdentityBound(
        address holder,
        bytes32 idNode,
        bytes32 handleNode,
        bytes32 platformId,
        uint64 observedAt,
        uint16 ceremonyVersion
    ) external {
        emit IdentityBound(holder, idNode, handleNode, platformId, observedAt, ceremonyVersion);
    }

    /// @notice `bind`'s events, in its order: `_write`'s withdrawal of the
    ///         taken handle's name, the previous handle's retirement and its
    ///         name's withdrawal, the binding; then the reveal, the name when
    ///         `primary`, and the ceremony.
    function emitBind(Bind calldata b) external {
        if (b.takenFrom != address(0)) emit HandleUnpublished(b.takenFrom);
        if (b.retiredNode != bytes32(0)) {
            emit HandleRetired(b.platformId, b.retiredNode, b.retiredHolder);
            if (b.retiredNamed) emit HandleUnpublished(b.retiredHolder);
        }
        emit IdentityBound(b.holder, b.idNode, b.handleNode, b.platformId, b.observedAt, b.ceremonyVersion);
        if (bytes(b.handle).length != 0) {
            emit HandleRevealed(b.platformId, b.handleNode, b.handle);
            if (b.primary) emit HandlePublished(b.holder, b.platformId, b.handleNode);
        }
        emit CeremonyBound(b.authorizationDigest, b.holder, b.platformId, "");
    }

    function emitCeremonyBound(
        bytes32 authorizationDigest,
        address holder,
        bytes32 platformId,
        bytes calldata clientIdentifier
    ) external {
        emit CeremonyBound(authorizationDigest, holder, platformId, clientIdentifier);
    }

    function emitBindFeePaid(bytes32 authorizationDigest, address receiver, uint256 amount) external {
        emit BindFeePaid(authorizationDigest, receiver, amount);
    }

    function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address holder) external {
        emit HandleRetired(platformId, handleNode, holder);
    }

    function emitProofVerifierConfigured(address verifier) external {
        emit ProofVerifierConfigured(verifier);
    }

    /// @notice `reveal`'s event, or `publish`'s first.
    function emitHandleRevealed(bytes32 platformId, bytes32 handleNode, string calldata handle) external {
        emit HandleRevealed(platformId, handleNode, handle);
    }

    function emitHandleUnpublished(address holder) external {
        emit HandleUnpublished(holder);
    }
}
