I mean, we could build a k8s distro ontop of aster

⏺ Oh. You mean replace the control plane, not just deploy on K8s.

  etcd is a key-value store with consensus. iroh-docs is a replicated key-value store with CRDT sync. The kubelet needs to talk to the API
  server over a secure channel. QUIC + ed25519 endpoint identity is a secure channel with mutual authentication built in. The API server
  needs to distribute state to kubelets. Gossip does that.

  The pieces map:

  ┌────────────────────────────────────┬──────────────────────────────────────────────────┐
  │           K8s component            │                 Aster equivalent                 │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ etcd (consensus store)             │ iroh-docs (CRDT replicated state)                │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ API server (authenticated RPC)     │ aster-rpc service with trust model               │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ TLS + CA (mTLS between components) │ QUIC + ed25519 (no CA needed)                    │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ kube-proxy / service mesh          │ Direct P2P, NAT traversal built in               │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ DNS-based service discovery        │ @aster handle-based resolution                   │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ RBAC                               │ Three-gate trust model + capability requirements │
  ├────────────────────────────────────┼──────────────────────────────────────────────────┤
  │ Container registry                 │ iroh-blobs (content-addressed)                   │
  └────────────────────────────────────┴──────────────────────────────────────────────────┘

  The interesting part: you wouldn't need the entire networking stack that K8s struggles with. No kube-proxy, no iptables rules, no CNI
  plugins, no overlay networks. Nodes talk to each other directly over QUIC with hole-punching. Services register via aster-rpc, not DNS.
  mTLS is replaced by something strictly stronger (endpoint identity is the key, not a certificate issued by a CA).

  That's a real product. A P2P-native container orchestrator with minimal infrastructure requirements. No etcd quorum to babysit. No CA to
  rotate. No overlay network to debug.
