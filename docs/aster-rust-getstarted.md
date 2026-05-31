Use Aster’s new Gate0 instead of maintaining a separate static-only allowlist. The model should be:

1. Start the daemon-scoped node with hooks and the admission ALPN registered:

Node::start_with_alpns( cfg.hooks(true), vec!\[aster::alpns::PRODUCER_ADMISSION.to_vec()\], )

Built-in docs/blobs/gossip ALPNs are still registered automatically by Aster.

2. Run a Gate-0 hook loop:

let gate = aster::Gate0::new();

while let Some(req) = admission.next_handshake().await { if gate.should_allow(&req.peer, &req.alpn) { req.accept(); } else { req.reject(403, b"not admitted".to_vec()); } }

This means:

- unknown peer on aster.producer_admission = allowed to present a credential;
- unknown peer on docs/blobs/gossip/manifest sync = rejected;
- admitted peer on normal ALPNs = accepted.

3. Run a custom-ALPN admission accept loop:

- node.accept().await
- if alpn == aster::alpns::PRODUCER_ADMISSION
- accept a bi-stream
- read a bounded request, e.g. 64 KiB max
- parse the peer’s Chain
- verify with:

let expected = PublicKey::from_hex(conn.peer().as_str())?; verify_chain(&chain, &\[bundle.root_pub\], &expected)?; gate.admit(conn.peer());

Then respond ok / denied. Do not leak detailed reject reasons on the wire; log locally.

4. Use a small portal-sync-owned wire envelope, not raw bytes long-term. Suggested v1:

portalsync-admission/1 chain=aster.attestation.chain.v1:&lt;...&gt;

Response:

portalsync-admission-response/1 accepted=true

The Aster test uses raw chain bytes, but portal-sync should version its own payload now.

Important gotcha for Slice 4: aster::Node is cheap Clone, so no Arc&lt;Node&gt; is required just for sharing. But Node::shutdown(self) closes the underlying shared node. Therefore BlobStore::open_over(node, tree_id) must not let each per-Tree BlobStore::shutdown() shut down the daemon node. Use an ownership flag or separate constructors: owned-node stores shut down their node; shared-node stores only release the handle.

One remaining Aster caveat: Admission still has separate next_handshake() and next_connect() streams. If hooks are enabled, unhandled outbound connect hooks may wait for hook_timeout_ms before default accept. This should not block Slice 4, but portal-sync should either set an acceptable timeout or track an Aster follow-up for a unified/ split admission polling API.

-----

 a responder must conn.closed().await after finish() — dropping the connection immediately truncates the peer's read (closed by peer:
  0). I exposed Connection::closed() for exactly this and documented it.